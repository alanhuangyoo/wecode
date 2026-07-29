use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use console::{Style, Term};
use rustyline::{DefaultEditor, error::ReadlineError};

use crate::config::{
    CacheMode, Config, ProviderFamily, WireApi, default_config_path, default_history_path,
};
use crate::instructions::InstructionSet;
use crate::session::SessionSummary;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatCommand {
    Config,
    Help,
    History,
    New,
    Rename(String),
    Resume(Option<String>),
    Rules,
    Sessions,
    Status,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChatInput {
    Task(String),
    Command(ChatCommand),
    Interrupted,
    Exit,
}

pub struct ChatShell {
    editor: Option<DefaultEditor>,
    history_path: PathBuf,
}

impl ChatShell {
    pub fn new() -> Result<Self> {
        let history_path = default_history_path();
        if let Some(parent) = history_path.parent() {
            create_private_directory(parent)?;
        }
        let editor = if io::stdin().is_terminal() && io::stdout().is_terminal() {
            let mut editor = DefaultEditor::new()?;
            if history_path.is_file() {
                let _ = editor.load_history(&history_path);
            }
            Some(editor)
        } else {
            None
        };
        Ok(Self {
            editor,
            history_path,
        })
    }

    pub fn welcome(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
    ) {
        let cyan = Style::new().cyan().bold();
        let dim = Style::new().dim();
        let width = (Term::stdout().size().1 as usize).clamp(48, 92);
        let rule = "─".repeat(width.saturating_sub(2));
        println!("{}", cyan.apply_to(format!("╭{rule}╮")));
        welcome_row(
            width,
            &format!("WECODE  v{} · coding agent", env!("CARGO_PKG_VERSION")),
        );
        welcome_row(
            width,
            &format!(
                "model      {} / {}",
                config.model.provider, config.model.model
            ),
        );
        welcome_row(width, &format!("workspace  {}", workspace.display()));
        welcome_row(
            width,
            &format!(
                "session    {} · {}",
                short_id(&session.id),
                session.title.as_deref().unwrap_or("new session")
            ),
        );
        welcome_row(
            width,
            &format!(
                "protocol   {}",
                protocol_name(config.model.family, config.model.wire_api)
            ),
        );
        welcome_row(
            width,
            &format!("rules      {} instruction files", instructions.files.len()),
        );
        welcome_row(width, "tools      shell · apply_patch · finish");
        println!("{}", cyan.apply_to(format!("╰{rule}╯")));
        println!(
            "{}",
            dim.apply_to("  Type a task to begin · /help for commands · Ctrl-D to exit\n")
        );
    }

    pub fn read_input(&mut self) -> Result<ChatInput> {
        println!("╭─ {}", Style::new().blue().bold().apply_to("You"));
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
        Ok(parse_input(line))
    }

    pub fn clear_screen(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
    ) -> Result<()> {
        Term::stdout().clear_screen()?;
        self.welcome(config, workspace, session, instructions);
        Ok(())
    }

    pub fn show_help(&self) {
        println!(
            "\n{}\n\
             \n  {:12} Start a fresh conversation\
             \n  {:12} Resume the latest or selected session\
             \n  {:12} List recent sessions for this workspace\
             \n  {:12} Rename the current session\
             \n  {:12} Show model, workspace, cache, and context\
             \n  {:12} Show loaded project instruction files\
             \n  {:12} Show the active config path\
             \n  {:12} Show the history file\
             \n  {:12} Show this help\
             \n  {:12} Exit WeCode\n",
            Style::new().cyan().bold().apply_to("Commands"),
            "/new",
            "/resume [id]",
            "/sessions",
            "/rename <name>",
            "/status",
            "/rules",
            "/config",
            "/history",
            "/help",
            "/quit",
        );
    }

    pub fn show_status(
        &self,
        config: &Config,
        workspace: &Path,
        session: &SessionSummary,
        instructions: &InstructionSet,
        context_messages: usize,
    ) {
        println!(
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
        );
    }

    pub fn show_rules(&self, instructions: &InstructionSet) {
        println!("\n{}", Style::new().cyan().bold().apply_to("Project rules"));
        if instructions.files.is_empty() {
            println!("  No AGENTS.md, CLAUDE.md, or rules files loaded.\n");
            return;
        }
        for file in &instructions.files {
            let suffix = if file.truncated { " (truncated)" } else { "" };
            println!(
                "  {}  ~{} tokens{suffix}",
                file.path.display(),
                xai_token_estimation::estimate_tokens(&file.content)
            );
        }
        println!();
    }

    pub fn show_sessions(&self, sessions: &[SessionSummary]) {
        println!(
            "\n{}",
            Style::new().cyan().bold().apply_to("Recent sessions")
        );
        if sessions.is_empty() {
            println!("  No saved sessions for this workspace.\n");
            return;
        }
        for session in sessions.iter().take(20) {
            println!(
                "  {:10} {:>4} messages  {}",
                short_id(&session.id),
                session.message_count,
                session.title.as_deref().unwrap_or("untitled"),
            );
        }
        println!("\n  Resume with /resume <id>.\n");
    }

    pub fn show_config_path(&self) {
        println!(
            "\n{} {}\n",
            Style::new().cyan().bold().apply_to("Config"),
            default_config_path().display(),
        );
    }

    pub fn show_history_path(&self) {
        println!(
            "\n{} {}\n",
            Style::new().cyan().bold().apply_to("History"),
            self.history_path.display(),
        );
    }

    pub fn show_setup_required(&self, error: &anyhow::Error) {
        println!(
            "\n{}\n  {}\n\n  Run {} to configure a provider and store its key safely.\n",
            Style::new().yellow().bold().apply_to("Setup required"),
            error,
            Style::new().cyan().bold().apply_to("wecode setup"),
        );
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

fn welcome_row(width: usize, content: &str) {
    let cyan = Style::new().cyan().bold();
    let available = width.saturating_sub(5);
    let content = truncate_chars(content, available);
    let padding = " ".repeat(available.saturating_sub(content.chars().count()));
    println!(
        "{}  {}{} {}",
        cyan.apply_to("│"),
        content,
        padding,
        cyan.apply_to("│")
    );
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
        "/clear" | "/new" => ChatInput::Command(ChatCommand::New),
        "/config" => ChatInput::Command(ChatCommand::Config),
        "/help" | "/?" => ChatInput::Command(ChatCommand::Help),
        "/history" => ChatInput::Command(ChatCommand::History),
        "/rename" | "/name" => ChatInput::Command(ChatCommand::Rename(argument.to_owned())),
        "/resume" => ChatInput::Command(ChatCommand::Resume(
            (!argument.is_empty()).then(|| argument.to_owned()),
        )),
        "/rules" | "/instructions" => ChatInput::Command(ChatCommand::Rules),
        "/sessions" => ChatInput::Command(ChatCommand::Sessions),
        "/model" | "/status" => ChatInput::Command(ChatCommand::Status),
        command if command.starts_with('/') => {
            eprintln!(
                "{} Unknown command {command:?}. Type /help.",
                Style::new().yellow().apply_to("!")
            );
            ChatInput::Interrupted
        }
        _ => ChatInput::Task(line.to_owned()),
    }
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
