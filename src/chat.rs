use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::Result;
use console::{Style, Term};
use ignore::WalkBuilder;
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler, KeyCode,
    KeyEvent, Modifiers, RepeatCount, error::ReadlineError,
};
use tokio::sync::mpsc;

use crate::approval::ApprovalRequest;
use crate::attachments::PendingAttachment;
use crate::background_process::{
    BackgroundProcessEvent, BackgroundProcessStatus, BackgroundProcessSummary,
};
use crate::commands::PromptCommand;
use crate::config::{
    CacheMode, Config, ProviderFamily, WireApi, default_config_path, default_history_path,
};
use crate::context::{CompactionReport, ContextUsage};
use crate::executor::ExecutionResult;
use crate::hooks::{HookReport, HookStatus, HookSummary};
use crate::input_queue::QueueSnapshot;
use crate::instructions::InstructionSet;
use crate::interaction::{PlanSnapshot, UserInputRequest};
use crate::lsp::{LspEvent, LspServerStatus, LspServerSummary};
use crate::mcp::{McpServerReport, McpServerState};
use crate::model::Usage;
use crate::protocol::PlanStatus;
use crate::session::{SessionCheckpoint, SessionSummary};
use crate::skills::Skill;
use crate::subagent::{SubagentEvent, SubagentStatus, SubagentSummary};
use crate::tui::{self, TuiHandle, TuiMessage, TuiTone};
use crate::ui::TerminalOutput;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatCommand {
    Agents,
    Attach(String),
    Attachments,
    Approve,
    ApproveSession,
    Cancel,
    Checkpoint(Option<String>),
    Checkpoints,
    ClearQueue,
    Compact(Option<String>),
    Commands,
    Config,
    Context,
    Deny(String),
    Detach(String),
    Diff,
    Help,
    History,
    Hooks,
    Lsp,
    LspRestart,
    Mcp,
    Model(Option<String>),
    Fork(Option<String>),
    New,
    Plan,
    Processes,
    Rename(String),
    Rewind(Option<String>),
    Resume(Option<String>),
    Rules,
    Sessions,
    Skill { name: String, arguments: String },
    Skills,
    Status,
    StopAgent(Option<u64>),
    StopProcess(Option<u64>),
    Queue,
    Unknown { name: String, arguments: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChatInput {
    Task(String),
    FollowUp(String),
    Shell {
        command: String,
        include_in_context: bool,
    },
    Command(ChatCommand),
    Interrupted,
    Exit,
}

pub struct ChatShell {
    editor: Option<DefaultEditor>,
    follow_up_submit: Arc<AtomicBool>,
    history_path: PathBuf,
    output: TerminalOutput,
    tui_receiver: Option<std::sync::mpsc::Receiver<TuiMessage>>,
    completions: Vec<tui::CommandCompletion>,
    models: Vec<String>,
}

#[derive(Clone)]
pub struct ChatView {
    history_path: PathBuf,
    output: TerminalOutput,
}

impl ChatShell {
    pub fn new(
        workspace: &Path,
        config: &Config,
        commands: &[PromptCommand],
        skills: &[Skill],
    ) -> Result<Self> {
        let history_path = default_history_path();
        if let Some(parent) = history_path.parent() {
            create_private_directory(parent)?;
        }
        let follow_up_submit = Arc::new(AtomicBool::new(false));
        let use_tui = tui::supported();
        let mut editor = if !use_tui && io::stdin().is_terminal() && io::stdout().is_terminal() {
            let mut editor = DefaultEditor::new()?;
            if history_path.is_file() {
                let _ = editor.load_history(&history_path);
            }
            editor.bind_sequence(
                KeyEvent(KeyCode::Enter, Modifiers::ALT),
                EventHandler::Conditional(Box::new(FollowUpSubmit {
                    requested: follow_up_submit.clone(),
                })),
            );
            Some(editor)
        } else {
            None
        };
        let (tui_handle, tui_receiver) = if use_tui {
            let (handle, receiver) = TuiHandle::new();
            let file_handle = handle.clone();
            let file_workspace = workspace.to_path_buf();
            thread::spawn(move || {
                file_handle.set_files(completion_files(&file_workspace));
            });
            (Some(handle), Some(receiver))
        } else {
            (None, None)
        };
        let output = if let Some(handle) = &tui_handle {
            TerminalOutput::tui(handle.clone())
        } else {
            match editor.as_mut() {
                Some(editor) => match editor.create_external_printer() {
                    Ok(printer) => TerminalOutput::external(Box::new(printer)),
                    Err(_) => TerminalOutput::stdout(),
                },
                None => TerminalOutput::stdout(),
            }
        };
        Ok(Self {
            editor,
            follow_up_submit,
            history_path,
            output,
            tui_receiver,
            completions: command_completions(commands, skills),
            models: model_completions(config),
        })
    }

    pub fn view(&self) -> ChatView {
        ChatView {
            history_path: self.history_path.clone(),
            output: self.output.clone(),
        }
    }

    pub fn into_input_stream(mut self) -> mpsc::UnboundedReceiver<Result<ChatInput>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Some(tui_receiver) = self.tui_receiver.take() {
            let history_path = self.history_path.clone();
            let completions = self.completions.clone();
            let models = self.models.clone();
            thread::spawn(move || {
                if let Err(error) = tui::run(
                    tui_receiver,
                    sender.clone(),
                    history_path,
                    completions,
                    models,
                ) {
                    let _ = sender.send(Err(error));
                }
            });
            return receiver;
        }
        thread::spawn(move || {
            loop {
                let input = self.read_input();
                let done = matches!(input, Ok(ChatInput::Exit)) || input.is_err();
                if sender.send(input).is_err() || done {
                    break;
                }
            }
        });
        receiver
    }

    fn read_input(&mut self) -> Result<ChatInput> {
        self.output.print(format!(
            "╭─ {}\n",
            Style::new().blue().bold().apply_to("You")
        ))?;
        let line = if let Some(editor) = self.editor.as_mut() {
            match editor.readline("╰─› ") {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => return Ok(ChatInput::Interrupted),
                Err(ReadlineError::Eof) => return Ok(ChatInput::Exit),
                Err(error) => return Err(error.into()),
            }
        } else {
            print!("╰─› ");
            io::stdout().flush()?;
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                return Ok(ChatInput::Exit);
            }
            line
        };

        let line = line.trim();
        if line.is_empty() {
            return Ok(ChatInput::Interrupted);
        }
        if let Some(editor) = self.editor.as_mut()
            && !line.starts_with("!!")
        {
            let _ = editor.add_history_entry(line);
            if editor.save_history(&self.history_path).is_ok() {
                let _ = make_file_private(&self.history_path);
            }
        }
        let input = parse_input(line);
        if self.follow_up_submit.swap(false, Ordering::AcqRel)
            && let ChatInput::Task(text) = input
        {
            return Ok(ChatInput::FollowUp(text));
        }
        Ok(input)
    }
}

fn completion_files(workspace: &Path) -> Vec<String> {
    const MAX_COMPLETION_FILES: usize = 50_000;

    let mut builder = WalkBuilder::new(workspace);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | "target")
                )
        });
    let mut files = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(workspace)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        })
        .filter(|path| !path.contains(['"', '\n', '\r']))
        .take(MAX_COMPLETION_FILES)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

