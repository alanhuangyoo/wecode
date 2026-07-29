use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::agent::{Agent, RunOptions};
use crate::bench::{BenchOptions, run_manifest};
use crate::cache::ResponseCache;
use crate::config::{
    CacheMode, Config, ProviderFamily, WireApi, default_config_path, provider_preset,
};
use crate::events::{EventSink, JsonlSink};
use crate::model::create_model;
use crate::ui::TerminalUi;

#[derive(Debug, Parser)]
#[command(name = "wecode", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one autonomous coding task.
    Run(RunArgs),
    /// Start a small line-oriented interactive session.
    Chat(ChatArgs),
    /// Run tasks from a JSONL benchmark manifest.
    Bench(BenchArgs),
    /// Print provider presets.
    Providers,
    /// Manage the exact-response cache.
    Cache(CacheArgs),
    /// Write a starter config file.
    Init(InitArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputMode {
    Human,
    Jsonl,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CacheModeArg {
    Off,
    ReadOnly,
    ReadWrite,
    Refresh,
}

impl From<CacheModeArg> for CacheMode {
    fn from(value: CacheModeArg) -> Self {
        match value {
            CacheModeArg::Off => Self::Off,
            CacheModeArg::ReadOnly => Self::ReadOnly,
            CacheModeArg::ReadWrite => Self::ReadWrite,
            CacheModeArg::Refresh => Self::Refresh,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum WireApiArg {
    ChatCompletions,
    Responses,
}

impl From<WireApiArg> for WireApi {
    fn from(value: WireApiArg) -> Self {
        match value {
            WireApiArg::ChatCompletions => Self::ChatCompletions,
            WireApiArg::Responses => Self::Responses,
        }
    }
}

#[derive(Debug, Args)]
pub struct CommonArgs {
    /// Repository or workspace to operate in.
    #[arg(short = 'C', long, default_value = ".")]
    pub workspace: PathBuf,
    /// Explicit TOML config path.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Provider preset.
    #[arg(long)]
    pub provider: Option<String>,
    /// Model name or provider model ID.
    #[arg(long)]
    pub model: Option<String>,
    /// Override the provider base URL.
    #[arg(long)]
    pub base_url: Option<String>,
    /// Override the API-key environment variable name.
    #[arg(long)]
    pub api_key_env: Option<String>,
    /// Read the API key from a repository-external file.
    #[arg(long)]
    pub api_key_file: Option<PathBuf>,
    /// Override the OpenAI-compatible wire protocol.
    #[arg(long, value_enum)]
    pub wire_api: Option<WireApiArg>,
    /// Disable native function tools and require the JSON text-action fallback.
    #[arg(long)]
    pub text_actions: bool,
    /// Override the request cache mode.
    #[arg(long, value_enum)]
    pub cache_mode: Option<CacheModeArg>,
    /// Maximum model turns.
    #[arg(long)]
    pub max_steps: Option<usize>,
    /// Disable the local dangerous-command denylist. Use only inside a sandbox.
    #[arg(long)]
    pub unsafe_local: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Task text. Reads stdin when omitted.
    pub task: Option<String>,
    /// A command that must pass before the agent may finish.
    #[arg(long)]
    pub verify: Option<String>,
    /// Write the resulting Git patch to this path.
    #[arg(long)]
    pub patch_out: Option<PathBuf>,
    /// Write the final structured result to this path.
    #[arg(long)]
    pub result_out: Option<PathBuf>,
    /// Event output format.
    #[arg(long, value_enum, default_value = "human")]
    pub output: OutputMode,
}

#[derive(Debug, Args)]
pub struct ChatArgs {
    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// JSONL file with id, task, workspace, and optional verify fields.
    pub manifest: PathBuf,
    /// JSONL destination for task results.
    #[arg(long, default_value = "wecode-results.jsonl")]
    pub output: PathBuf,
    /// Continue after a task fails to run.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub keep_going: bool,
}

#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    Stats,
    Prune {
        #[arg(long, default_value_t = 2_048)]
        max_megabytes: u64,
    },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long)]
    pub force: bool,
}

pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command.unwrap_or(Command::Chat(ChatArgs {
        common: CommonArgs {
            workspace: PathBuf::from("."),
            config: None,
            provider: None,
            model: None,
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            wire_api: None,
            text_actions: false,
            cache_mode: None,
            max_steps: None,
            unsafe_local: false,
        },
    })) {
        Command::Run(args) => run_once(args).await,
        Command::Chat(args) => chat(args).await,
        Command::Bench(args) => bench(args).await,
        Command::Providers => {
            print_providers();
            Ok(())
        }
        Command::Cache(args) => cache(args).await,
        Command::Init(args) => init(args),
    }
}

async fn run_once(args: RunArgs) -> Result<()> {
    let task = match args.task {
        Some(task) => task,
        None => {
            if io::stdin().is_terminal() {
                bail!("pass a task argument or pipe task text on stdin");
            }
            let mut task = String::new();
            io::stdin()
                .read_to_string(&mut task)
                .context("failed to read task from stdin")?;
            task
        }
    };
    if task.trim().is_empty() {
        bail!("task cannot be empty");
    }

    let (config, workspace) = resolve_config(&args.common)?;
    let cache = ResponseCache::new(config.cache.clone())?;
    let model = create_model(&config.model, config.api_key()?, cache)?;
    let sink: Box<dyn EventSink> = match args.output {
        OutputMode::Human => Box::new(TerminalUi::new()),
        OutputMode::Jsonl => Box::new(JsonlSink::stdout()),
    };
    let mut agent = Agent::new(config, model, sink, workspace);
    let result = agent
        .run(
            &task,
            RunOptions {
                verify: args.verify,
                patch_out: args.patch_out,
                result_out: args.result_out,
                task_id: None,
            },
        )
        .await?;
    if !result.success {
        std::process::exit(2);
    }
    Ok(())
}

async fn chat(args: ChatArgs) -> Result<()> {
    let (config, workspace) = resolve_config(&args.common)?;
    println!(
        "wecode  {} / {}  ({})",
        config.model.provider,
        config.model.model,
        workspace.display()
    );
    println!("Enter a task, or /quit.");

    loop {
        print!("wecode> ");
        io::stdout().flush()?;
        let mut task = String::new();
        if io::stdin().read_line(&mut task)? == 0 {
            break;
        }
        let task = task.trim();
        if matches!(task, "/quit" | "/exit") {
            break;
        }
        if task.is_empty() {
            continue;
        }

        let cache = ResponseCache::new(config.cache.clone())?;
        let model = create_model(&config.model, config.api_key()?, cache)?;
        let mut agent = Agent::new(
            config.clone(),
            model,
            Box::new(TerminalUi::new()),
            workspace.clone(),
        );
        agent.run(task, RunOptions::default()).await?;
    }
    Ok(())
}

async fn bench(args: BenchArgs) -> Result<()> {
    let (config, workspace) = resolve_config(&args.common)?;
    run_manifest(BenchOptions {
        manifest: args.manifest,
        output: args.output,
        default_workspace: workspace,
        config,
        keep_going: args.keep_going,
    })
    .await
}

async fn cache(args: CacheArgs) -> Result<()> {
    let cache = ResponseCache::new(Default::default())?;
    match args.command {
        CacheCommand::Stats => {
            let stats = cache.stats().await?;
            println!(
                "{} entries, {:.2} MiB at {}",
                stats.entries,
                stats.bytes as f64 / 1_048_576.0,
                cache.directory().display()
            );
        }
        CacheCommand::Prune { max_megabytes } => {
            let stats = cache.prune(max_megabytes).await?;
            println!(
                "cache now has {} entries ({:.2} MiB)",
                stats.entries,
                stats.bytes as f64 / 1_048_576.0
            );
        }
    }
    Ok(())
}

fn init(args: InitArgs) -> Result<()> {
    let path = default_config_path();
    if path.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(&Config::default())?;
    std::fs::write(&path, contents)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn resolve_config(args: &CommonArgs) -> Result<(Config, PathBuf)> {
    let workspace = canonical_workspace(&args.workspace)?;
    let mut config = Config::load(&workspace, args.config.as_deref())?;
    if let Some(provider) = &args.provider {
        config.apply_provider(provider)?;
    }
    if let Some(model) = &args.model {
        config.model.model = model.clone();
    }
    if let Some(base_url) = &args.base_url {
        config.model.base_url = base_url.clone();
    }
    if let Some(api_key_env) = &args.api_key_env {
        config.model.api_key_env = api_key_env.clone();
    }
    if let Some(api_key_file) = &args.api_key_file {
        config.model.api_key_file = Some(api_key_file.clone());
    }
    if let Some(wire_api) = args.wire_api {
        config.model.wire_api = wire_api.into();
    }
    if args.text_actions {
        config.model.native_tools = false;
    }
    if let Some(cache_mode) = args.cache_mode {
        config.cache.mode = cache_mode.into();
    }
    if let Some(max_steps) = args.max_steps {
        config.agent.max_steps = max_steps;
    }
    if args.unsafe_local {
        config.agent.deny_dangerous_commands = false;
    }
    config.validate()?;
    if let Some(api_key_file) = &config.model.api_key_file {
        ensure_secret_file_outside_workspace(api_key_file, &workspace)?;
    }
    Ok((config, workspace))
}

fn ensure_secret_file_outside_workspace(path: &Path, workspace: &Path) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("API key file {} does not exist", path.display()))?;
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("workspace {} does not exist", workspace.display()))?;
    if path.starts_with(&workspace) {
        bail!(
            "API key file {} must be outside the agent workspace {}",
            path.display(),
            workspace.display()
        );
    }
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("workspace {} does not exist", path.display()))
}

