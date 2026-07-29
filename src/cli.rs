use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::agent::{Agent, Conversation, RunOptions};
use crate::approval::{ApprovalClient, ApprovalDecision, ApprovalEnvelope};
use crate::bench::{BenchOptions, run_manifest};
use crate::cache::ResponseCache;
use crate::chat::{ChatCommand, ChatInput, ChatShell, ChatView};
use crate::commands::CommandCatalog;
use crate::config::{
    ApprovalPolicy, CacheMode, Config, ProviderFamily, WireApi, default_config_path,
    provider_preset,
};
use crate::control::CancellationToken;
use crate::events::{EventSink, JsonlSink};
use crate::input_queue::{InputQueue, QueuedInput};
use crate::instructions;
use crate::interaction::{
    PlanState, UserInputClient, UserInputEnvelope, UserInputResponse, resolve_answers,
};
use crate::mcp::McpManager;
use crate::model::{ToolProfile, create_model, create_model_with_tools};
use crate::session::ChatSession;
use crate::setup::{SetupOptions, run as run_setup};
use crate::skills::SkillCatalog;
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

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ApprovalPolicyArg {
    Untrusted,
    OnRequest,
    Never,
}

impl From<ApprovalPolicyArg> for ApprovalPolicy {
    fn from(value: ApprovalPolicyArg) -> Self {
        match value {
            ApprovalPolicyArg::Untrusted => Self::UnlessTrusted,
            ApprovalPolicyArg::OnRequest => Self::OnRequest,
            ApprovalPolicyArg::Never => Self::Never,
        }
    }
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
    /// Disable provider streaming for gateways that only support buffered responses.
    #[arg(long)]
    pub no_stream: bool,
    /// Control when shell commands and patches require user approval.
    #[arg(long, value_enum)]
    pub approval_policy: Option<ApprovalPolicyArg>,
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
            no_stream: false,
            approval_policy: None,
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
    let (approval, approval_task) =
        if matches!(args.output, OutputMode::Human) && io::stdin().is_terminal() {
            let (approval, requests) = ApprovalClient::channel();
            (
                Some(approval),
                Some(spawn_terminal_approval_reviewer(requests)),
            )
        } else {
            (None, None)
        };
    let mut agent = Agent::new(config, model, sink, workspace);
    let cancellation = CancellationToken::new();
    let signal_task = spawn_cancellation_signal(cancellation.clone());
    let result = agent
        .run(
            &task,
            RunOptions {
                verify: args.verify,
                patch_out: args.patch_out,
                result_out: args.result_out,
                task_id: None,
                session_id: None,
                cancellation: Some(cancellation),
                input_queue: None,
                approval,
                plan: None,
                user_input: None,
            },
        )
        .await;
    signal_task.abort();
    if let Some(approval_task) = approval_task {
        approval_task.abort();
    }
    let result = result?;
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
    let skills = SkillCatalog::discover(&workspace, &config.skills)?;
    let commands = CommandCatalog::discover(&workspace, &config.commands)?;
    let state_directory = config.agent.trajectory_directory.clone();
    let shell = ChatShell::new()?;
    let view = shell.view();
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
    let input_queue = InputQueue::new();
    let (approval, mut approval_requests) = ApprovalClient::channel();
    let (user_input, mut user_input_requests) = UserInputClient::channel();
    let mut plan = PlanState::restore(conversation.messages());
    view.welcome(
        &config,
        &workspace,
        session.summary(),
        &instruction_set,
        skills.len(),
        commands.len(),
    )?;
    view.sync_plan(&plan.current());
    if !config.mcp.servers.is_empty() {
        view.notice(format!(
            "Connecting {} configured MCP server{}…",
            config.mcp.servers.len(),
            if config.mcp.servers.len() == 1 {
                ""
            } else {
                "s"
            }
        ))?;
    }
    let mcp = McpManager::connect_with_secret_env(
        &config.mcp,
        &workspace,
        Some(&config.model.api_key_env),
    )
    .await;
    for report in mcp
        .reports()
        .into_iter()
        .filter(|report| report.error.is_some())
    {
        view.warning(format!(
            "MCP server {:?} could not connect: {}",
            report.name,
            report.error.as_deref().unwrap_or("unknown error")
        ))?;
    }
    for diagnostic in skills.diagnostics().iter().take(5) {
        view.warning(format!(
            "Skill {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        ))?;
    }
    if skills.diagnostics().len() > 5 {
        view.warning(format!(
            "{} additional skill diagnostics omitted; use /skills to inspect loaded skills.",
            skills.diagnostics().len() - 5
        ))?;
    }
    if !skills.is_empty() {
        view.notice(format!(
            "Discovered {} skill{} · /skills to inspect.",
            skills.len(),
            if skills.len() == 1 { "" } else { "s" }
        ))?;
    }
    for diagnostic in commands.diagnostics().iter().take(5) {
        view.warning(format!(
            "Command {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        ))?;
    }
    if commands.diagnostics().len() > 5 {
        view.warning(format!(
            "{} additional command diagnostics omitted; use /commands to inspect loaded commands.",
            commands.diagnostics().len() - 5
        ))?;
    }
    if !commands.is_empty() {
        view.notice(format!(
            "Discovered {} prompt command{} · /commands to inspect.",
            commands.len(),
            if commands.len() == 1 { "" } else { "s" }
        ))?;
    }
    let mut inputs = shell.into_input_stream();

    loop {
        let Some(input) = inputs.recv().await else {
            break;
        };
        let mut title_override = None;
        let input = match input? {
            ChatInput::Command(ChatCommand::Skill { name, arguments }) => {
                match skills.explicit_request(&name, &arguments) {
                    Ok(request) => {
                        title_override = Some(format!(
                            "/skill:{name}{}",
                            if arguments.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" {}", arguments.trim())
                            }
                        ));
                        ChatInput::Task(request)
                    }
                    Err(error) => {
                        view.warning(error.to_string())?;
                        continue;
                    }
                }
            }
            ChatInput::Command(ChatCommand::Unknown { name, arguments }) => {
                match commands.expand(&name, &arguments) {
                    Ok(request) => {
                        title_override = Some(format!(
                            "/{name}{}",
                            if arguments.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" {}", arguments.trim())
                            }
                        ));
                        ChatInput::Task(request)
                    }
                    Err(_) => {
                        view.warning(format!("Unknown command \"/{name}\". Type /help."))?;
                        continue;
                    }
                }
            }
            input => input,
        };
        match input {
            ChatInput::Exit => break,
            ChatInput::Interrupted => continue,
            ChatInput::FollowUp(task) | ChatInput::Task(task) if task.trim().is_empty() => {
                view.warning("Message cannot be empty.")?;
            }
            ChatInput::Command(ChatCommand::New) => {
                session = ChatSession::create(
                    &state_directory,
                    &workspace,
                    &config.model.provider,
                    &config.model.model,
                )?;
                conversation = Conversation::default();
                plan.clear();
                input_queue.clear();
                view.clear_screen(
                    &config,
                    &workspace,
                    session.summary(),
                    &instruction_set,
                    skills.len(),
                    commands.len(),
                )?;
                view.notice("Started a new session.")?;
            }
            ChatInput::Command(ChatCommand::Cancel) => {
                view.notice("No task is running.")?;
            }
            ChatInput::Command(ChatCommand::Checkpoint(label)) => {
                let checkpoint = session.checkpoint(label.as_deref(), &conversation, false)?;
                view.notice(format!(
                    "Saved checkpoint {} at {} messages: {}.",
                    checkpoint.id, checkpoint.message_count, checkpoint.label
                ))?;
            }
            ChatInput::Command(ChatCommand::Checkpoints) => {
                view.show_checkpoints(session.checkpoints())?;
            }
            ChatInput::Command(ChatCommand::Approve)
            | ChatInput::Command(ChatCommand::ApproveSession)
            | ChatInput::Command(ChatCommand::Deny(_)) => {
                view.notice("No approval request is pending.")?;
            }
            ChatInput::Command(ChatCommand::ClearQueue) => {
                let cleared = input_queue.clear();
                view.notice(format!("Cleared {} queued messages.", cleared.len()))?;
            }
            ChatInput::Command(ChatCommand::Config) => view.show_config_path()?,
            ChatInput::Command(ChatCommand::Commands) => {
                view.show_commands(&commands.commands())?
            }
            ChatInput::Command(ChatCommand::Help) => view.show_help()?,
            ChatInput::Command(ChatCommand::History) => view.show_history_path()?,
            ChatInput::Command(ChatCommand::Mcp) => view.show_mcp(&mcp.reports())?,
            ChatInput::Command(ChatCommand::Skills) => view.show_skills(&skills.skills())?,
            ChatInput::Command(ChatCommand::Plan) => view.show_plan(&plan.current())?,
            ChatInput::Command(ChatCommand::Fork(selector)) => {
                let source_id = session.summary().id.clone();
                match session.fork(&state_directory, &conversation, selector.as_deref()) {
                    Ok((next_session, next_conversation)) => {
                        session = next_session;
                        conversation = next_conversation;
                        plan = PlanState::restore(conversation.messages());
                        input_queue.clear();
                        view.clear_screen(
                            &config,
                            &workspace,
                            session.summary(),
                            &instruction_set,
                            skills.len(),
                            commands.len(),
                        )?;
                        view.sync_plan(&plan.current());
                        view.notice(format!(
                            "Forked session {} from {} with {} messages.",
                            session.summary().id,
                            source_id,
                            conversation.message_count()
                        ))?;
                    }
                    Err(error) => view.warning(error.to_string())?,
                }
            }
            ChatInput::Command(ChatCommand::Queue) => {
                view.show_queue(&input_queue.snapshot())?;
            }
            ChatInput::Command(ChatCommand::Rename(title)) => {
                if title.is_empty() {
                    view.warning("Usage: /rename <title>")?;
                } else {
                    session.rename(&title)?;
                    view.notice(format!("Session renamed to {title:?}."))?;
                }
            }
            ChatInput::Command(ChatCommand::Rewind(selector)) => {
                let source_id = session.summary().id.clone();
                match session.rewind(&state_directory, &conversation, selector.as_deref()) {
                    Ok((next_session, next_conversation)) => {
                        session = next_session;
                        conversation = next_conversation;
                        plan = PlanState::restore(conversation.messages());
                        input_queue.clear();
                        view.clear_screen(
                            &config,
                            &workspace,
                            session.summary(),
                            &instruction_set,
                            skills.len(),
                            commands.len(),
                        )?;
                        view.sync_plan(&plan.current());
                        view.notice(format!(
                            "Rewound safely into session {} from {} at {} messages; the original session is unchanged.",
                            session.summary().id,
                            source_id,
                            conversation.message_count()
                        ))?;
                    }
                    Err(error) => view.warning(error.to_string())?,
                }
            }
            ChatInput::Command(ChatCommand::Resume(selector)) => {
                let resumed =
                    ChatSession::resume(&state_directory, &workspace, selector.as_deref());
                match resumed {
                    Ok((next_session, next_conversation)) => {
                        session = next_session;
                        conversation = next_conversation;
                        plan = PlanState::restore(conversation.messages());
                        input_queue.clear();
                        view.clear_screen(
                            &config,
                            &workspace,
                            session.summary(),
                            &instruction_set,
                            skills.len(),
                            commands.len(),
                        )?;
                        view.sync_plan(&plan.current());
                        view.notice(format!(
                            "Resumed session {} with {} messages.",
                            session.summary().id,
                            conversation.message_count()
                        ))?;
                    }
                    Err(error) => view.warning(error.to_string())?,
                }
            }
            ChatInput::Command(ChatCommand::Rules) => view.show_rules(&instruction_set)?,
            ChatInput::Command(ChatCommand::Sessions) => {
                let sessions = ChatSession::list(&state_directory, &workspace)?;
                view.show_sessions(&sessions)?;
            }
            ChatInput::Command(ChatCommand::Status) => {
                view.show_status(
                    &config,
                    &workspace,
                    session.summary(),
                    &instruction_set,
                    conversation.message_count(),
                    &input_queue.snapshot(),
                )?;
            }
            ChatInput::Command(ChatCommand::Unknown { .. }) => {
                unreachable!("custom commands are expanded before dispatch")
            }
            ChatInput::Command(ChatCommand::Skill { .. }) => {
                unreachable!("skill commands are expanded before dispatch")
            }
            ChatInput::FollowUp(task) | ChatInput::Task(task) => {
                session.set_initial_title(title_override.as_deref().unwrap_or(&task))?;
                session.checkpoint(
                    Some(&automatic_checkpoint_label(&task)),
                    &conversation,
                    true,
                )?;
                if agent.is_none() {
                    let api_key = match config.api_key() {
                        Ok(api_key) => api_key,
                        Err(error) => {
                            view.show_setup_required(&error)?;
                            continue;
                        }
                    };
                    let cache = ResponseCache::new(config.cache.clone())?;
                    let model = create_model_with_tools(
                        &config.model,
                        api_key,
                        cache,
                        ToolProfile::Interactive,
                        mcp.definitions(),
                    )?;
                    agent = Some(Agent::new_with_extensions(
                        config.clone(),
                        model,
                        Box::new(TerminalUi::chat(view.output())),
                        workspace.clone(),
                        ToolProfile::Interactive,
                        mcp.clone(),
                        skills.clone(),
                    ));
                }

                let mut current_task = task;
                let exit_requested = loop {
                    let result = run_active_chat_task(
                        agent.as_mut().expect("chat agent initialized"),
                        &current_task,
                        &mut conversation,
                        &session,
                        &config,
                        &workspace,
                        &instruction_set,
                        &input_queue,
                        &approval,
                        &mut approval_requests,
                        &plan,
                        &user_input,
                        &mut user_input_requests,
                        &mcp,
                        &skills,
                        &commands,
                        &mut inputs,
                        &view,
                    )
                    .await?;
                    session.save(&conversation)?;
                    if result.exit_requested || result.reason != "finished" {
                        break result.exit_requested;
                    }
                    let follow_ups =
                        input_queue.take_follow_ups(config.agent.follow_up_mode.take_all());
                    if follow_ups.is_empty() {
                        break false;
                    }
                    current_task = combined_follow_up(&follow_ups);
                    view.notice(format!(
                        "Starting {} queued follow-up message{}.",
                        follow_ups.len(),
                        if follow_ups.len() == 1 { "" } else { "s" }
                    ))?;
                };
                if exit_requested {
                    break;
                }
            }
        }
    }
    mcp.shutdown().await;
    Ok(())
}