fn model_completions(config: &Config) -> Vec<String> {
    let suggested: &[&str] = match config.model.provider.as_str() {
        "openai" => &["gpt-5.4", "gpt-5.4-mini", "gpt-5.4-codex"],
        "anthropic" => &["claude-sonnet-4-6", "claude-haiku-4-5", "claude-opus-4-6"],
        "gemini" => &[
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
        ],
        "openrouter" => &[
            "anthropic/claude-sonnet-4.6",
            "openai/gpt-5.4",
            "google/gemini-2.5-pro",
        ],
        "deepseek" => &["deepseek-chat", "deepseek-reasoner"],
        "groq" => &["openai/gpt-oss-120b", "qwen/qwen3-32b"],
        "xai" => &["grok-code-fast-1", "grok-4.1-fast"],
        "mistral" => &["devstral-medium-latest", "devstral-small-latest"],
        "ollama" => &["qwen3-coder", "gpt-oss"],
        _ => &[],
    };
    let mut models = Vec::new();
    for model in std::iter::once(config.model.model.as_str()).chain(suggested.iter().copied()) {
        if !models.iter().any(|current| current == model) {
            models.push(model.to_owned());
        }
    }
    models
}

fn command_completions(
    commands: &[PromptCommand],
    skills: &[Skill],
) -> Vec<tui::CommandCompletion> {
    let builtins = [
        ("/new", "Start a fresh conversation"),
        ("/attach", "Attach a text file or image"),
        ("/attachments", "Show pending attachments"),
        ("/detach", "Remove pending attachments"),
        ("/diff", "Show staged, unstaged, and untracked changes"),
        ("/resume", "Resume a saved session"),
        ("/sessions", "List saved sessions"),
        ("/checkpoint", "Save a conversation checkpoint"),
        ("/checkpoints", "List checkpoints"),
        ("/fork", "Fork from a checkpoint"),
        ("/rewind", "Rewind safely by forking"),
        ("/plan", "Show the current plan"),
        ("/processes", "Show managed background processes"),
        ("/stop-process", "Stop a managed background process"),
        ("/queue", "Show queued messages"),
        ("/clear-queue", "Clear queued messages"),
        ("/cancel", "Cancel the active task"),
        ("/compact", "Compact older context with an optional focus"),
        ("/context", "Show context usage and prompt-cache metrics"),
        ("/model", "Show or switch the session model"),
        ("/status", "Show session status"),
        ("/mcp", "Show MCP servers"),
        ("/lsp", "Show language servers"),
        ("/lsp-restart", "Restart language servers"),
        ("/agents", "Show delegated subagents"),
        ("/stop-agent", "Stop a delegated subagent"),
        ("/hooks", "Show lifecycle hooks"),
        ("/commands", "Show reusable prompts"),
        ("/skills", "Show Agent Skills"),
        ("/rules", "Show project rules"),
        ("/config", "Show the active config"),
        ("/history", "Show input history"),
        ("/help", "Show command help"),
        ("/quit", "Exit WeCode"),
    ];
    let mut output = builtins
        .into_iter()
        .map(|(command, description)| tui::CommandCompletion {
            command: command.into(),
            description: description.into(),
        })
        .collect::<Vec<_>>();
    output.extend(commands.iter().map(|command| tui::CommandCompletion {
        command: format!("/{}", command.name),
        description: command.description.clone(),
    }));
    output.extend(skills.iter().map(|skill| tui::CommandCompletion {
        command: format!("/skill:{}", skill.name),
        description: skill.description.clone(),
    }));
    output.sort_by(|left, right| left.command.cmp(&right.command));
    output.dedup_by(|left, right| left.command == right.command);
    output
}

impl ChatView {
    pub fn output(&self) -> TerminalOutput {
        self.output.clone()
    }

    pub fn welcome(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
        skill_count: usize,
        command_count: usize,
    ) -> Result<()> {
        if self.output.set_tui_header(
            format!(
                "{} · {}  |  {}",
                config.model.provider,
                config.model.model,
                compact_path(workspace)
            ),
            format!(
                "session {} · {} rules · {} skills · {} commands · {} · /help",
                short_id(&session.id),
                instructions.files.len(),
                skill_count,
                command_count,
                protocol_name(config.model.family, config.model.wire_api)
            ),
        ) {
            return Ok(());
        }
        self.output.print(render_welcome(
            config,
            workspace,
            session,
            instructions,
            skill_count,
            command_count,
        ))
    }

    pub fn clear_screen(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
        skill_count: usize,
        command_count: usize,
    ) -> Result<()> {
        if self.output.clear_tui() {
            return self.welcome(
                config,
                workspace,
                session,
                instructions,
                skill_count,
                command_count,
            );
        }
        self.output.print(format!(
            "\x1b[2J\x1b[H]{}",
            render_welcome(
                config,
                workspace,
                session,
                instructions,
                skill_count,
                command_count,
            )
        ))
    }

    pub fn show_help(&self) -> Result<()> {
        self.output.print(format!(
            "\n{}\n\
             \n  {:12} Start a fresh conversation\
             \n  {:12} Resume the latest or selected session\
             \n  {:12} List recent sessions for this workspace\
             \n  {:12} Rename the current session\
             \n  {:12} Save a named conversation checkpoint\
             \n  {:12} List checkpoints in the current session\
             \n  {:12} Fork from now or a selected checkpoint\
             \n  {:12} Rewind safely by forking from an earlier checkpoint\
             \n  {:12} Attach a text file or image to the next message\
             \n  {:12} Show files attached to the next message\
             \n  {:12} Remove the last, selected, or all attachments\
             \n  {:12} Fuzzy-search and attach a repository file\
             \n  {:12} Show staged, unstaged, and untracked changes\
             \n  {:12} Run a shell command and include its result in context\
             \n  {:12} Run a shell command without saving it to context\
             \n  {:12} Show the current task plan\
             \n  {:12} Show managed background processes\
             \n  {:12} Stop a managed background process\
             \n  {:12} Steer the active task at the next model boundary\
             \n  {:12} Queue work for after the active task\
             \n  {:12} Show pending steer and follow-up messages\
             \n  {:12} Clear all pending messages\
             \n  {:12} Cancel the active model request or command\
             \n  {:12} Allow a pending action once\
             \n  {:12} Allow matching actions for this session\
             \n  {:12} Deny a pending action with optional feedback\
             \n  {:12} Show context usage and prompt-cache metrics\
             \n  {:12} Compact older context, preserving an optional focus\
             \n  {:12} Show or switch the model for this session\
             \n  {:12} Show model, workspace, cache, and context\
             \n  {:12} Show MCP server and tool status\
             \n  {:12} Show language-server status\
             \n  {:12} Restart language servers\
             \n  {:12} Show delegated subagents\
             \n  {:12} Stop a delegated subagent\
             \n  {:12} Show lifecycle hooks\
             \n  {:12} Show reusable prompt commands\
             \n  {:12} Show discovered skills\
             \n  {:12} Invoke a skill with optional arguments\
             \n  {:12} Show loaded project instruction files\
             \n  {:12} Show the active config path\
             \n  {:12} Show the history file\
             \n  {:12} Show this help\
             \n  {:12} Exit WeCode\
             \n\
             \n  During a run: Enter steers · Alt-Enter queues a follow-up · Ctrl-C cancels\n",
            Style::new().cyan().bold().apply_to("Commands"),
            "/new",
            "/resume [id]",
            "/sessions",
            "/rename <name>",
            "/checkpoint [name]",
            "/checkpoints",
            "/fork [checkpoint]",
            "/rewind [checkpoint]",
            "/attach <path>",
            "/attachments",
            "/detach [number|all]",
            "@path",
            "/diff",
            "!command",
            "!!command",
            "/plan",
            "/processes",
            "/stop-process <id>",
            "/steer <text>",
            "/followup <text>",
            "/queue",
            "/clear-queue",
            "/cancel",
            "/approve",
            "/approve-session",
            "/deny [reason]",
            "/context",
            "/compact [focus]",
            "/model [id]",
            "/status",
            "/mcp",
            "/lsp",
            "/lsp-restart",
            "/agents",
            "/stop-agent <id>",
            "/hooks",
            "/commands",
            "/skills",
            "/skill:<name>",
            "/rules",
            "/config",
            "/history",
            "/help",
            "/quit",
        ))
    }

