use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::Result;
use console::{Style, Term};
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler, KeyCode,
    KeyEvent, Modifiers, RepeatCount, error::ReadlineError,
};
use tokio::sync::mpsc;

use crate::approval::ApprovalRequest;
use crate::config::{
    CacheMode, Config, ProviderFamily, WireApi, default_config_path, default_history_path,
};
use crate::input_queue::QueueSnapshot;
use crate::instructions::InstructionSet;
use crate::session::SessionSummary;
use crate::ui::TerminalOutput;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatCommand {
    Approve,
    ApproveSession,
    Cancel,
    ClearQueue,
    Config,
    Deny(String),
    Help,
    History,
    New,
    Rename(String),
    Resume(Option<String>),
    Rules,
    Sessions,
    Status,
    Queue,
    Unknown(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChatInput {
    Task(String),
    FollowUp(String),
    Command(ChatCommand),
    Interrupted,
    Exit,
}

pub struct ChatShell {
    editor: Option<DefaultEditor>,
    follow_up_submit: Arc<AtomicBool>,
    history_path: PathBuf,
    output: TerminalOutput,
}

#[derive(Clone)]
pub struct ChatView {
    history_path: PathBuf,
    output: TerminalOutput,
}

impl ChatShell {
    pub fn new() -> Result<Self> {
        let history_path = default_history_path();
        if let Some(parent) = history_path.parent() {
            create_private_directory(parent)?;
        }
        let follow_up_submit = Arc::new(AtomicBool::new(false));
        let mut editor = if io::stdin().is_terminal() && io::stdout().is_terminal() {
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
        let output = match editor.as_mut() {
            Some(editor) => match editor.create_external_printer() {
                Ok(printer) => TerminalOutput::external(Box::new(printer)),
                Err(_) => TerminalOutput::stdout(),
            },
            None => TerminalOutput::stdout(),
        };
        Ok(Self {
            editor,
            follow_up_submit,
            history_path,
            output,
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
        if let Some(editor) = self.editor.as_mut() {
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
    ) -> Result<()> {
        self.output
            .print(render_welcome(config, workspace, session, instructions))
    }

    pub fn clear_screen(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
    ) -> Result<()> {
        self.output.print(format!(
            "\x1b[2J\x1b[H]{}",
            render_welcome(config, workspace, session, instructions)
        ))
    }

    pub fn show_help(&self) -> Result<()> {
        self.output.print(format!(
            "\n{}\n\
             \n  {:12} Start a fresh conversation\
             \n  {:12} Resume the latest or selected session\
             \n  {:12} List recent sessions for this workspace\
             \n  {:12} Rename the current session\
             \n  {:12} Steer the active task at the next model boundary\
             \n  {:12} Queue work for after the active task\
             \n  {:12} Show pending steer and follow-up messages\
             \n  {:12} Clear all pending messages\
             \n  {:12} Cancel the active model request or command\
             \n  {:12} Allow a pending action once\
             \n  {:12} Allow matching actions for this session\
             \n  {:12} Deny a pending action with optional feedback\
             \n  {:12} Show model, workspace, cache, and context\
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
            "/steer <text>",
            "/followup <text>",
            "/queue",
            "/clear-queue",
            "/cancel",
            "/approve",
            "/approve-session",
            "/deny [reason]",
            "/status",
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
            "\n{}\n  id         {}\n  title      {}\n  provider   {}\n  model      {}\n  protocol   {}\n  workspace  {}\n  rules      {} files\n  cache      {}\n  context    {} messages\n  file       {}\n",
            Style::new().cyan().bold().apply_to("Session"),
            session.id,
            session.title.as_deref().unwrap_or("untitled"),
            config.model.provider,
            config.model.model,
            protocol_name(config.model.family, config.model.wire_api),
            workspace.display(),
            instructions.files.len(),
            cache_mode_name(config.cache.mode),
            context_messages,
            session.path.display(),
        ))?;
        if !queue.is_empty() {
            self.show_queue(queue)?;
        }
        Ok(())
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
                "  {:10} {:>4} messages  {}\n",
                short_id(&session.id),
                session.message_count,
                session.title.as_deref().unwrap_or("untitled"),
            ));
        }
        output.push_str("\n  Resume with /resume <id>.\n\n");
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
            output.push_str(&format!(
                "  {} #{:<3} {}\n",
                Style::new().cyan().apply_to("steer"),
                input.id,
                one_line(&input.text)
            ));
        }
        for input in &queue.follow_ups {
            output.push_str(&format!(
                "  {} #{:<3} {}\n",
                Style::new().magenta().apply_to("follow-up"),
                input.id,
                one_line(&input.text)
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

    pub fn notice(&self, message: impl AsRef<str>) -> Result<()> {
        self.output.print(format!("  {}\n", message.as_ref()))
    }

    pub fn warning(&self, message: impl AsRef<str>) -> Result<()> {
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
        "tools      shell · apply_patch · finish",
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

fn parse_input(line: &str) -> ChatInput {
    let (command, argument) = line
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((line, ""));
    match command {
        "/quit" | "/exit" => ChatInput::Exit,
        "/approve" | "/allow" => ChatInput::Command(ChatCommand::Approve),
        "/approve-session" | "/allow-session" | "/always" => {
            ChatInput::Command(ChatCommand::ApproveSession)
        }
        "/cancel" | "/stop" => ChatInput::Command(ChatCommand::Cancel),
        "/clear-queue" => ChatInput::Command(ChatCommand::ClearQueue),
        "/clear" | "/new" => ChatInput::Command(ChatCommand::New),
        "/config" => ChatInput::Command(ChatCommand::Config),
        "/deny" | "/reject" => ChatInput::Command(ChatCommand::Deny(argument.to_owned())),
        "/followup" | "/follow-up" | "/later" => ChatInput::FollowUp(argument.to_owned()),
        "/help" | "/?" => ChatInput::Command(ChatCommand::Help),
        "/history" => ChatInput::Command(ChatCommand::History),
        "/queue" => ChatInput::Command(ChatCommand::Queue),
        "/rename" | "/name" => ChatInput::Command(ChatCommand::Rename(argument.to_owned())),
        "/resume" => ChatInput::Command(ChatCommand::Resume(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/rules" | "/instructions" => ChatInput::Command(ChatCommand::Rules),
        "/sessions" => ChatInput::Command(ChatCommand::Sessions),
        "/steer" => ChatInput::Task(argument.to_owned()),
        "/model" | "/status" => ChatInput::Command(ChatCommand::Status),
        command if command.starts_with('/') => {
            ChatInput::Command(ChatCommand::Unknown(command.to_owned()))
        }
        _ => ChatInput::Task(line.to_owned()),
    }
}

fn one_line(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&value, 72)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
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
            parse_input("/model"),
            ChatInput::Command(ChatCommand::Status)
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
        assert_eq!(parse_input("/quit"), ChatInput::Exit);
        assert_eq!(
            parse_input("fix the parser"),
            ChatInput::Task("fix the parser".into())
        );
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
}