struct ActiveChatResult {
    reason: String,
    exit_requested: bool,
}

#[allow(clippy::too_many_arguments)]
async fn run_active_chat_task(
    agent: &mut Agent,
    task: &str,
    conversation: &mut Conversation,
    session: &ChatSession,
    config: &Config,
    workspace: &Path,
    instruction_set: &instructions::InstructionSet,
    input_queue: &InputQueue,
    approval: &ApprovalClient,
    approval_requests: &mut tokio::sync::mpsc::UnboundedReceiver<ApprovalEnvelope>,
    plan: &PlanState,
    user_input: &UserInputClient,
    user_input_requests: &mut tokio::sync::mpsc::UnboundedReceiver<UserInputEnvelope>,
    mcp: &McpManager,
    skills: &SkillCatalog,
    commands: &CommandCatalog,
    inputs: &mut tokio::sync::mpsc::UnboundedReceiver<Result<ChatInput>>,
    view: &ChatView,
) -> Result<ActiveChatResult> {
    let cancellation = CancellationToken::new();
    let signal_task = spawn_cancellation_signal(cancellation.clone());
    let context_messages = conversation.message_count();
    let run = agent.run_in_conversation(
        task,
        RunOptions {
            session_id: Some(session.summary().id.clone()),
            cancellation: Some(cancellation.clone()),
            input_queue: Some(input_queue.clone()),
            approval: Some(approval.clone()),
            plan: Some(plan.clone()),
            user_input: Some(user_input.clone()),
            ..Default::default()
        },
        conversation,
    );
    tokio::pin!(run);
    let mut exit_requested = false;
    let mut pending_approval: Option<ApprovalEnvelope> = None;
    let mut pending_user_input: Option<UserInputEnvelope> = None;

    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            request = approval_requests.recv(), if pending_approval.is_none() => {
                if let Some(request) = request {
                    view.show_approval(&request.request)?;
                    pending_approval = Some(request);
                }
            }
            request = user_input_requests.recv(), if pending_user_input.is_none() => {
                if let Some(request) = request {
                    view.show_question(&request.request)?;
                    pending_user_input = Some(request);
                }
            }
            input = inputs.recv() => {
                let Some(input) = input else {
                    exit_requested = true;
                    cancellation.cancel();
                    continue;
                };
                let input = input?;
                if pending_approval.is_some() {
                    let decision = match &input {
                        ChatInput::Command(ChatCommand::Approve) => {
                            Some(ApprovalDecision::AllowOnce)
                        }
                        ChatInput::Command(ChatCommand::ApproveSession) => {
                            Some(ApprovalDecision::AllowSession)
                        }
                        ChatInput::Command(ChatCommand::Deny(reason)) => {
                            Some(ApprovalDecision::Deny {
                                reason: if reason.trim().is_empty() {
                                    "denied by user".into()
                                } else {
                                    reason.clone()
                                },
                            })
                        }
                        ChatInput::Task(answer)
                            if matches!(
                                answer.trim().to_ascii_lowercase().as_str(),
                                "y" | "yes" | "allow"
                            ) =>
                        {
                            Some(ApprovalDecision::AllowOnce)
                        }
                        ChatInput::Task(answer)
                            if matches!(
                                answer.trim().to_ascii_lowercase().as_str(),
                                "s" | "session" | "always"
                            ) =>
                        {
                            Some(ApprovalDecision::AllowSession)
                        }
                        ChatInput::Task(answer)
                            if matches!(
                                answer.trim().to_ascii_lowercase().as_str(),
                                "n" | "no" | "deny"
                            ) =>
                        {
                            Some(ApprovalDecision::Deny {
                                reason: "denied by user".into(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(decision) = decision {
                        pending_approval
                            .take()
                            .expect("pending approval exists")
                            .resolve(decision);
                        view.clear_interaction_prompt();
                        continue;
                    }
                    if matches!(
                        input,
                        ChatInput::Task(_)
                            | ChatInput::Command(ChatCommand::Approve)
                            | ChatInput::Command(ChatCommand::ApproveSession)
                            | ChatInput::Command(ChatCommand::Deny(_))
                    ) {
                        view.warning(
                            "Resolve the approval with /approve, /approve-session, or /deny.",
                        )?;
                        continue;
                    }
                }
                if let Some(request) = pending_user_input.as_ref() {
                    match &input {
                        ChatInput::Task(answer) | ChatInput::FollowUp(answer) => {
                            match resolve_answers(&request.request, answer) {
                                Ok(answers) => {
                                    pending_user_input
                                        .take()
                                        .expect("pending user input exists")
                                        .resolve(UserInputResponse::Answered(answers));
                                    view.clear_interaction_prompt();
                                }
                                Err(error) => view.warning(error)?,
                            }
                            continue;
                        }
                        ChatInput::Interrupted | ChatInput::Command(ChatCommand::Cancel) => {
                            pending_user_input
                                .take()
                                .expect("pending user input exists")
                                .resolve(UserInputResponse::Cancelled {
                                    reason: "active run cancelled".into(),
                                });
                            view.clear_interaction_prompt();
                            cancellation.cancel();
                            continue;
                        }
                        ChatInput::Exit => {
                            pending_user_input
                                .take()
                                .expect("pending user input exists")
                                .resolve(UserInputResponse::Cancelled {
                                    reason: "interactive session closed".into(),
                                });
                            view.clear_interaction_prompt();
                            exit_requested = true;
                            cancellation.cancel();
                            continue;
                        }
                        ChatInput::Command(ChatCommand::Help) => {
                            view.show_help()?;
                            continue;
                        }
                        ChatInput::Command(ChatCommand::Plan) => {
                            view.show_plan(&plan.current())?;
                            continue;
                        }
                        ChatInput::Command(ChatCommand::Mcp) => {
                            view.show_mcp(&mcp.reports())?;
                            continue;
                        }
                        ChatInput::Command(ChatCommand::Commands) => {
                            view.show_commands(&commands.commands())?;
                            continue;
                        }
                        ChatInput::Command(ChatCommand::Skills) => {
                            view.show_skills(&skills.skills())?;
                            continue;
                        }
                        _ => {
                            view.warning(
                                "Answer the pending question, or use /cancel to stop the task.",
                            )?;
                            continue;
                        }
                    }
                }
                match input {
                    ChatInput::Task(text) if text.trim().is_empty() => {
                        view.warning("Steering message cannot be empty.")?;
                    }
                    ChatInput::Task(text) => {
                        let queued = input_queue.steer(text);
                        view.show_queued("steer", queued.id, input_queue.snapshot().len())?;
                    }
                    ChatInput::FollowUp(text) if text.trim().is_empty() => {
                        view.warning("Follow-up message cannot be empty.")?;
                    }
                    ChatInput::FollowUp(text) => {
                        let queued = input_queue.follow_up(text);
                        view.show_queued("follow-up", queued.id, input_queue.snapshot().len())?;
                    }
                    ChatInput::Interrupted | ChatInput::Command(ChatCommand::Cancel) => {
                        if let Some(request) = pending_approval.take() {
                            request.resolve(ApprovalDecision::Deny {
                                reason: "active run cancelled".into(),
                            });
                        }
                        cancellation.cancel();
                    }
                    ChatInput::Exit => {
                        exit_requested = true;
                        cancellation.cancel();
                    }
                    ChatInput::Command(ChatCommand::ClearQueue) => {
                        let cleared = input_queue.clear();
                        view.notice(format!("Cleared {} queued messages.", cleared.len()))?;
                    }
                    ChatInput::Command(ChatCommand::Queue) => {
                        view.show_queue(&input_queue.snapshot())?;
                    }
                    ChatInput::Command(ChatCommand::Help) => view.show_help()?,
                    ChatInput::Command(ChatCommand::Approve)
                    | ChatInput::Command(ChatCommand::ApproveSession)
                    | ChatInput::Command(ChatCommand::Deny(_)) => {
                        view.notice("No approval request is pending.")?;
                    }
                    ChatInput::Command(ChatCommand::Status) => {
                        view.show_status(
                            config,
                            workspace,
                            session.summary(),
                            instruction_set,
                            context_messages,
                            &input_queue.snapshot(),
                        )?;
                    }
                    ChatInput::Command(ChatCommand::Config) => view.show_config_path()?,
                    ChatInput::Command(ChatCommand::History) => view.show_history_path()?,
                    ChatInput::Command(ChatCommand::Mcp) => {
                        view.show_mcp(&mcp.reports())?;
                    }
                    ChatInput::Command(ChatCommand::Commands) => {
                        view.show_commands(&commands.commands())?;
                    }
                    ChatInput::Command(ChatCommand::Skills) => {
                        view.show_skills(&skills.skills())?;
                    }
                    ChatInput::Command(ChatCommand::Skill { name, arguments }) => {
                        match skills.explicit_request(&name, &arguments) {
                            Ok(request) => {
                                let queued = input_queue.steer(request);
                                view.show_queued(
                                    "skill steer",
                                    queued.id,
                                    input_queue.snapshot().len(),
                                )?;
                            }
                            Err(error) => view.warning(error.to_string())?,
                        }
                    }
                    ChatInput::Command(ChatCommand::Unknown { name, arguments }) => {
                        match commands.expand(&name, &arguments) {
                            Ok(request) => {
                                let queued = input_queue.steer(request);
                                view.show_queued(
                                    "command steer",
                                    queued.id,
                                    input_queue.snapshot().len(),
                                )?;
                            }
                            Err(_) => {
                                view.warning(format!(
                                    "Unknown command \"/{name}\". Type /help."
                                ))?;
                            }
                        }
                    }
                    ChatInput::Command(ChatCommand::Rules) => view.show_rules(instruction_set)?,
                    ChatInput::Command(ChatCommand::Plan) => view.show_plan(&plan.current())?,
                    ChatInput::Command(ChatCommand::Checkpoints) => {
                        view.show_checkpoints(session.checkpoints())?;
                    }
                    ChatInput::Command(ChatCommand::Sessions) => {
                        view.warning("Use /sessions after the active task finishes.")?;
                    }
                    ChatInput::Command(ChatCommand::New)
                    | ChatInput::Command(ChatCommand::Checkpoint(_))
                    | ChatInput::Command(ChatCommand::Fork(_))
                    | ChatInput::Command(ChatCommand::Rename(_))
                    | ChatInput::Command(ChatCommand::Resume(_))
                    | ChatInput::Command(ChatCommand::Rewind(_)) => {
                        view.warning("That command is available after the active task finishes.")?;
                    }
                }
            }
        }
    };
    signal_task.abort();
    if let Some(request) = pending_approval.take() {
        request.resolve(ApprovalDecision::Deny {
            reason: "active run ended".into(),
        });
    }
    if let Some(request) = pending_user_input.take() {
        request.resolve(UserInputResponse::Cancelled {
            reason: "active run ended".into(),
        });
    }
    while let Ok(request) = approval_requests.try_recv() {
        request.resolve(ApprovalDecision::Deny {
            reason: "active run ended".into(),
        });
    }
    while let Ok(request) = user_input_requests.try_recv() {
        request.resolve(UserInputResponse::Cancelled {
            reason: "active run ended".into(),
        });
    }
    view.clear_interaction_prompt();
    let reason = match result {
        Ok(result) => result.reason,
        Err(error) => {
            view.warning(format!("Agent error: {error:#}"))?;
            "error".into()
        }
    };
    Ok(ActiveChatResult {
        reason,
        exit_requested,
    })
}

fn combined_follow_up(inputs: &[QueuedInput]) -> String {
    if inputs.len() == 1 {
        return inputs[0].text.clone();
    }
    let mut result = String::from("Queued follow-up requests, in order:\n");
    for (index, input) in inputs.iter().enumerate() {
        result.push_str(&format!("\n{}. {}", index + 1, input.text.trim()));
    }
    result
}

fn automatic_checkpoint_label(task: &str) -> String {
    let task = task.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt = task.chars().take(64).collect::<String>();
    format!("before: {excerpt}")
}

fn spawn_cancellation_signal(cancellation: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancellation.cancel();
        }
    })
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
    let explicit_approval_policy = args.common.approval_policy.is_some();
    let (mut config, workspace) = resolve_config(&args.common)?;
    if !explicit_approval_policy {
        config.agent.approval_policy = ApprovalPolicy::Never;
    }
    run_manifest(BenchOptions {
        manifest: args.manifest,
        output: args.output,
        default_workspace: workspace,
        config,
        keep_going: args.keep_going,
    })
    .await
}