    pub fn show_status(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
        context_messages: usize,
        queue: &QueueSnapshot,
    ) -> Result<()> {
        self.output.print(format!(
            "\n{}\n  id          {}\n  parent      {}\n  title       {}\n  provider    {}\n  model       {}\n  protocol    {}\n  workspace   {}\n  rules       {} files\n  cache       {}\n  context     {} messages\n  checkpoints {}\n  file        {}\n",
            Style::new().cyan().bold().apply_to("Session"),
            session.id,
            session.parent_session_id.as_deref().unwrap_or("none"),
            session.title.as_deref().unwrap_or("untitled"),
            config.model.provider,
            config.model.model,
            protocol_name(config.model.family, config.model.wire_api),
            workspace.display(),
            instructions.files.len(),
            cache_mode_name(config.cache.mode),
            context_messages,
            session.checkpoint_count,
            session.path.display(),
        ))?;
        if !queue.is_empty() {
            self.show_queue(queue)?;
        }
        Ok(())
    }

    pub fn show_model(&self, config: &Config) -> Result<()> {
        self.output.print(format!(
            "\n{}\n  provider  {}\n  model     {}\n  protocol  {}\n\n  Switch with /model <id>. The change applies to this session only.\n",
            Style::new().cyan().bold().apply_to("Model"),
            config.model.provider,
            config.model.model,
            protocol_name(config.model.family, config.model.wire_api),
        ))
    }

