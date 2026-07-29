use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::agent::{Agent, Conversation, RunOptions};
use crate::bench::{BenchOptions, run_manifest};
use crate::cache::ResponseCache;
use crate::chat::{ChatCommand, ChatInput, ChatShell};
use crate::config::{
    CacheMode, Config, ProviderFamily, WireApi, default_config_path, provider_preset,
};
use crate::events::{EventSink, JsonlSink};
use crate::instructions;
use crate::model::create_model;
use crate::session::ChatSession;
use crate::setup::{SetupOptions, run as run_setup};
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
    /// Start the interactive coding-agent session.
    Chat(ChatArgs),
    /// Resume the latest or a selected interactive session.
    Resume(ResumeArgs),
    /// List saved interactive sessions for a workspace.
    Sessions(SessionsArgs),
    /// Run tasks from a JSONL benchmark manifest.
    Bench(BenchArgs),
    /// Print provider presets.
    Providers,
    /// Manage the exact-response cache.
    Cache(CacheArgs),
    /// Write a starter config file.
    Init(InitArgs),
    /// Configure a model provider and store its API key securely.
    Setup(SetupArgs),
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
pub struct ResumeArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Session ID prefix or exact title. Uses the latest session when omitted.
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
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

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Provider preset to configure.
    #[arg(long)]
    pub provider: Option<String>,
    /// Model name or provider model ID.
    #[arg(long)]
    pub model: Option<String>,
    /// Provider base URL.
    #[arg(long)]
    pub base_url: Option<String>,
    /// OpenAI-compatible wire protocol.
    #[arg(long, value_enum)]
    pub wire_api: Option<WireApiArg>,
    /// Update model settings without prompting for an API key.
    #[arg(long)]
    pub skip_key: bool,
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
        Command::Chat(args) => chat(args.common, ChatStart::New).await,
        Command::Resume(args) => chat(args.common, ChatStart::Resume(args.session)).await,
        Command::Sessions(args) => list_sessions(args),
        Command::Bench(args) => bench(args).await,
        Command::Providers => {
            print_providers();
            Ok(())
        }
        Command::Cache(args) => cache(args).await,
        Command::Init(args) => init(args),
        Command::Setup(args) => setup(args),
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
                session_id: None,
            },
        )
        .await?;
    if !result.success {
        std::process::exit(2);
    }
    Ok(())
}

enum ChatStart {
    New,
    Resume(Option<String>),
}