fn spawn_terminal_approval_reviewer(
    mut requests: tokio::sync::mpsc::UnboundedReceiver<ApprovalEnvelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let detail = request.request.detail.clone();
            let kind = request.request.kind.as_str();
            let risk = request.request.risk.as_str();
            let decision = tokio::task::spawn_blocking(move || {
                eprintln!(
                    "\nApproval required · {kind} · {risk}\n  {detail}\n\n  [y] allow once  [s] allow session  [n] deny"
                );
                loop {
                    eprint!("approval> ");
                    let _ = io::Write::flush(&mut io::stderr());
                    let mut answer = String::new();
                    if io::stdin().read_line(&mut answer).is_err() {
                        return ApprovalDecision::Deny {
                            reason: "failed to read approval response".into(),
                        };
                    }
                    match answer.trim().to_ascii_lowercase().as_str() {
                        "y" | "yes" | "allow" => return ApprovalDecision::AllowOnce,
                        "s" | "session" | "always" => {
                            return ApprovalDecision::AllowSession;
                        }
                        "n" | "no" | "deny" => {
                            return ApprovalDecision::Deny {
                                reason: "denied by user".into(),
                            };
                        }
                        _ => eprintln!("Enter y, s, or n."),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| ApprovalDecision::Deny {
                reason: "approval reviewer stopped".into(),
            });
            request.resolve(decision);
        }
    })
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
    if args.no_stream {
        config.model.streaming = false;
    }
    if let Some(approval_policy) = args.approval_policy {
        config.agent.approval_policy = approval_policy.into();
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
    fn parses_approval_policy_override() {
        let cli = Cli::try_parse_from([
            "wecode",
            "run",
            "--approval-policy",
            "never",
            "fix the test",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run command");
        };
        assert!(matches!(
            args.common.approval_policy,
            Some(ApprovalPolicyArg::Never)
        ));
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