fn print_providers() {
    for name in [
        "openai",
        "anthropic",
        "gemini",
        "openrouter",
        "deepseek",
        "groq",
        "xai",
        "mistral",
        "ollama",
        "lmstudio",
        "vllm",
    ] {
        let preset = provider_preset(name, None).expect("built-in preset");
        let family = match preset.family {
            ProviderFamily::OpenAiCompatible => match preset.wire_api {
                WireApi::Responses => "openai-responses",
                WireApi::ChatCompletions => "openai-compatible",
            },
            ProviderFamily::Anthropic => "anthropic-messages",
            ProviderFamily::Gemini => "gemini-generate-content",
        };
        println!(
            "{name:12} {family:24} {:32} {}",
            preset.model, preset.base_url
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_api_override() {
        let cli = Cli::try_parse_from(["wecode", "run", "--wire-api", "responses", "fix the test"])
            .unwrap();

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run command");
        };
        assert!(matches!(args.common.wire_api, Some(WireApiArg::Responses)));
        assert_eq!(args.task.as_deref(), Some("fix the test"));
    }

    #[test]
    fn parses_text_action_fallback() {
        let cli = Cli::try_parse_from(["wecode", "run", "--text-actions", "fix the test"]).unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.common.text_actions);
    }

    #[test]
    fn rejects_api_key_file_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("api-key");
        std::fs::write(&path, "test-key").unwrap();

        let error = ensure_secret_file_outside_workspace(&path, workspace.path()).unwrap_err();
        assert!(error.to_string().contains("must be outside"));
    }

    #[test]
    fn accepts_api_key_file_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let secrets = tempfile::tempdir().unwrap();
        let path = secrets.path().join("api-key");
        std::fs::write(&path, "test-key").unwrap();

        ensure_secret_file_outside_workspace(&path, workspace.path()).unwrap();
    }
}