    pub fn show_context(
        &self,
        usage: ContextUsage,
        max_tokens: u64,
        rules_tokens: u64,
        tool_tokens: u64,
        last_usage: Option<(Usage, usize)>,
    ) -> Result<()> {
        self.sync_context_metrics(usage, max_tokens, last_usage);
        let percent = usage.percent_of(max_tokens);
        let bar = context_bar(percent, 24);
        let provider = match last_usage {
            Some((usage, exact_cache_hits)) => format!(
                "{} input · {} output · {} cache read · {} cache write · {} exact hit{}",
                compact_number(usage.input_tokens),
                compact_number(usage.output_tokens),
                compact_number(usage.cache_read_tokens),
                compact_number(usage.cache_write_tokens),
                exact_cache_hits,
                if exact_cache_hits == 1 { "" } else { "s" },
            ),
            None => "no model request in this session yet".into(),
        };
        let body = format!(
            "{bar}  {percent}%\n\n\
             Conversation  {} / {} estimated tokens\n\
             Text          {} tokens in {} messages ({} user · {} assistant)\n\
             Images        {} tokens across {} images\n\
             Rules         ~{} tokens\n\
             Tools         ~{} tokens\n\
             Last request  {provider}\n\n\
             Limits are estimates; providers tokenize images and tool schemas differently.",
            compact_number(usage.total_tokens),
            compact_number(max_tokens),
            compact_number(usage.text_tokens),
            usage.messages,
            usage.user_messages,
            usage.assistant_messages,
            compact_number(usage.image_tokens),
            usage.images,
            compact_number(rules_tokens),
            compact_number(tool_tokens),
        );
        if self.output.tui_entry("CONTEXT", &body, TuiTone::Normal) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n{body}\n",
            Style::new().cyan().bold().apply_to("Context")
        ))
    }

    pub fn sync_context_metrics(
        &self,
        usage: ContextUsage,
        max_tokens: u64,
        last_usage: Option<(Usage, usize)>,
    ) {
        let mut metrics = format!(
            "context {}/{} · {}%",
            compact_number(usage.total_tokens),
            compact_number(max_tokens),
            usage.percent_of(max_tokens),
        );
        if let Some((provider, exact_hits)) = last_usage {
            metrics.push_str(&format!(
                " · {} cached · {} exact",
                compact_number(provider.cache_read_tokens),
                exact_hits
            ));
        }
        self.output.set_tui_metrics(Some(metrics));
    }

    pub fn show_compaction(&self, report: &CompactionReport, focus: Option<&str>) -> Result<()> {
        let saved = report
            .before
            .total_tokens
            .saturating_sub(report.after.total_tokens);
        let mut body = format!(
            "{} → {} messages · {} → {} estimated tokens · {} saved",
            report.before.messages,
            report.after.messages,
            compact_number(report.before.total_tokens),
            compact_number(report.after.total_tokens),
            compact_number(saved),
        );
        if let Some(focus) = focus.map(str::trim).filter(|focus| !focus.is_empty()) {
            body.push_str("\n\nPreserved focus: ");
            body.push_str(focus);
        }
        if self.output.tui_entry("COMPACT", &body, TuiTone::Success) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n{body}\n",
            Style::new().green().bold().apply_to("Context compacted")
        ))
    }

    pub fn shell_started(&self, command: &str) {
        self.output
            .set_tui_status(Some(format!("● Shell · {}", one_line(command))));
    }

    pub fn show_shell_result(
        &self,
        command: &str,
        result: &ExecutionResult,
        included: bool,
    ) -> Result<()> {
        self.output.set_tui_status(None);
        let mut body = format!(
            "$ {command}\nexit {} · {} ms{}",
            result
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into()),
            result.duration_ms,
            if result.timed_out {
                " · timed out"
            } else {
                ""
            }
        );
        if !result.stdout.is_empty() {
            body.push_str("\n\n");
            body.push_str(result.stdout.trim_end());
        }
        if !result.stderr.is_empty() {
            body.push_str("\n\nstderr:\n");
            body.push_str(result.stderr.trim_end());
        }
        if result.truncated_bytes > 0 {
            body.push_str(&format!(
                "\n\n… {} output bytes omitted",
                result.truncated_bytes
            ));
        }
        body.push_str(if included {
            "\n\nIncluded in the next model context."
        } else {
            "\n\nExcluded from model context and session history."
        });
        let tone = if result.success() {
            TuiTone::Success
        } else {
            TuiTone::Warning
        };
        if self.output.tui_entry("SHELL", &body, tone) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n{body}\n",
            Style::new().cyan().bold().apply_to("Shell")
        ))
    }

    pub fn shell_failed(&self, error: impl AsRef<str>) -> Result<()> {
        self.output.set_tui_status(None);
        self.warning(format!("Shell command failed to start: {}", error.as_ref()))
    }

    pub fn show_diff(&self, diff: &str) -> Result<()> {
        let body = bounded_middle(diff, 512 * 1_024);
        if self.output.tui_entry("DIFF", &body, TuiTone::Normal) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n{}\n",
            Style::new().cyan().bold().apply_to("Working tree diff"),
            body
        ))
    }

    pub fn refresh_model_header(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
        skill_count: usize,
        command_count: usize,
    ) -> Result<()> {
        self.output.set_tui_header(
            format!(
                "{} · {}  |  {}",
                config.model.provider,
                config.model.model,
                compact_path(workspace)
            ),
            format!(
                "session {} · {} rules · {} skills · {} commands · {} · /help",
                short_id(&session.id),
                instructions.files.len(),
                skill_count,
                command_count,
                protocol_name(config.model.family, config.model.wire_api)
            ),
        );
        self.notice(format!(
            "Model switched to {} / {} for this session.",
            config.model.provider, config.model.model
        ))
    }

    pub fn show_rules(&self, instructions: &InstructionSet) -> Result<()> {
        let mut output = format!(
            "\n{}\n",
            Style::new().cyan().bold().apply_to("Project rules")
        );
        if instructions.files.is_empty() {
            output.push_str("  No AGENTS.md, CLAUDE.md, or rules files loaded.\n\n");
            return self.output.print(output);
        }
        for file in &instructions.files {
            let suffix = if file.truncated { " (truncated)" } else { "" };
            output.push_str(&format!(
                "  {}  ~{} tokens{suffix}\n",
                file.path.display(),
                xai_token_estimation::estimate_tokens(&file.content)
            ));
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn show_processes(&self, processes: &[BackgroundProcessSummary]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} known\n",
            Style::new().cyan().bold().apply_to("Background processes"),
            processes.len()
        );
        if processes.is_empty() {
            output.push_str("  No managed background processes.\n\n");
            return self.output.print(output);
        }
        for process in processes {
            let exit = process
                .exit_code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            output.push_str(&format!(
                "  #{:<3} {:9} {:>7}ms · {}{} · {} bytes\n",
                process.process_id,
                process.status.as_str(),
                process.duration_ms,
                one_line(&process.description),
                exit,
                process.total_output_bytes,
            ));
        }
        output.push_str("\n  Stop one with /stop-process <id>.\n\n");
        self.output.print(output)
    }

    pub fn show_process_event(&self, event: &BackgroundProcessEvent) -> Result<()> {
        let summary = &event.summary;
        let exit = summary
            .exit_code
            .map(|code| format!(" · exit {code}"))
            .unwrap_or_default();
        let tail = one_line(&event.output_tail);
        let detail = format!(
            "{} · {}ms{exit} · {}{}",
            summary.status.as_str(),
            summary.duration_ms,
            summary.description,
            if tail.is_empty() {
                String::new()
            } else {
                format!("\n{tail}")
            }
        );
        let tone = match summary.status {
            BackgroundProcessStatus::Exited if summary.exit_code == Some(0) => TuiTone::Success,
            BackgroundProcessStatus::Running => TuiTone::Normal,
            BackgroundProcessStatus::Exited
            | BackgroundProcessStatus::Failed
            | BackgroundProcessStatus::Killed
            | BackgroundProcessStatus::TimedOut => TuiTone::Warning,
        };
        if self
            .output
            .tui_entry(format!("PROCESS · {}", summary.process_id), &detail, tone)
        {
            return Ok(());
        }
        self.output.print(format!(
            "  process #{} · {}\n",
            summary.process_id,
            one_line(&detail)
        ))
    }

    pub fn show_mcp(&self, reports: &[McpServerReport]) -> Result<()> {
        let mut output = format!("\n{}\n", Style::new().cyan().bold().apply_to("MCP servers"));
        if reports.is_empty() {
            output.push_str("  No MCP servers configured.\n\n");
            return self.output.print(output);
        }
        for report in reports {
            let state = match report.state {
                McpServerState::Connected => "connected",
                McpServerState::Disabled => "disabled",
                McpServerState::Failed => "failed",
            };
            output.push_str(&format!(
                "  {:16} {:10} {} tool{}\n",
                report.name,
                state,
                report.tools.len(),
                if report.tools.len() == 1 { "" } else { "s" }
            ));
            for tool in &report.tools {
                output.push_str(&format!("    · {tool}\n"));
            }
            if let Some(error) = &report.error {
                output.push_str(&format!("    ! {}\n", one_line(error)));
            }
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn show_lsp(&self, servers: &[LspServerSummary]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} available\n",
            Style::new().cyan().bold().apply_to("Language servers"),
            servers.len()
        );
        if servers.is_empty() {
            output.push_str(
                "  No installed or configured language servers detected.\n\
                 \n  Configure [lsp.servers.<name>] or install a supported server.\n\n",
            );
            return self.output.print(output);
        }
        for server in servers {
            output.push_str(&format!(
                "  {:18} {:10} {} · {}\n",
                server.name,
                server.status.as_str(),
                one_line(&server.command),
                server.extensions.join(", ")
            ));
            if let Some(error) = &server.error {
                output.push_str(&format!("    ! {}\n", one_line(error)));
            }
        }
        output.push_str(
            "\n  Servers start lazily when the agent queries or edits a matching file.\n\n",
        );
        self.output.print(output)
    }

    pub fn show_lsp_event(&self, event: &LspEvent) -> Result<()> {
        let tone = match event.status {
            LspServerStatus::Ready => TuiTone::Success,
            LspServerStatus::Failed => TuiTone::Warning,
            LspServerStatus::Available | LspServerStatus::Starting | LspServerStatus::Stopped => {
                TuiTone::Dim
            }
        };
        if self
            .output
            .tui_entry(format!("LSP · {}", event.server), &event.detail, tone)
        {
            return Ok(());
        }
        self.output.print(format!(
            "  lsp {} · {}\n",
            event.server,
            one_line(&event.detail)
        ))
    }

    pub fn show_subagents(&self, agents: &[SubagentSummary]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} total\n",
            Style::new().cyan().bold().apply_to("Subagents"),
            agents.len()
        );
        if agents.is_empty() {
            output.push_str("  No subagents have been started in this session.\n\n");
            return self.output.print(output);
        }
        for agent in agents {
            output.push_str(&format!(
                "  #{:<3} {:10} {:16} {} · {} turn{} · {} ms\n",
                agent.id,
                agent.status.as_str(),
                agent.agent_type,
                one_line(&agent.description),
                agent.turns,
                if agent.turns == 1 { "" } else { "s" },
                agent.duration_ms
            ));
            if let Some(result) = &agent.result {
                output.push_str(&format!("       {}\n", one_line(result)));
            } else if let Some(error) = &agent.error {
                output.push_str(&format!("       ! {}\n", one_line(error)));
            }
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn sync_attachments(&self, attachments: &[PendingAttachment]) {
        self.output.set_tui_attachments(
            attachments
                .iter()
                .enumerate()
                .map(|(index, attachment)| format!("{}:{}", index + 1, attachment.display_name()))
                .collect(),
        );
    }

    pub fn show_attachments(&self, attachments: &[PendingAttachment]) -> Result<()> {
        self.sync_attachments(attachments);
        if attachments.is_empty() {
            return self.notice("No files are attached to the next message.");
        }
        if self.output.tui_entry(
            "ATTACHMENTS",
            attachments
                .iter()
                .enumerate()
                .map(|(index, attachment)| format!("{}. {}", index + 1, attachment.display_name()))
                .collect::<Vec<_>>()
                .join("\n"),
            TuiTone::Normal,
        ) {
            return Ok(());
        }
        let mut output = format!(
            "\n{}\n",
            Style::new().magenta().bold().apply_to("Attachments")
        );
        for (index, attachment) in attachments.iter().enumerate() {
            output.push_str(&format!("  {}. {}\n", index + 1, attachment.display_name()));
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn show_subagent_event(&self, event: &SubagentEvent) -> Result<()> {
        let tone = match event.status {
            SubagentStatus::Completed => TuiTone::Success,
            SubagentStatus::Failed => TuiTone::Warning,
            SubagentStatus::Cancelled => TuiTone::Dim,
            SubagentStatus::Queued | SubagentStatus::Running => TuiTone::Normal,
        };
        let label = format!(
            "Agent #{} · {} · {}",
            event.id,
            event.agent_type,
            event.status.as_str()
        );
        if self.output.tui_entry(label.clone(), &event.detail, tone) {
            return Ok(());
        }
        self.output
            .print(format!("  {label} · {}\n", one_line(&event.detail)))
    }

    pub fn show_skills(&self, skills: &[Skill]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} available\n",
            Style::new().cyan().bold().apply_to("Skills"),
            skills.len()
        );
        if skills.is_empty() {
            output.push_str("  No skills discovered.\n\n");
            return self.output.print(output);
        }
        for skill in skills {
            output.push_str(&format!(
                "  {:24} {:8} {}{}\n",
                skill.name,
                skill.scope.as_str(),
                one_line(&skill.description),
                if skill.disable_model_invocation {
                    "  · explicit only"
                } else {
                    ""
                }
            ));
        }
        output.push_str("\n  Invoke with /skill:<name> [arguments].\n\n");
        self.output.print(output)
    }

    pub fn show_commands(&self, commands: &[PromptCommand]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} available\n",
            Style::new().cyan().bold().apply_to("Prompt commands"),
            commands.len()
        );
        if commands.is_empty() {
            output.push_str("  No prompt commands discovered.\n\n");
            return self.output.print(output);
        }
        for command in commands {
            let hint = command
                .argument_hint
                .as_deref()
                .map(|hint| format!("{hint} · "))
                .unwrap_or_default();
            output.push_str(&format!(
                "  /{:<22} {:8} {}{}\n",
                command.name,
                command.scope.as_str(),
                hint,
                one_line(&command.description)
            ));
        }
        output.push_str("\n  Invoke directly, for example /review src/.\n\n");
        self.output.print(output)
    }

    pub fn show_hooks(&self, hooks: &[HookSummary]) -> Result<()> {
        let mut output = format!(
            "\n{} · {} enabled\n",
            Style::new().cyan().bold().apply_to("Lifecycle hooks"),
            hooks.len()
        );
        if hooks.is_empty() {
            output.push_str("  No lifecycle hooks configured.\n\n");
            return self.output.print(output);
        }
        for hook in hooks {
            output.push_str(&format!(
                "  {:18} {:12} {}{}{}\n",
                hook.event.as_str(),
                hook.label,
                hook.matcher
                    .as_deref()
                    .map(|matcher| format!("/{matcher}/"))
                    .unwrap_or_else(|| "all".into()),
                if hook.asynchronous { " · async" } else { "" },
                if hook.fail_closed {
                    " · fail-closed"
                } else {
                    ""
                }
            ));
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn show_hook_reports(&self, reports: &[HookReport]) -> Result<()> {
        for report in reports {
            let detail = if report.suppress_output {
                format!(
                    "{} · {} · {}ms",
                    report.event.as_str(),
                    report.status.as_str(),
                    report.duration_ms
                )
            } else {
                let output = if !report.stderr.trim().is_empty() {
                    one_line(&report.stderr)
                } else {
                    one_line(&report.stdout)
                };
                format!(
                    "{} · {} · {}ms{}",
                    report.event.as_str(),
                    report.status.as_str(),
                    report.duration_ms,
                    if output.is_empty() {
                        String::new()
                    } else {
                        format!(" · {output}")
                    }
                )
            };
            let tone = match report.status {
                HookStatus::Started | HookStatus::Completed => TuiTone::Dim,
                HookStatus::Blocked | HookStatus::Failed | HookStatus::TimedOut => TuiTone::Warning,
            };
            if self
                .output
                .tui_entry(format!("HOOK · {}", report.label), &detail, tone)
            {
                continue;
            }
            self.output
                .print(format!("  hook {} · {detail}\n", report.label))?;
        }
        Ok(())
    }

    pub fn show_sessions(&self, sessions: &[SessionSummary]) -> Result<()> {
        let mut output = format!(
            "\n{}",
            Style::new().cyan().bold().apply_to("Recent sessions")
        );
        if sessions.is_empty() {
            output.push_str("\n  No saved sessions for this workspace.\n\n");
            return self.output.print(output);
        }
        output.push('\n');
        for session in sessions.iter().take(20) {
            output.push_str(&format!(
                "  {:10} {:>4} messages  {:>2} checkpoints  {}{}\n",
                short_id(&session.id),
                session.message_count,
                session.checkpoint_count,
                session.title.as_deref().unwrap_or("untitled"),
                session
                    .parent_session_id
                    .as_deref()
                    .map(|parent| format!("  ← {}", short_id(parent)))
                    .unwrap_or_default(),
            ));
        }
        output.push_str("\n  Resume with /resume <id>.\n\n");
        self.output.print(output)
    }

    pub fn show_checkpoints(&self, checkpoints: &[SessionCheckpoint]) -> Result<()> {
        let mut output = format!("\n{}", Style::new().cyan().bold().apply_to("Checkpoints"));
        if checkpoints.is_empty() {
            output.push_str("\n  No checkpoints in this session.\n\n");
            return self.output.print(output);
        }
        output.push('\n');
        for checkpoint in checkpoints.iter().rev().take(30) {
            output.push_str(&format!(
                "  {:8} {:>4} messages  {}{}\n",
                checkpoint.id,
                checkpoint.message_count,
                checkpoint.label,
                if checkpoint.automatic {
                    "  · auto"
                } else {
                    ""
                },
            ));
        }
        output.push_str("\n  Use /rewind <id> or /fork <id>.\n\n");
        self.output.print(output)
    }

    pub fn show_config_path(&self) -> Result<()> {
        self.output.print(format!(
            "\n{} {}\n",
            Style::new().cyan().bold().apply_to("Config"),
            default_config_path().display(),
        ))
    }

    pub fn show_history_path(&self) -> Result<()> {
        self.output.print(format!(
            "\n{} {}\n",
            Style::new().cyan().bold().apply_to("History"),
            self.history_path.display(),
        ))
    }

    pub fn show_setup_required(&self, error: &anyhow::Error) -> Result<()> {
        if self.output.tui_entry(
            "SETUP REQUIRED",
            format!(
                "{error}\n\nRun `wecode setup` to configure a provider and store its key safely."
            ),
            TuiTone::Warning,
        ) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n  {}\n\n  Run {} to configure a provider and store its key safely.\n",
            Style::new().yellow().bold().apply_to("Setup required"),
            error,
            Style::new().cyan().bold().apply_to("wecode setup"),
        ))
    }

    pub fn show_queue(&self, queue: &QueueSnapshot) -> Result<()> {
        let mut output = format!(
            "\n{} · {} pending\n",
            Style::new().cyan().bold().apply_to("Input queue"),
            queue.len()
        );
        for input in &queue.steering {
            let images = image_count_label(input.images.len());
            output.push_str(&format!(
                "  {} #{:<3} {}{}\n",
                Style::new().cyan().apply_to("steer"),
                input.id,
                one_line(&input.text),
                images
            ));
        }
        for input in &queue.follow_ups {
            let images = image_count_label(input.images.len());
            output.push_str(&format!(
                "  {} #{:<3} {}{}\n",
                Style::new().magenta().apply_to("follow-up"),
                input.id,
                one_line(&input.text),
                images
            ));
        }
        if queue.is_empty() {
            output.push_str("  No pending input.\n");
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn show_queued(&self, kind: &str, id: u64, pending: usize) -> Result<()> {
        self.output.print(format!(
            "  {} queued {kind} #{id} · {pending} pending\n",
            Style::new().cyan().apply_to("↳")
        ))
    }

    pub fn show_approval(&self, request: &ApprovalRequest) -> Result<()> {
        self.output.set_tui_composer(
            Some(" Approval decision ".into()),
            Some("Type /approve, /approve-session, or /deny [reason]".into()),
        );
        if self.output.tui_entry(
            "APPROVAL REQUIRED",
            format!(
                "{} · {} risk\n{}\n\n/approve once · /approve-session · /deny [reason]",
                request.kind.as_str(),
                request.risk.as_str(),
                request.detail
            ),
            TuiTone::Warning,
        ) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n  id       #{}\n  action   {}\n  risk     {}\n  summary  {}\n  detail   {}\n\n  {} allow once · {} allow session · {} deny\n\n",
            Style::new().yellow().bold().apply_to("Approval required"),
            request.id,
            request.kind.as_str(),
            request.risk.as_str(),
            request.summary,
            one_line(&request.detail),
            Style::new().cyan().apply_to("/approve"),
            Style::new().cyan().apply_to("/approve-session"),
            Style::new().cyan().apply_to("/deny [reason]"),
        ))
    }

    pub fn show_question(&self, request: &UserInputRequest) -> Result<()> {
        let mut body = String::new();
        for (question_index, question) in request.questions.iter().enumerate() {
            if question_index > 0 {
                body.push('\n');
            }
            body.push_str(&format!("{} · {}\n", question.header, question.question));
            for (option_index, option) in question.options.iter().enumerate() {
                body.push_str(&format!(
                    "  {}. {} — {}\n",
                    option_index + 1,
                    option.label,
                    option.description
                ));
            }
        }
        if request.questions.len() > 1 {
            body.push_str("\nAnswer each question in order, separated with semicolons.");
        } else {
            body.push_str("\nReply with an option number or type another answer.");
        }
        self.output.set_tui_composer(
            Some(" Answer WeCode ".into()),
            Some(
                if request.questions.len() > 1 {
                    "Answer each question; separate answers with semicolons"
                } else {
                    "Type an option number or a free-form answer"
                }
                .into(),
            ),
        );
        if self.output.tui_entry("QUESTION", &body, TuiTone::Warning) {
            return Ok(());
        }
        self.output.print(format!(
            "\n{}\n{}\n",
            Style::new()
                .magenta()
                .bold()
                .apply_to(format!("Question #{}", request.id)),
            body
        ))
    }

    pub fn clear_interaction_prompt(&self) {
        self.output.set_tui_composer(None, None);
    }

    pub fn sync_plan(&self, plan: &PlanSnapshot) -> bool {
        let lines = plan
            .items
            .iter()
            .map(|item| {
                let marker = match item.status {
                    PlanStatus::Pending => "○",
                    PlanStatus::InProgress => "◉",
                    PlanStatus::Completed => "✓",
                };
                format!("{marker} {}", item.step)
            })
            .collect::<Vec<_>>();
        self.output
            .set_tui_plan((!lines.is_empty()).then_some(lines))
    }

    pub fn show_plan(&self, plan: &PlanSnapshot) -> Result<()> {
        if plan.items.is_empty() {
            return self.notice("No plan has been created for this session.");
        }
        if self.sync_plan(plan) {
            return Ok(());
        }
        let mut output = format!("\n{}\n", Style::new().cyan().bold().apply_to("Plan"));
        if let Some(explanation) = &plan.explanation {
            output.push_str(&format!("  {explanation}\n"));
        }
        for item in &plan.items {
            output.push_str(&format!("  [{}] {}\n", item.status.as_str(), item.step));
        }
        output.push('\n');
        self.output.print(output)
    }

    pub fn notice(&self, message: impl AsRef<str>) -> Result<()> {
        if self
            .output
            .tui_entry("NOTICE", message.as_ref(), TuiTone::Dim)
        {
            return Ok(());
        }
        self.output.print(format!("  {}\n", message.as_ref()))
    }

    pub fn warning(&self, message: impl AsRef<str>) -> Result<()> {
        if self
            .output
            .tui_entry("WARNING", message.as_ref(), TuiTone::Warning)
        {
            return Ok(());
        }
        self.output.print(format!(
            "  {} {}\n",
            Style::new().yellow().apply_to("!"),
            message.as_ref()
        ))
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn make_file_private(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

struct FollowUpSubmit {
    requested: Arc<AtomicBool>,
}

impl ConditionalEventHandler for FollowUpSubmit {
    fn handle(
        &self,
        _event: &Event,
        _repeat: RepeatCount,
        _positive: bool,
        _context: &EventContext,
    ) -> Option<Cmd> {
        self.requested.store(true, Ordering::Release);
        Some(Cmd::AcceptLine)
    }
}

fn render_welcome(
    config: &Config,
    workspace: &Path,
    session: &SessionSummary,
    instructions: &InstructionSet,
    skill_count: usize,
    command_count: usize,
) -> String {
    let cyan = Style::new().cyan().bold();
    let dim = Style::new().dim();
    let width = (Term::stdout().size().1 as usize).clamp(48, 92);
    let rule = "─".repeat(width.saturating_sub(2));
    let mut output = format!("{}\n", cyan.apply_to(format!("╭{rule}╮")));
    welcome_row(
        &mut output,
        width,
        &format!("WECODE  v{} · coding agent", env!("CARGO_PKG_VERSION")),
    );
    welcome_row(
        &mut output,
        width,
        &format!(
            "model      {} / {}",
            config.model.provider, config.model.model
        ),
    );
    welcome_row(
        &mut output,
        width,
        &format!("workspace  {}", workspace.display()),
    );
    welcome_row(
        &mut output,
        width,
        &format!(
            "session    {} · {}",
            short_id(&session.id),
            session.title.as_deref().unwrap_or("new session")
        ),
    );
    welcome_row(
        &mut output,
        width,
        &format!(
            "protocol   {}",
            protocol_name(config.model.family, config.model.wire_api)
        ),
    );
    welcome_row(
        &mut output,
        width,
        &format!("rules      {} instruction files", instructions.files.len()),
    );
    welcome_row(
        &mut output,
        width,
        &format!("skills     {skill_count} discovered · /skills to inspect"),
    );
    welcome_row(
        &mut output,
        width,
        &format!("commands   {command_count} reusable prompts · /commands to inspect"),
    );
    welcome_row(
        &mut output,
        width,
        "tools      repo · LSP · agents · shell · patch · plan · ask · processes · finish",
    );
    welcome_row(
        &mut output,
        width,
        "input      Enter steer · Alt-Enter follow-up · Ctrl-C cancel",
    );
    output.push_str(&format!("{}\n", cyan.apply_to(format!("╰{rule}╯"))));
    output.push_str(&format!(
        "{}\n",
        dim.apply_to("  Type a task to begin · /help for commands · Ctrl-D to exit\n")
    ));
    output
}

fn welcome_row(output: &mut String, width: usize, content: &str) {
    let cyan = Style::new().cyan().bold();
    let available = width.saturating_sub(5);
    let content = truncate_chars(content, available);
    let padding = " ".repeat(available.saturating_sub(content.chars().count()));
    output.push_str(&format!(
        "{}  {}{} {}\n",
        cyan.apply_to("│"),
        content,
        padding,
        cyan.apply_to("│")
    ));
}

fn truncate_chars(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let mut truncated = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

pub(crate) fn parse_input(line: &str) -> ChatInput {
    if let Some(command) = line.strip_prefix("!!") {
        return ChatInput::Shell {
            command: command.trim_start().to_owned(),
            include_in_context: false,
        };
    }
    if let Some(command) = line.strip_prefix('!') {
        return ChatInput::Shell {
            command: command.trim_start().to_owned(),
            include_in_context: true,
        };
    }
    let (command, argument) = line
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((line, ""));
    match command {
        "/quit" | "/exit" => ChatInput::Exit,
        "/agents" => ChatInput::Command(ChatCommand::Agents),
        "/attach" | "/image" => ChatInput::Command(ChatCommand::Attach(argument.to_owned())),
        "/attachments" | "/files" => ChatInput::Command(ChatCommand::Attachments),
        "/approve" | "/allow" => ChatInput::Command(ChatCommand::Approve),
        "/approve-session" | "/allow-session" | "/always" => {
            ChatInput::Command(ChatCommand::ApproveSession)
        }
        "/cancel" | "/stop" => ChatInput::Command(ChatCommand::Cancel),
        "/checkpoint" | "/mark" => ChatInput::Command(ChatCommand::Checkpoint(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/checkpoints" | "/marks" => ChatInput::Command(ChatCommand::Checkpoints),
        "/clear-queue" => ChatInput::Command(ChatCommand::ClearQueue),
        "/compact" => ChatInput::Command(ChatCommand::Compact(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/commands" => ChatInput::Command(ChatCommand::Commands),
        "/clear" | "/new" => ChatInput::Command(ChatCommand::New),
        "/config" => ChatInput::Command(ChatCommand::Config),
        "/context" => ChatInput::Command(ChatCommand::Context),
        "/deny" | "/reject" => ChatInput::Command(ChatCommand::Deny(argument.to_owned())),
        "/detach" | "/remove-attachment" => {
            ChatInput::Command(ChatCommand::Detach(argument.to_owned()))
        }
        "/diff" => ChatInput::Command(ChatCommand::Diff),
        "/followup" | "/follow-up" | "/later" => ChatInput::FollowUp(argument.to_owned()),
        "/help" | "/?" => ChatInput::Command(ChatCommand::Help),
        "/history" => ChatInput::Command(ChatCommand::History),
        "/hooks" => ChatInput::Command(ChatCommand::Hooks),
        "/lsp" => ChatInput::Command(ChatCommand::Lsp),
        "/lsp-restart" => ChatInput::Command(ChatCommand::LspRestart),
        "/mcp" => ChatInput::Command(ChatCommand::Mcp),
        "/fork" | "/branch" => ChatInput::Command(ChatCommand::Fork(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/queue" => ChatInput::Command(ChatCommand::Queue),
        "/plan" | "/todo" | "/todos" => ChatInput::Command(ChatCommand::Plan),
        "/processes" | "/jobs" => ChatInput::Command(ChatCommand::Processes),
        "/rename" | "/name" => ChatInput::Command(ChatCommand::Rename(argument.to_owned())),
        "/resume" => ChatInput::Command(ChatCommand::Resume(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/rewind" | "/rollback" => ChatInput::Command(ChatCommand::Rewind(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/rules" | "/instructions" => ChatInput::Command(ChatCommand::Rules),
        "/sessions" => ChatInput::Command(ChatCommand::Sessions),
        "/skills" => ChatInput::Command(ChatCommand::Skills),
        "/stop-process" | "/process-stop" => {
            ChatInput::Command(ChatCommand::StopProcess(argument.parse::<u64>().ok()))
        }
        "/stop-agent" | "/agent-stop" => {
            ChatInput::Command(ChatCommand::StopAgent(argument.parse::<u64>().ok()))
        }
        "/steer" => ChatInput::Task(argument.to_owned()),
        "/model" => ChatInput::Command(ChatCommand::Model(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/status" => ChatInput::Command(ChatCommand::Status),
        command if command.starts_with("/skill:") => ChatInput::Command(ChatCommand::Skill {
            name: command.trim_start_matches("/skill:").to_owned(),
            arguments: argument.to_owned(),
        }),
        command if command.starts_with('/') => ChatInput::Command(ChatCommand::Unknown {
            name: command.trim_start_matches('/').to_owned(),
            arguments: argument.to_owned(),
        }),
        _ => ChatInput::Task(line.to_owned()),
    }
}

fn one_line(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&value, 72)
}

fn bounded_middle(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes.saturating_sub(head_budget);
    let mut head = head_budget.min(value.len());
    while head > 0 && !value.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = value.len().saturating_sub(tail_budget);
    while tail < value.len() && !value.is_char_boundary(tail) {
        tail += 1;
    }
    let omitted = tail.saturating_sub(head);
    format!(
        "{}\n\n… {omitted} diff bytes omitted from the middle …\n\n{}",
        &value[..head],
        &value[tail..]
    )
}

fn image_count_label(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(" · {count} image{}", if count == 1 { "" } else { "s" })
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn compact_path(path: &Path) -> String {
    let display = path.display().to_string();
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().display().to_string())
    else {
        return display;
    };
    display
        .strip_prefix(&home)
        .map(|suffix| format!("~{suffix}"))
        .unwrap_or(display)
}

fn compact_number(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        let whole = value / 1_000;
        let decimal = (value % 1_000) / 100;
        return if decimal == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{decimal}k")
        };
    }
    let whole = value / 1_000_000;
    let decimal = (value % 1_000_000) / 100_000;
    if decimal == 0 {
        format!("{whole}m")
    } else {
        format!("{whole}.{decimal}m")
    }
}

fn context_bar(percent: u64, width: usize) -> String {
    let filled = usize::try_from(percent.min(100))
        .unwrap_or(100)
        .saturating_mul(width)
        .saturating_add(50)
        / 100;
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn protocol_name(family: ProviderFamily, wire: WireApi) -> &'static str {
    match family {
        ProviderFamily::Anthropic => "anthropic-messages",
        ProviderFamily::Gemini => "gemini-generate-content",
        ProviderFamily::OpenAiCompatible => match wire {
            WireApi::ChatCompletions => "chat-completions",
            WireApi::Responses => "responses",
        },
    }
}

fn cache_mode_name(mode: CacheMode) -> &'static str {
    match mode {
        CacheMode::Off => "off",
        CacheMode::ReadOnly => "read-only",
        CacheMode::ReadWrite => "read-write",
        CacheMode::Refresh => "refresh",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_commands_and_tasks() {
        assert_eq!(parse_input("/new"), ChatInput::Command(ChatCommand::New));
        assert_eq!(
            parse_input("/resume abc123"),
            ChatInput::Command(ChatCommand::Resume(Some("abc123".into())))
        );
        assert_eq!(
            parse_input("/rename parser cleanup"),
            ChatInput::Command(ChatCommand::Rename("parser cleanup".into()))
        );
        assert_eq!(
            parse_input("/checkpoint before refactor"),
            ChatInput::Command(ChatCommand::Checkpoint(Some("before refactor".into())))
        );
        assert_eq!(
            parse_input("/fork cp-0002"),
            ChatInput::Command(ChatCommand::Fork(Some("cp-0002".into())))
        );
        assert_eq!(
            parse_input("/rewind"),
            ChatInput::Command(ChatCommand::Rewind(None))
        );
        assert_eq!(parse_input("/plan"), ChatInput::Command(ChatCommand::Plan));
        assert_eq!(
            parse_input("/processes"),
            ChatInput::Command(ChatCommand::Processes)
        );
        assert_eq!(
            parse_input("/stop-process 7"),
            ChatInput::Command(ChatCommand::StopProcess(Some(7)))
        );
        assert_eq!(
            parse_input("/stop-process nope"),
            ChatInput::Command(ChatCommand::StopProcess(None))
        );
        assert_eq!(
            parse_input("/agents"),
            ChatInput::Command(ChatCommand::Agents)
        );
        assert_eq!(
            parse_input("/stop-agent 4"),
            ChatInput::Command(ChatCommand::StopAgent(Some(4)))
        );
        assert_eq!(
            parse_input("/stop-agent nope"),
            ChatInput::Command(ChatCommand::StopAgent(None))
        );
        assert_eq!(parse_input("/mcp"), ChatInput::Command(ChatCommand::Mcp));
        assert_eq!(parse_input("/lsp"), ChatInput::Command(ChatCommand::Lsp));
        assert_eq!(
            parse_input("/lsp-restart"),
            ChatInput::Command(ChatCommand::LspRestart)
        );
        assert_eq!(
            parse_input("/hooks"),
            ChatInput::Command(ChatCommand::Hooks)
        );
        assert_eq!(
            parse_input("/commands"),
            ChatInput::Command(ChatCommand::Commands)
        );
        assert_eq!(
            parse_input("/skills"),
            ChatInput::Command(ChatCommand::Skills)
        );
        assert_eq!(
            parse_input("/skill:review focus on safety"),
            ChatInput::Command(ChatCommand::Skill {
                name: "review".into(),
                arguments: "focus on safety".into(),
            })
        );
        assert_eq!(
            parse_input("/model"),
            ChatInput::Command(ChatCommand::Model(None))
        );
        assert_eq!(
            parse_input("/model gpt-fast"),
            ChatInput::Command(ChatCommand::Model(Some("gpt-fast".into())))
        );
        assert_eq!(parse_input("/diff"), ChatInput::Command(ChatCommand::Diff));
        assert_eq!(
            parse_input("/context"),
            ChatInput::Command(ChatCommand::Context)
        );
        assert_eq!(
            parse_input("/compact"),
            ChatInput::Command(ChatCommand::Compact(None))
        );
        assert_eq!(
            parse_input("/compact preserve the failing test"),
            ChatInput::Command(ChatCommand::Compact(Some(
                "preserve the failing test".into()
            )))
        );
        assert_eq!(
            parse_input("! cargo test"),
            ChatInput::Shell {
                command: "cargo test".into(),
                include_in_context: true,
            }
        );
        assert_eq!(
            parse_input("!! printenv"),
            ChatInput::Shell {
                command: "printenv".into(),
                include_in_context: false,
            }
        );
        assert_eq!(
            parse_input("/review src \"error paths\""),
            ChatInput::Command(ChatCommand::Unknown {
                name: "review".into(),
                arguments: "src \"error paths\"".into(),
            })
        );
        assert_eq!(
            parse_input("/followup run tests"),
            ChatInput::FollowUp("run tests".into())
        );
        assert_eq!(
            parse_input("/steer do not change the API"),
            ChatInput::Task("do not change the API".into())
        );
        assert_eq!(
            parse_input("/queue"),
            ChatInput::Command(ChatCommand::Queue)
        );
        assert_eq!(
            parse_input("/cancel"),
            ChatInput::Command(ChatCommand::Cancel)
        );
        assert_eq!(
            parse_input("/approve-session"),
            ChatInput::Command(ChatCommand::ApproveSession)
        );
        assert_eq!(
            parse_input("/deny no network"),
            ChatInput::Command(ChatCommand::Deny("no network".into()))
        );
        assert_eq!(
            parse_input("/attach \"screenshots/error state.png\""),
            ChatInput::Command(ChatCommand::Attach(
                "\"screenshots/error state.png\"".into()
            ))
        );
        assert_eq!(
            parse_input("/attachments"),
            ChatInput::Command(ChatCommand::Attachments)
        );
        assert_eq!(
            parse_input("/detach"),
            ChatInput::Command(ChatCommand::Detach(String::new()))
        );
        assert_eq!(
            parse_input("/detach all"),
            ChatInput::Command(ChatCommand::Detach("all".into()))
        );
        assert_eq!(parse_input("/quit"), ChatInput::Exit);
        assert_eq!(
            parse_input("fix the parser"),
            ChatInput::Task("fix the parser".into())
        );
    }

    #[test]
    fn bounded_middle_preserves_utf8_edges() {
        let value = format!("{}{}", "你".repeat(100), "好".repeat(100));
        let bounded = bounded_middle(&value, 64);
        assert!(bounded.starts_with('你'));
        assert!(bounded.ends_with('好'));
        assert!(bounded.contains("diff bytes omitted"));
        assert!(bounded.len() < value.len());
    }

    #[cfg(unix)]
    #[test]
    fn history_storage_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("wecode");
        let history = directory.join("history");
        create_private_directory(&directory).unwrap();
        std::fs::write(&history, "fix the parser\n").unwrap();
        make_file_private(&history).unwrap();

        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(history).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn completion_file_index_respects_ignores_and_includes_hidden_project_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join(".github/workflows")).unwrap();
        std::fs::create_dir_all(temp.path().join("target/debug")).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(temp.path().join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(temp.path().join("target/debug/build"), "ignored\n").unwrap();

        assert_eq!(
            completion_files(temp.path()),
            [".github/workflows/ci.yml", ".gitignore", "src/main.rs"]
        );
    }
}