async fn chat(common: CommonArgs, start: ChatStart) -> Result<()> {
    let (config, workspace) = resolve_config(&common)?;
    let instruction_set = instructions::discover(&workspace)?;
    let state_directory = config.agent.trajectory_directory.clone();
    let mut shell = ChatShell::new()?;
    let (mut session, mut conversation) = match start {
        ChatStart::New => (
            ChatSession::create(
                &state_directory,
                &workspace,
                &config.model.provider,
                &config.model.model,
            )?,
            Conversation::default(),
        ),
        ChatStart::Resume(selector) => {
            ChatSession::resume(&state_directory, &workspace, selector.as_deref())?
        }
    };
    let mut agent: Option<Agent> = None;
    shell.welcome(&config, &workspace, session.summary(), &instruction_set);

    loop {
        match shell.read_input()? {
            ChatInput::Exit => break,
            ChatInput::Interrupted => continue,
            ChatInput::Command(ChatCommand::New) => {
                session = ChatSession::create(
                    &state_directory,
                    &workspace,
                    &config.model.provider,
                    &config.model.model,
                )?;
                conversation = Conversation::default();
                shell.clear_screen(&config, &workspace, session.summary(), &instruction_set)?;
                println!("  Started a new session.\n");
            }
            ChatInput::Command(ChatCommand::Config) => shell.show_config_path(),
            ChatInput::Command(ChatCommand::Help) => shell.show_help(),
            ChatInput::Command(ChatCommand::History) => shell.show_history_path(),
            ChatInput::Command(ChatCommand::Rename(title)) => {
                if title.is_empty() {
                    eprintln!("  Usage: /rename <title>");
                } else {
                    session.rename(&title)?;
                    println!("  Session renamed to {title:?}.\n");
                }
            }
            ChatInput::Command(ChatCommand::Resume(selector)) => {
                let resumed =
                    ChatSession::resume(&state_directory, &workspace, selector.as_deref());
                match resumed {
                    Ok((next_session, next_conversation)) => {
                        session = next_session;
                        conversation = next_conversation;
                        shell.clear_screen(
                            &config,
                            &workspace,
                            session.summary(),
                            &instruction_set,
                        )?;
                        println!(
                            "  Resumed session {} with {} messages.\n",
                            session.summary().id,
                            conversation.message_count()
                        );
                    }
                    Err(error) => eprintln!("  {error}\n"),
                }
            }
            ChatInput::Command(ChatCommand::Rules) => shell.show_rules(&instruction_set),
            ChatInput::Command(ChatCommand::Sessions) => {
                let sessions = ChatSession::list(&state_directory, &workspace)?;
                shell.show_sessions(&sessions);
            }
            ChatInput::Command(ChatCommand::Status) => {
                shell.show_status(
                    &config,
                    &workspace,
                    session.summary(),
                    &instruction_set,
                    conversation.message_count(),
                );
            }
            ChatInput::Task(task) => {
                session.set_initial_title(&task)?;
                if agent.is_none() {
                    let api_key = match config.api_key() {
                        Ok(api_key) => api_key,
                        Err(error) => {
                            shell.show_setup_required(&error);
                            continue;
                        }
                    };
                    let cache = ResponseCache::new(config.cache.clone())?;
                    let model = create_model(&config.model, api_key, cache)?;
                    agent = Some(Agent::new(
                        config.clone(),
                        model,
                        Box::new(TerminalUi::chat()),
                        workspace.clone(),
                    ));
                }
                let result = agent
                    .as_mut()
                    .expect("chat agent initialized")
                    .run_in_conversation(
                        &task,
                        RunOptions {
                            session_id: Some(session.summary().id.clone()),
                            ..Default::default()
                        },
                        &mut conversation,
                    )
                    .await;
                session.save(&conversation)?;
                if let Err(error) = result {
                    eprintln!("error: {error:#}");
                }
            }
        }
    }
    Ok(())
}

fn list_sessions(args: SessionsArgs) -> Result<()> {
    let (config, workspace) = resolve_config(&args.common)?;
    let sessions = ChatSession::list(&config.agent.trajectory_directory, &workspace)?;
    if sessions.is_empty() {
        println!("no saved sessions for {}", workspace.display());
        return Ok(());
    }
    println!("{:<10} {:>8}  TITLE", "ID", "MESSAGES");
    for session in sessions {
        println!(
            "{:<10} {:>8}  {}",
            session.id.get(..8).unwrap_or(&session.id),
            session.message_count,
            session.title.as_deref().unwrap_or("untitled")
        );
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

fn setup(args: SetupArgs) -> Result<()> {
    let report = run_setup(SetupOptions {
        provider: args.provider,
        model: args.model,
        base_url: args.base_url,
        wire_api: args.wire_api.map(Into::into),
        skip_key: args.skip_key,
    })?;
    println!("configured {}", report.config_path.display());
    if let Some(path) = report.credentials_path {
        println!("stored credentials securely at {}", path.display());
    }
    println!("run `wecode` inside a repository to start");
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
    fn parses_non_interactive_setup() {
        let cli = Cli::try_parse_from([
            "wecode",
            "setup",
            "--provider",
            "openai",
            "--model",
            "gpt-test",
            "--wire-api",
            "chat-completions",
            "--skip-key",
        ])
        .unwrap();
        let Some(Command::Setup(args)) = cli.command else {
            panic!("expected setup command");
        };
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert_eq!(args.model.as_deref(), Some("gpt-test"));
        assert!(args.skip_key);
    }

    #[test]
    fn parses_session_commands() {
        let resume = Cli::try_parse_from(["wecode", "resume", "abc123"]).unwrap();
        let Some(Command::Resume(args)) = resume.command else {
            panic!("expected resume command");
        };
        assert_eq!(args.session.as_deref(), Some("abc123"));

        let sessions = Cli::try_parse_from(["wecode", "sessions", "-C", "."]).unwrap();
        assert!(matches!(sessions.command, Some(Command::Sessions(_))));
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
