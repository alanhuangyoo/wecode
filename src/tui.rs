use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::{Context, Result};
use console::strip_ansi_codes;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};
use tokio::sync::mpsc as tokio_mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::chat::{ChatInput, parse_input};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandCompletion {
    pub command: String,
    pub description: String,
}

#[derive(Clone)]
pub struct TuiHandle {
    sender: Sender<TuiMessage>,
}

#[derive(Clone, Copy)]
pub enum TuiTone {
    Normal,
    Success,
    Warning,
    Error,
    Dim,
}

pub(crate) enum TuiMessage {
    Append(String),
    Entry {
        label: String,
        text: String,
        tone: TuiTone,
    },
    ToolStart {
        step: usize,
        label: String,
        text: String,
        tone: TuiTone,
    },
    ToolResult {
        step: usize,
        text: String,
        tone: TuiTone,
    },
    Clear,
    Header {
        primary: String,
        secondary: String,
    },
    Welcome {
        model: String,
        workspace: String,
        session: String,
        capabilities: String,
    },
    Metrics(Option<String>),
    Status(Option<String>),
    StreamStart,
    StreamDelta {
        text: String,
        reasoning: bool,
    },
    StreamFinish {
        commit: bool,
    },
    Plan(Option<Vec<String>>),
    Composer {
        title: Option<String>,
        placeholder: Option<String>,
    },
    Attachments(Vec<String>),
    Files(Vec<String>),
}

impl TuiHandle {
    pub(crate) fn new() -> (Self, Receiver<TuiMessage>) {
        let (sender, receiver) = mpsc::channel();
        (Self { sender }, receiver)
    }

    pub fn append(&self, message: String) {
        let _ = self.sender.send(TuiMessage::Append(message));
    }

    pub fn entry(&self, label: String, text: String, tone: TuiTone) {
        let _ = self.sender.send(TuiMessage::Entry { label, text, tone });
    }

    pub fn tool_start(&self, step: usize, label: String, text: String, tone: TuiTone) {
        let _ = self.sender.send(TuiMessage::ToolStart {
            step,
            label,
            text,
            tone,
        });
    }

    pub fn tool_result(&self, step: usize, text: String, tone: TuiTone) {
        let _ = self
            .sender
            .send(TuiMessage::ToolResult { step, text, tone });
    }

    pub fn clear(&self) {
        let _ = self.sender.send(TuiMessage::Clear);
    }

    pub fn set_header(&self, primary: String, secondary: String) {
        let _ = self.sender.send(TuiMessage::Header { primary, secondary });
    }

    pub fn set_welcome(
        &self,
        model: String,
        workspace: String,
        session: String,
        capabilities: String,
    ) {
        let _ = self.sender.send(TuiMessage::Welcome {
            model,
            workspace,
            session,
            capabilities,
        });
    }

    pub fn set_status(&self, status: Option<String>) {
        let _ = self.sender.send(TuiMessage::Status(status));
    }

    pub fn start_stream(&self) {
        let _ = self.sender.send(TuiMessage::StreamStart);
    }

    pub fn stream_delta(&self, text: String, reasoning: bool) {
        let _ = self
            .sender
            .send(TuiMessage::StreamDelta { text, reasoning });
    }

    pub fn finish_stream(&self, commit: bool) {
        let _ = self.sender.send(TuiMessage::StreamFinish { commit });
    }

    pub fn set_metrics(&self, metrics: Option<String>) {
        let _ = self.sender.send(TuiMessage::Metrics(metrics));
    }

    pub fn set_plan(&self, plan: Option<Vec<String>>) {
        let _ = self.sender.send(TuiMessage::Plan(plan));
    }

    pub fn set_composer(&self, title: Option<String>, placeholder: Option<String>) {
        let _ = self
            .sender
            .send(TuiMessage::Composer { title, placeholder });
    }

    pub fn set_attachments(&self, attachments: Vec<String>) {
        let _ = self.sender.send(TuiMessage::Attachments(attachments));
    }

    pub fn set_files(&self, files: Vec<String>) {
        let _ = self.sender.send(TuiMessage::Files(files));
    }
}

pub fn supported() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        && std::env::var("WECODE_TUI").map_or(true, |value| value != "0")
}

pub(crate) fn run(
    receiver: Receiver<TuiMessage>,
    inputs: tokio_mpsc::UnboundedSender<Result<ChatInput>>,
    history_path: PathBuf,
    completions: Vec<CommandCompletion>,
    models: Vec<String>,
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let history = load_history(&history_path);
    let mut state = TuiState::new(history, completions);
    state.models = models;
    let mut redraw = true;

    loop {
        match drain_messages(&receiver, &mut state) {
            MessageState::Stop => break,
            MessageState::Changed => redraw = true,
            MessageState::Idle => {}
        }

        if redraw {
            terminal
                .terminal
                .draw(|frame| draw(frame, &mut state))
                .context("failed to render terminal UI")?;
            redraw = false;
        }

        if !event::poll(Duration::from_millis(70))? {
            if state.status.is_some() {
                state.tick = state.tick.wrapping_add(1);
                redraw = true;
            }
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let outcome = state.handle_key(key);
                redraw = outcome.changed;
                if let Some(input) = outcome.input {
                    match &input {
                        ChatInput::Task(text) | ChatInput::FollowUp(text) => {
                            save_history_entry(&history_path, text);
                            state.history.push(text.clone());
                            state.history_index = None;
                            state.append_user(text);
                        }
                        ChatInput::Shell {
                            command,
                            include_in_context: true,
                        } => {
                            let entry = format!("!{command}");
                            save_history_entry(&history_path, &entry);
                            state.history.push(entry);
                            state.history_index = None;
                        }
                        _ => {}
                    }
                    let done = matches!(input, ChatInput::Exit);
                    if inputs.send(Ok(input)).is_err() || done {
                        break;
                    }
                }
            }
            Event::Paste(text) => {
                if crate::attachments::looks_like_image_path(&text) {
                    let path = crate::attachments::normalized_path_text(&text).to_owned();
                    let _ = inputs.send(Ok(ChatInput::Command(crate::chat::ChatCommand::Attach(
                        path,
                    ))));
                } else {
                    state.composer.insert(&text);
                    redraw = true;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    state.scroll = state.scroll.saturating_add(3);
                    redraw = true;
                }
                MouseEventKind::ScrollDown => {
                    state.scroll = state.scroll.saturating_sub(3);
                    redraw = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => redraw = true,
            _ => {}
        }
    }
    Ok(())
}

enum MessageState {
    Idle,
    Changed,
    Stop,
}

fn drain_messages(receiver: &Receiver<TuiMessage>, state: &mut TuiState) -> MessageState {
    let mut changed = false;
    loop {
        match receiver.try_recv() {
            Ok(TuiMessage::Append(message)) => {
                state.append_output(&message);
                changed = true;
            }
            Ok(TuiMessage::Entry { label, text, tone }) => {
                state.append_entry(label, text, tone);
                changed = true;
            }
            Ok(TuiMessage::ToolStart {
                step,
                label,
                text,
                tone,
            }) => {
                state.append_tool(step, label, text, tone);
                changed = true;
            }
            Ok(TuiMessage::ToolResult { step, text, tone }) => {
                state.finish_tool(step, text, tone);
                changed = true;
            }
            Ok(TuiMessage::Clear) => {
                state.transcript.clear();
                state.live_response = None;
                state.metrics = None;
                state.scroll = 0;
                state.status = None;
                state.plan = None;
                state.composer_title = None;
                state.composer_placeholder = None;
                state.attachments.clear();
                changed = true;
            }
            Ok(TuiMessage::Header { primary, secondary }) => {
                state.header_primary = primary;
                state.header_secondary = secondary;
                changed = true;
            }
            Ok(TuiMessage::Welcome {
                model,
                workspace,
                session,
                capabilities,
            }) => {
                state.welcome = Some(WelcomeCard {
                    model,
                    workspace,
                    session,
                    capabilities,
                });
                changed = true;
            }
            Ok(TuiMessage::Metrics(metrics)) => {
                state.metrics = metrics;
                changed = true;
            }
            Ok(TuiMessage::Status(status)) => {
                state.status = status;
                changed = true;
            }
            Ok(TuiMessage::StreamStart) => {
                state.live_response = Some(LiveResponse::default());
                changed = true;
            }
            Ok(TuiMessage::StreamDelta { text, reasoning }) => {
                let response = state
                    .live_response
                    .get_or_insert_with(LiveResponse::default);
                if reasoning {
                    response.reasoning.push_str(&text);
                } else {
                    response.text.push_str(&text);
                }
                changed = true;
            }
            Ok(TuiMessage::StreamFinish { commit }) => {
                if let Some(response) = state.live_response.take()
                    && commit
                    && !response.text.trim().is_empty()
                    && !looks_like_internal_protocol(&response.text)
                {
                    state.transcript.push(TranscriptEntry {
                        kind: TranscriptKind::Agent,
                        text: response.text.trim().to_owned(),
                    });
                    state.scroll = 0;
                }
                changed = true;
            }
            Ok(TuiMessage::Plan(plan)) => {
                state.plan = plan;
                changed = true;
            }
            Ok(TuiMessage::Composer { title, placeholder }) => {
                state.composer_title = title;
                state.composer_placeholder = placeholder;
                changed = true;
            }
            Ok(TuiMessage::Attachments(attachments)) => {
                state.attachments = attachments;
                changed = true;
            }
            Ok(TuiMessage::Files(files)) => {
                state.files = files;
                changed = true;
            }
            Err(TryRecvError::Disconnected) => {
                return MessageState::Stop;
            }
            Err(TryRecvError::Empty) => {
                return if changed {
                    MessageState::Changed
                } else {
                    MessageState::Idle
                };
            }
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        ) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone)]
enum TranscriptKind {
    User,
    Agent,
    Custom {
        label: String,
        tone: TuiTone,
    },
    Tool {
        step: usize,
        label: String,
        tone: TuiTone,
        result: Option<(String, TuiTone)>,
    },
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
}

#[derive(Clone)]
struct WelcomeCard {
    model: String,
    workspace: String,
    session: String,
    capabilities: String,
}

#[derive(Default)]
struct LiveResponse {
    reasoning: String,
    text: String,
}

struct TuiState {
    attachments: Vec<String>,
    composer: Composer,
    composer_placeholder: Option<String>,
    composer_title: Option<String>,
    completion_selected: usize,
    completions: Vec<CommandCompletion>,
    files: Vec<String>,
    header_primary: String,
    header_secondary: String,
    history: Vec<String>,
    history_index: Option<usize>,
    live_response: Option<LiveResponse>,
    metrics: Option<String>,
    models: Vec<String>,
    plan: Option<Vec<String>>,
    scroll: u16,
    status: Option<String>,
    tick: usize,
    transcript: Vec<TranscriptEntry>,
    welcome: Option<WelcomeCard>,
}

impl TuiState {
    fn new(history: Vec<String>, completions: Vec<CommandCompletion>) -> Self {
        Self {
            attachments: Vec::new(),
            composer: Composer::default(),
            composer_placeholder: None,
            composer_title: None,
            completion_selected: 0,
            completions,
            files: Vec::new(),
            header_primary: format!("WeCode {}", env!("CARGO_PKG_VERSION")),
            header_secondary: "Lightweight coding agent".into(),
            history,
            history_index: None,
            live_response: None,
            metrics: None,
            models: Vec::new(),
            plan: None,
            scroll: 0,
            status: None,
            tick: 0,
            transcript: Vec::new(),
            welcome: None,
        }
    }

    fn append_output(&mut self, message: &str) {
        let clean = strip_ansi_codes(message).trim().to_owned();
        if clean.is_empty() {
            return;
        }
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Agent,
            text: clean,
        });
        self.scroll = 0;
    }

    fn append_user(&mut self, message: &str) {
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::User,
            text: message.trim().to_owned(),
        });
        self.scroll = 0;
    }

    fn append_entry(&mut self, label: String, text: String, tone: TuiTone) {
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Custom { label, tone },
            text: strip_ansi_codes(&text).trim().to_owned(),
        });
        self.scroll = 0;
    }

    fn append_tool(&mut self, step: usize, label: String, text: String, tone: TuiTone) {
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Tool {
                step,
                label,
                tone,
                result: None,
            },
            text: strip_ansi_codes(&text).trim().to_owned(),
        });
        self.scroll = 0;
    }

    fn finish_tool(&mut self, step: usize, text: String, tone: TuiTone) {
        let text = strip_ansi_codes(&text).trim().to_owned();
        if let Some(entry) = self.transcript.iter_mut().find(|entry| {
            matches!(
                &entry.kind,
                TranscriptKind::Tool {
                    step: tool_step,
                    result: None,
                    ..
                } if *tool_step == step
            )
        }) && let TranscriptKind::Tool { result, .. } = &mut entry.kind
        {
            *result = Some((text, tone));
        } else {
            self.append_entry("TOOL".into(), text, tone);
        }
        self.scroll = 0;
    }

    fn is_shell_mode(&self) -> bool {
        self.composer.text.trim_start().starts_with('!')
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let modifiers = key.modifiers;
        if modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.composer.is_empty() {
                        return KeyOutcome::input(ChatInput::Interrupted);
                    }
                    self.composer.clear();
                    return KeyOutcome::changed();
                }
                KeyCode::Char('d') if self.composer.is_empty() => {
                    return KeyOutcome::input(ChatInput::Exit);
                }
                KeyCode::Char('j') => {
                    self.composer.insert("\n");
                    return KeyOutcome::changed();
                }
                KeyCode::Char('a') => {
                    self.composer.home();
                    return KeyOutcome::changed();
                }
                KeyCode::Char('e') => {
                    self.composer.end();
                    return KeyOutcome::changed();
                }
                KeyCode::Char('u') => {
                    self.composer.clear();
                    return KeyOutcome::changed();
                }
                KeyCode::Char('l') => {
                    self.transcript.clear();
                    self.scroll = 0;
                    return KeyOutcome::changed();
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter => {
                if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) {
                    self.composer.insert("\n");
                    return KeyOutcome::changed();
                }
                if self.apply_completion(false) {
                    return KeyOutcome::changed();
                }
                let text = self.composer.take();
                if text.trim().is_empty() {
                    if self.attachments.is_empty() {
                        return KeyOutcome::changed();
                    }
                    let prompt = "Inspect the attached file or image.".to_owned();
                    return if modifiers.contains(KeyModifiers::ALT) {
                        KeyOutcome::input(ChatInput::FollowUp(prompt))
                    } else {
                        KeyOutcome::input(ChatInput::Task(prompt))
                    };
                }
                let parsed = parse_input(text.trim());
                if matches!(parsed, ChatInput::Shell { .. }) {
                    KeyOutcome::input(parsed)
                } else if modifiers.contains(KeyModifiers::ALT) {
                    KeyOutcome::input(ChatInput::FollowUp(text))
                } else {
                    KeyOutcome::input(parsed)
                }
            }
            KeyCode::Char(character) => {
                self.composer.insert_char(character);
                self.completion_selected = 0;
                KeyOutcome::changed()
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                self.completion_selected = 0;
                KeyOutcome::changed()
            }
            KeyCode::Delete => {
                self.composer.delete();
                KeyOutcome::changed()
            }
            KeyCode::Left => {
                if modifiers.contains(KeyModifiers::ALT) {
                    self.composer.previous_word();
                } else {
                    self.composer.left();
                }
                KeyOutcome::changed()
            }
            KeyCode::Right => {
                if modifiers.contains(KeyModifiers::ALT) {
                    self.composer.next_word();
                } else {
                    self.composer.right();
                }
                KeyOutcome::changed()
            }
            KeyCode::Home => {
                self.composer.home();
                KeyOutcome::changed()
            }
            KeyCode::End => {
                self.composer.end();
                KeyOutcome::changed()
            }
            KeyCode::Tab => {
                if self.apply_completion(true) {
                    KeyOutcome::changed()
                } else {
                    KeyOutcome::unchanged()
                }
            }
            KeyCode::BackTab => {
                let count = self.active_completion_count();
                if count > 0 {
                    self.completion_selected =
                        self.completion_selected.checked_sub(1).unwrap_or(count - 1);
                    KeyOutcome::changed()
                } else {
                    KeyOutcome::unchanged()
                }
            }
            KeyCode::Up if self.active_completion_count() > 0 => {
                let count = self.active_completion_count();
                self.completion_selected =
                    self.completion_selected.checked_sub(1).unwrap_or(count - 1);
                KeyOutcome::changed()
            }
            KeyCode::Down if self.active_completion_count() > 0 => {
                let count = self.active_completion_count();
                self.completion_selected = (self.completion_selected + 1) % count;
                KeyOutcome::changed()
            }
            KeyCode::Up if !self.composer.text.contains('\n') => {
                self.recall_history(true);
                KeyOutcome::changed()
            }
            KeyCode::Down if !self.composer.text.contains('\n') => {
                self.recall_history(false);
                KeyOutcome::changed()
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(8);
                KeyOutcome::changed()
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(8);
                KeyOutcome::changed()
            }
            KeyCode::Esc => {
                self.composer.clear();
                KeyOutcome::changed()
            }
            _ => KeyOutcome::unchanged(),
        }
    }

    fn recall_history(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_index, older) {
            (None, true) => Some(self.history.len() - 1),
            (Some(0), true) => Some(0),
            (Some(index), true) => Some(index - 1),
            (Some(index), false) if index + 1 < self.history.len() => Some(index + 1),
            (Some(_), false) => None,
            (None, false) => None,
        };
        self.history_index = next;
        self.composer.set(
            next.and_then(|index| self.history.get(index).cloned())
                .unwrap_or_default(),
        );
    }

    fn active_command_completions(&self) -> Vec<&CommandCompletion> {
        if self.is_shell_mode()
            || self.active_file_query().is_some()
            || self.active_model_query().is_some()
        {
            return Vec::new();
        }
        let value = self.composer.text.trim_start();
        if !value.starts_with('/') || value.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        self.completions
            .iter()
            .filter(|completion| completion.command.starts_with(value))
            .take(8)
            .collect()
    }

    fn active_model_query(&self) -> Option<String> {
        if self.is_shell_mode() {
            return None;
        }
        let value = self.composer.text.trim_start();
        let query = value.strip_prefix("/model")?;
        if query.is_empty() || !query.starts_with(char::is_whitespace) || query.contains('\n') {
            return None;
        }
        Some(query.trim().to_ascii_lowercase())
    }

    fn active_model_completions(&self) -> Vec<&String> {
        let Some(query) = self.active_model_query() else {
            return Vec::new();
        };
        let mut matches = self
            .models
            .iter()
            .filter_map(|model| file_match_score(model, &query).map(|score| (score, model)))
            .collect::<Vec<_>>();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.len().cmp(&right.len()))
                .then_with(|| left.cmp(right))
        });
        matches
            .into_iter()
            .take(8)
            .map(|(_, model)| model)
            .collect()
    }

    fn active_file_query(&self) -> Option<(usize, usize, String)> {
        if self.is_shell_mode() {
            return None;
        }
        let cursor_byte = char_to_byte(&self.composer.text, self.composer.cursor);
        let before = &self.composer.text[..cursor_byte];

        if let Some(start_byte) = before.rfind("@\"")
            && crate::attachments::starts_file_mention(before, start_byte)
        {
            let query_start = start_byte + 2;
            if !before[query_start..].contains('"') {
                return Some((
                    before[..start_byte].chars().count(),
                    self.composer.cursor,
                    before[query_start..].to_owned(),
                ));
            }
        }

        let start_byte = before.char_indices().rev().find_map(|(index, character)| {
            (character == '@' && crate::attachments::starts_file_mention(before, index))
                .then_some(index)
        })?;
        let query = &before[start_byte + 1..];
        (!query.chars().any(char::is_whitespace) && !query.contains('"')).then(|| {
            (
                before[..start_byte].chars().count(),
                self.composer.cursor,
                query.to_owned(),
            )
        })
    }

    fn active_file_completions(&self) -> Vec<&String> {
        let Some((_, _, query)) = self.active_file_query() else {
            return Vec::new();
        };
        let query = query.to_ascii_lowercase().replace('\\', "/");
        let mut matches = self
            .files
            .iter()
            .filter_map(|path| file_match_score(path, &query).map(|score| (score, path)))
            .collect::<Vec<_>>();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.len().cmp(&right.len()))
                .then_with(|| left.cmp(right))
        });
        matches.into_iter().take(8).map(|(_, path)| path).collect()
    }

    fn active_completion_count(&self) -> usize {
        let models = self.active_model_completions();
        if !models.is_empty() {
            return models.len();
        }
        let files = self.active_file_completions();
        if files.is_empty() {
            self.active_command_completions().len()
        } else {
            files.len()
        }
    }

    fn apply_completion(&mut self, force: bool) -> bool {
        let model = self
            .active_model_completions()
            .get(self.completion_selected)
            .map(|model| (*model).clone());
        if let Some(model) = model {
            if !force && self.composer.text.trim() == format!("/model {model}") {
                return false;
            }
            self.composer.set(format!("/model {model}"));
            self.completion_selected = 0;
            return true;
        }

        let file = self
            .active_file_completions()
            .get(self.completion_selected)
            .map(|path| (*path).clone());
        if let Some(file) = file
            && let Some((start, end, _)) = self.active_file_query()
        {
            let mention = if file.chars().any(char::is_whitespace) {
                format!("@\"{file}\" ")
            } else {
                format!("@{file} ")
            };
            self.composer.replace(start, end, &mention);
            self.completion_selected = 0;
            return true;
        }

        let selected = self
            .active_command_completions()
            .get(self.completion_selected)
            .map(|completion| completion.command.clone());
        let Some(command) = selected else {
            return false;
        };
        if !force && self.composer.text.trim() == command {
            return false;
        }
        self.composer.set(format!("{command} "));
        self.completion_selected = 0;
        true
    }
}

fn file_match_score(path: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(10_000_i64.saturating_sub(path.len() as i64));
    }
    let path = path.to_ascii_lowercase();
    let basename = path.rsplit('/').next().unwrap_or(&path);
    if path.starts_with(query) {
        return Some(50_000_i64.saturating_sub(path.len() as i64));
    }
    if basename.starts_with(query) {
        return Some(40_000_i64.saturating_sub(path.len() as i64));
    }
    if let Some(index) = path.find(query) {
        return Some(
            30_000_i64
                .saturating_sub(index as i64 * 10)
                .saturating_sub(path.len() as i64),
        );
    }

    let mut score = 20_000_i64;
    let mut query = query.chars();
    let mut wanted = query.next()?;
    let mut previous_match = None;
    for (index, character) in path.chars().enumerate() {
        if character != wanted {
            continue;
        }
        score = score.saturating_sub(index as i64);
        if previous_match.is_some_and(|previous| previous + 1 == index) {
            score = score.saturating_add(100);
        }
        previous_match = Some(index);
        let Some(next) = query.next() else {
            return Some(score.saturating_sub(path.len() as i64));
        };
        wanted = next;
    }
    None
}

struct KeyOutcome {
    changed: bool,
    input: Option<ChatInput>,
}

impl KeyOutcome {
    fn unchanged() -> Self {
        Self {
            changed: false,
            input: None,
        }
    }

    fn changed() -> Self {
        Self {
            changed: true,
            input: None,
        }
    }

    fn input(input: ChatInput) -> Self {
        Self {
            changed: true,
            input: Some(input),
        }
    }
}

#[derive(Default)]
struct Composer {
    cursor: usize,
    text: String,
}

impl Composer {
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn set(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.chars().count();
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    fn replace(&mut self, start: usize, end: usize, value: &str) {
        let start_byte = char_to_byte(&self.text, start);
        let end_byte = char_to_byte(&self.text, end);
        self.text.replace_range(start_byte..end_byte, value);
        self.cursor = start.saturating_add(value.chars().count());
    }

    fn insert(&mut self, value: &str) {
        let byte = char_to_byte(&self.text, self.cursor);
        self.text.insert_str(byte, value);
        self.cursor += value.chars().count();
    }

    fn insert_char(&mut self, value: char) {
        let byte = char_to_byte(&self.text, self.cursor);
        self.text.insert(byte, value);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_to_byte(&self.text, self.cursor - 1);
        let end = char_to_byte(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let start = char_to_byte(&self.text, self.cursor);
        let end = char_to_byte(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    fn previous_word(&mut self) {
        let characters = self.text.chars().collect::<Vec<_>>();
        while self.cursor > 0 && characters[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !characters[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    fn next_word(&mut self) {
        let characters = self.text.chars().collect::<Vec<_>>();
        while self.cursor < characters.len() && !characters[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < characters.len() && characters[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        let before = self.text.chars().take(self.cursor).collect::<String>();
        self.cursor -= before
            .chars()
            .rev()
            .take_while(|character| *character != '\n')
            .count();
    }

    fn end(&mut self) {
        self.cursor += self
            .text
            .chars()
            .skip(self.cursor)
            .take_while(|character| *character != '\n')
            .count();
    }
}

fn char_to_byte(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &mut TuiState) {
    let area = frame.area();
    let composer_height = composer_height(&state.composer.text, area.width);
    let completion_count = state.active_completion_count();
    let transcript_min = area
        .height
        .saturating_sub(3_u16.saturating_add(composer_height).saturating_add(1))
        .min(3);
    let base_height = 3_u16
        .saturating_add(transcript_min)
        .saturating_add(composer_height)
        .saturating_add(1);
    let mut optional_height = area.height.saturating_sub(base_height);
    let desired_completion_height = u16::try_from(completion_count)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let completion_height = if completion_count > 0 && optional_height >= 3 {
        desired_completion_height.min(optional_height)
    } else {
        0
    };
    optional_height = optional_height.saturating_sub(completion_height);
    let desired_attachment_height = if state.attachments.is_empty() { 0 } else { 3 };
    let attachment_height = if desired_attachment_height > 0 && optional_height >= 3 {
        desired_attachment_height
    } else {
        0
    };
    optional_height = optional_height.saturating_sub(attachment_height);
    let desired_plan_height = state
        .plan
        .as_ref()
        .map(|plan| u16::try_from(plan.len()).unwrap_or(u16::MAX).clamp(1, 5) + 2)
        .unwrap_or(0);
    let plan_height = if desired_plan_height > 0 && optional_height >= 3 {
        desired_plan_height.min(optional_height)
    } else {
        0
    };
    let mut constraints = vec![Constraint::Length(3)];
    if plan_height > 0 {
        constraints.push(Constraint::Length(plan_height));
    }
    constraints.push(Constraint::Min(transcript_min));
    if completion_height > 0 {
        constraints.push(Constraint::Length(completion_height));
    }
    if attachment_height > 0 {
        constraints.push(Constraint::Length(attachment_height));
    }
    constraints.extend([Constraint::Length(composer_height), Constraint::Length(1)]);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_header(frame, chunks[0], state);
    let mut index = 1;
    if plan_height > 0 {
        draw_plan(frame, chunks[index], state);
        index += 1;
    }
    draw_transcript(frame, chunks[index], state);
    index += 1;
    if completion_height > 0 {
        draw_completions(frame, chunks[index], state);
        index += 1;
    }
    if attachment_height > 0 {
        draw_attachments(frame, chunks[index], state);
        index += 1;
    }
    draw_composer(frame, chunks[index], state);
    draw_footer(frame, chunks[index + 1], state);
}

fn draw_attachments(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let available = area.width.saturating_sub(6) as usize;
    let names = state.attachments.join("  ·  ");
    let line = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(Color::Magenta)),
        Span::styled(
            truncate(&names, available),
            Style::default().fg(Color::Gray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .title(Span::styled(
                    " Attachments · /detach [number|all] ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn draw_completions(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let model_completions = state.active_model_completions();
    let file_completions = state.active_file_completions();
    let command_completions = state.active_command_completions();
    let model_mode = !model_completions.is_empty();
    let file_mode = !model_mode && !file_completions.is_empty();
    let completions = if model_mode {
        model_completions
            .iter()
            .map(|model| ((*model).as_str(), "switch for this session"))
            .collect::<Vec<_>>()
    } else if file_mode {
        file_completions
            .iter()
            .map(|path| ((*path).as_str(), "attach to message"))
            .collect::<Vec<_>>()
    } else {
        command_completions
            .iter()
            .map(|completion| (completion.command.as_str(), completion.description.as_str()))
            .collect::<Vec<_>>()
    };
    let selected = state
        .completion_selected
        .min(completions.len().saturating_sub(1));
    let visible_rows = usize::from(area.height.saturating_sub(2));
    let first_visible = selected.saturating_add(1).saturating_sub(visible_rows);
    let lines = completions
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_rows)
        .map(|(index, (label, description))| {
            let marker = if index == selected { "›" } else { " " };
            let style = if index == selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(vec![
                Span::styled(format!(" {marker} {label:24}"), style),
                Span::styled(
                    truncate(description, area.width.saturating_sub(31) as usize),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(Span::styled(
                    if model_mode {
                        " Models · ↑↓ select · Enter/Tab complete "
                    } else if file_mode {
                        " Files · ↑↓ select · Enter/Tab attach "
                    } else {
                        " Commands · ↑↓ select · Tab complete "
                    },
                    Style::default().fg(Color::Cyan),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn draw_plan(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let plan = state.plan.as_deref().unwrap_or_default();
    let visible = if plan.len() > 5 { 4 } else { plan.len() };
    let mut lines = plan
        .iter()
        .take(visible)
        .map(|item| {
            let color = if item.starts_with('✓') {
                Color::Green
            } else if item.starts_with('◉') {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            Line::from(Span::styled(format!(" {item}"), Style::default().fg(color)))
        })
        .collect::<Vec<_>>();
    if plan.len() > visible {
        lines.push(Line::from(Span::styled(
            format!(" … {} more steps", plan.len() - visible),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(Span::styled(
                    " Plan ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let primary_width = area.width.saturating_sub(13) as usize;
    let secondary_width = area.width.saturating_sub(2) as usize;
    let header = Text::from(vec![
        Line::from(vec![
            Span::styled(
                " WECODE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                truncate(&state.header_primary, primary_width),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(&state.header_secondary, secondary_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_transcript(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut TuiState) {
    let width = area.width.saturating_sub(4).max(1) as usize;
    let mut lines = Vec::new();
    if state.transcript.is_empty() && state.live_response.is_none() {
        render_empty_state(&mut lines, state, width, area.height);
    } else {
        for entry in &state.transcript {
            if let TranscriptKind::Tool {
                label,
                tone,
                result,
                ..
            } = &entry.kind
            {
                render_tool_entry(
                    &mut lines,
                    label,
                    &entry.text,
                    *tone,
                    result.as_ref(),
                    width,
                );
                continue;
            }
            let (label, color, marker) = match &entry.kind {
                TranscriptKind::User => ("YOU", Color::Blue, "›"),
                TranscriptKind::Agent => ("WECODE", Color::Cyan, "◆"),
                TranscriptKind::Custom { label, tone } => (
                    label.as_str(),
                    match tone {
                        TuiTone::Normal => Color::Cyan,
                        TuiTone::Success => Color::Green,
                        TuiTone::Warning => Color::Yellow,
                        TuiTone::Error => Color::Red,
                        TuiTone::Dim => Color::DarkGray,
                    },
                    tool_marker(label),
                ),
                TranscriptKind::Tool { .. } => unreachable!("tool entries render separately"),
            };
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {marker} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));
            let raw_lines = entry.text.lines().collect::<Vec<_>>();
            let limit = transcript_line_limit(&entry.kind);
            for raw in raw_lines.iter().take(limit).copied() {
                let style = match &entry.kind {
                    TranscriptKind::Custom { label, .. } if label == "DIFF" => diff_line_style(raw),
                    TranscriptKind::Agent => markdown_line_style(raw),
                    TranscriptKind::User => Style::default().fg(Color::White),
                    TranscriptKind::Custom {
                        tone: TuiTone::Dim, ..
                    } => Style::default().fg(Color::Gray),
                    TranscriptKind::Tool { .. } => {
                        unreachable!("tool entries render separately")
                    }
                    _ => Style::default(),
                };
                for line in wrap_plain(raw, width) {
                    lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                }
            }
            if raw_lines.len() > limit {
                lines.push(Line::from(Span::styled(
                    format!("    … {} more lines hidden", raw_lines.len() - limit),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        if let Some(response) = &state.live_response {
            render_live_response(&mut lines, response, state.tick, width);
        }
    }

    let visible = area.height.saturating_sub(1);
    let content_height = lines.len().min(u16::MAX as usize) as u16;
    let max_scroll = content_height.saturating_sub(visible);
    let offset = max_scroll.saturating_sub(state.scroll.min(max_scroll));
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((offset, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_tool_entry<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    detail: &str,
    tone: TuiTone,
    result: Option<&(String, TuiTone)>,
    width: usize,
) {
    let color = tone_color(tone);
    let compact_detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let label_width = UnicodeWidthStr::width(label);
    let detail_width = width.saturating_sub(label_width + 8);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", tool_marker(label)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label.to_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", truncate(&compact_detail, detail_width)),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    match result {
        None => lines.push(Line::from(Span::styled(
            "    └ running…",
            Style::default().fg(Color::DarkGray),
        ))),
        Some((result, result_tone)) => {
            let result_color = tone_color(*result_tone);
            let compact = compact_tool_result(result);
            for (index, result_line) in compact.iter().enumerate() {
                let branch = if index + 1 == compact.len() {
                    "└"
                } else {
                    "├"
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("    {branch} "), Style::default().fg(result_color)),
                    Span::styled(
                        truncate(result_line, width.saturating_sub(8)),
                        Style::default().fg(result_color),
                    ),
                ]));
            }
        }
    }
}

fn compact_tool_result(result: &str) -> Vec<String> {
    let clean = result
        .trim()
        .strip_prefix("TOOL ERROR:")
        .map(str::trim)
        .unwrap_or(result.trim());
    if clean.is_empty() {
        return vec!["completed".into()];
    }
    let lines = clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("TOOL RESULT "))
        .collect::<Vec<_>>();
    if let Some(entries) = lines
        .iter()
        .find_map(|line| line.strip_prefix("entries:").map(str::trim))
    {
        return vec![format!("{entries} entries")];
    }
    if let Some(summary) = lines.iter().rev().find(|line| {
        (line.starts_with('[') && line.contains("matches across")) || **line == "No matches found."
    }) {
        return vec![summary.trim_matches(['[', ']']).to_owned()];
    }
    if let Some(range) = lines
        .iter()
        .find_map(|line| line.strip_prefix("lines:").map(str::trim))
        && let Some(file) = lines
            .iter()
            .find_map(|line| line.strip_prefix("file:").map(str::trim))
    {
        return vec![format!("{file} · lines {range}")];
    }
    if lines.len() <= 3 {
        return lines.into_iter().map(ToOwned::to_owned).collect();
    }

    let mut output = lines
        .iter()
        .take(2)
        .map(|line| (*line).to_owned())
        .collect::<Vec<_>>();
    if let Some(summary) = lines.iter().find(|line| {
        line.starts_with("entries:")
            || line.starts_with("matches:")
            || line.starts_with("exit code:")
            || line.starts_with("lines:")
    }) && !output.iter().any(|line| line == summary)
    {
        output.push((*summary).to_owned());
    }
    output.push(format!(
        "… {} more lines",
        lines.len().saturating_sub(output.len())
    ));
    output
}

fn tone_color(tone: TuiTone) -> Color {
    match tone {
        TuiTone::Normal => Color::Cyan,
        TuiTone::Success => Color::Green,
        TuiTone::Warning => Color::Yellow,
        TuiTone::Error => Color::Red,
        TuiTone::Dim => Color::DarkGray,
    }
}

fn render_empty_state<'a>(
    lines: &mut Vec<Line<'a>>,
    state: &'a TuiState,
    width: usize,
    height: u16,
) {
    let top_padding = height.saturating_sub(14) / 3;
    lines.extend((0..top_padding).map(|_| Line::from("")));
    lines.push(centered_line(
        "◆  W E C O D E",
        width,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(centered_line(
        "Your repository, understood and changed from the terminal",
        width,
        Style::default().fg(Color::Gray),
    ));
    lines.push(Line::from(""));
    lines.push(centered_line(
        "Describe a task in the message box below",
        width,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(centered_line(
        "fix a bug   ·   build a feature   ·   explain code   ·   review changes",
        width,
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::from(""));
    if let Some(welcome) = &state.welcome {
        lines.push(centered_line(
            &format!("● ready  ·  {}", welcome.model),
            width,
            Style::default().fg(Color::Green),
        ));
        lines.push(centered_line(
            &welcome.workspace,
            width,
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(centered_line(
            &format!("session {}  ·  {}", welcome.session, welcome.capabilities),
            width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(""));
    lines.push(centered_line(
        "/ for commands   @ for files   ! for shell",
        width,
        Style::default().fg(Color::DarkGray),
    ));
}

fn centered_line(value: &str, width: usize, style: Style) -> Line<'static> {
    let value = truncate(value, width);
    let value_width = UnicodeWidthStr::width(value.as_str());
    let padding = " ".repeat(width.saturating_sub(value_width) / 2);
    Line::from(vec![Span::raw(padding), Span::styled(value, style)])
}

fn render_live_response<'a>(
    lines: &mut Vec<Line<'a>>,
    response: &'a LiveResponse,
    tick: usize,
    width: usize,
) {
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", frames[tick % frames.len()]),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if response.text.is_empty() {
                "WECODE · THINKING"
            } else {
                "WECODE · RESPONDING"
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let visible = if response.text.trim().is_empty() {
        response
            .reasoning
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        response.text.clone()
    };
    if visible.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "    Inspecting the repository and planning the next step…",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    if visible.trim_start().starts_with('{')
        && (visible.contains("\"action\"")
            || visible.contains("\"tool_calls\"")
            || !visible.contains('\n'))
    {
        lines.push(Line::from(Span::styled(
            "    Selecting the next repository action…",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    for raw in visible
        .lines()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        for line in wrap_plain(raw, width) {
            lines.push(Line::from(Span::styled(
                format!("    {line}"),
                if response.text.is_empty() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    markdown_line_style(raw)
                },
            )));
        }
    }
}

fn transcript_line_limit(kind: &TranscriptKind) -> usize {
    match kind {
        TranscriptKind::Custom { label, .. }
            if matches!(
                label.as_str(),
                "READ" | "LIST" | "GLOB" | "GREP" | "OUTPUT" | "RESULT"
            ) =>
        {
            10
        }
        TranscriptKind::Custom { label, .. } if label.starts_with("RUN ·") => 12,
        _ => usize::MAX,
    }
}

fn tool_marker(label: &str) -> &'static str {
    if label.starts_with("EDIT") {
        "±"
    } else if label.starts_with("RUN") || label == "SHELL" {
        "$"
    } else if label == "ERROR" || label == "STOPPED" {
        "×"
    } else if label == "DONE" || label == "VERIFY" {
        "✓"
    } else if label == "WARNING" || label == "APPROVAL" {
        "!"
    } else {
        "•"
    }
}

fn markdown_line_style(line: &str) -> Style {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("```") {
        Style::default().fg(Color::Magenta)
    } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        Style::default().fg(Color::White)
    } else if trimmed.starts_with('>') {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

fn looks_like_internal_protocol(text: &str) -> bool {
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| {
            object.contains_key("action")
                || object.contains_key("tool_calls")
                || object.contains_key("function_call")
        })
}

fn diff_line_style(line: &str) -> Style {
    if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
    {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Magenta)
    } else if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("Binary files ") {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let running = state.status.is_some();
    let overridden = state.composer_title.is_some();
    let shell_mode = !overridden && state.is_shell_mode();
    let color = if overridden {
        Color::Magenta
    } else if shell_mode {
        Color::Green
    } else if running {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let title = state.composer_title.as_deref().unwrap_or(if shell_mode {
        if state.composer.text.trim_start().starts_with("!!") {
            " Shell · excluded from context "
        } else {
            " Shell · included in context "
        }
    } else if running {
        " Steer active task "
    } else {
        " Ask WeCode "
    });
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let prompt_color = if shell_mode { Color::Green } else { color };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "›",
            Style::default()
                .fg(prompt_color)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(inner.x, inner.y, 1, inner.height),
    );
    let text_area = Rect::new(
        inner.x.saturating_add(2),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    if state.composer.text.is_empty() {
        let placeholder =
            state
                .composer_placeholder
                .as_deref()
                .unwrap_or(if !state.attachments.is_empty() {
                    "Add a message, or press Enter to send the attachments"
                } else if running {
                    "Type to steer, or Alt-Enter to queue the next task"
                } else {
                    "Describe what you want to build, fix, inspect, or understand…"
                });
        frame.render_widget(
            Paragraph::new(Span::styled(
                placeholder,
                Style::default().fg(Color::DarkGray),
            )),
            text_area,
        );
        frame.set_cursor_position(Position::new(text_area.x, text_area.y));
        return;
    }

    frame.render_widget(
        Paragraph::new(state.composer.text.as_str()).wrap(Wrap { trim: false }),
        text_area,
    );
    let (cursor_x, cursor_y) =
        cursor_position(&state.composer.text, state.composer.cursor, text_area.width);
    frame.set_cursor_position(Position::new(
        text_area.x.saturating_add(cursor_x),
        text_area
            .y
            .saturating_add(cursor_y)
            .min(text_area.bottom().saturating_sub(1)),
    ));
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    if state.composer_title.is_some() {
        let spans = vec![
            key(" Enter "),
            hint("answer"),
            separator(),
            key(" Ctrl-J "),
            hint("newline"),
            separator(),
            key(" Ctrl-C "),
            hint("cancel task"),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else if state.is_shell_mode() {
        let spans = vec![
            key(" Enter "),
            hint("run"),
            separator(),
            key(" !! "),
            hint("exclude from context"),
            separator(),
            key(" Esc "),
            hint("clear"),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else if let Some(status) = &state.status {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let animated = status.starts_with("Thinking")
            || status.starts_with("Streaming")
            || status.starts_with("Reviewing")
            || status.starts_with("Selecting");
        let spans = vec![
            Span::raw(" "),
            Span::styled(
                truncate(
                    &if animated {
                        format!("{} {status}", frames[state.tick % frames.len()])
                    } else {
                        status.clone()
                    },
                    area.width.saturating_sub(57) as usize,
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            separator(),
            key(" Ctrl-C "),
            hint("cancel"),
            separator(),
            key(" Enter "),
            hint("steer"),
            separator(),
            key(" Alt-Enter "),
            hint("queue"),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else {
        let mut spans = if state.metrics.is_some() && area.width >= 70 {
            vec![
                key(" Enter "),
                hint("send"),
                separator(),
                key(" Shift-Enter "),
                hint("newline"),
            ]
        } else if area.width < 90 {
            vec![
                key(" Enter "),
                hint("send"),
                separator(),
                key(" / "),
                hint("commands"),
            ]
        } else {
            vec![
                key(" Enter "),
                hint("send"),
                separator(),
                key(" Shift-Enter "),
                hint("newline"),
                separator(),
                key(" / "),
                hint("commands"),
                separator(),
                key(" @ "),
                hint("files"),
            ]
        };
        if let Some(metrics) = &state.metrics {
            spans.push(Span::raw("    "));
            spans.push(Span::styled(
                truncate(
                    metrics,
                    area.width
                        .saturating_sub(if area.width >= 70 { 38 } else { 30 })
                        as usize,
                ),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            spans.extend([
                separator(),
                key(" ! "),
                hint("shell"),
                separator(),
                key(" Ctrl-L "),
                hint("clear"),
            ]);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(Color::Black)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

fn hint(value: &'static str) -> Span<'static> {
    Span::styled(format!(" {value}"), Style::default().fg(Color::DarkGray))
}

fn separator() -> Span<'static> {
    Span::styled("  ·  ", Style::default().fg(Color::DarkGray))
}

fn composer_height(text: &str, width: u16) -> u16 {
    let inner = width.saturating_sub(4).max(1) as usize;
    let lines = text
        .lines()
        .map(|line| UnicodeWidthStr::width(line).max(1).div_ceil(inner))
        .sum::<usize>()
        .max(1);
    (lines as u16).clamp(1, 6).saturating_add(2)
}

fn cursor_position(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = width.max(1) as usize;
    let before = text.chars().take(cursor).collect::<String>();
    let mut x = 0usize;
    let mut y = 0usize;
    for character in before.chars() {
        if character == '\n' {
            x = 0;
            y += 1;
        } else {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if x + character_width > width {
                x = 0;
                y += 1;
            }
            x += character_width;
            if x >= width {
                x = 0;
                y += 1;
            }
        }
    }
    (x as u16, y as u16)
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let mut output = Vec::new();
    for raw in text.lines() {
        if raw.is_empty() {
            output.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for character in raw.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if line_width > 0 && line_width + character_width > width.max(1) {
                output.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
        if !line.is_empty() {
            output.push(line);
        }
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let mut result = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    result
}

fn load_history(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn save_history_entry(path: &Path, text: &str) {
    use std::io::Write;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{one_line}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn composer_edits_unicode_by_character() {
        let mut composer = Composer::default();
        composer.insert("你a好");
        composer.left();
        composer.backspace();
        assert_eq!(composer.text, "你好");
        composer.delete();
        assert_eq!(composer.text, "你");
    }

    #[test]
    fn composer_home_and_end_stay_on_line() {
        let mut composer = Composer::default();
        composer.insert("one\ntwo");
        composer.home();
        assert_eq!(composer.cursor, 4);
        composer.end();
        assert_eq!(composer.cursor, 7);
    }

    #[test]
    fn cursor_wraps_and_handles_newlines() {
        assert_eq!(cursor_position("abcdef", 6, 4), (2, 1));
        assert_eq!(cursor_position("ab\ncd", 5, 10), (2, 1));
    }

    #[test]
    fn output_is_wrapped_without_losing_blank_lines() {
        assert_eq!(wrap_plain("abcd\n\nxy", 2), ["ab", "cd", "", "xy"]);
    }

    #[test]
    fn diff_lines_use_semantic_colors() {
        assert_eq!(diff_line_style("+added").fg, Some(Color::Green));
        assert_eq!(diff_line_style("-removed").fg, Some(Color::Red));
        assert_eq!(diff_line_style("@@ -1 +1 @@").fg, Some(Color::Magenta));
        assert_eq!(diff_line_style(" context").fg, None);
    }

    #[test]
    fn slash_palette_filters_navigates_and_completes() {
        let completions = vec![
            CommandCompletion {
                command: "/help".into(),
                description: "Show help".into(),
            },
            CommandCompletion {
                command: "/hooks".into(),
                description: "Show hooks".into(),
            },
        ];
        let mut state = TuiState::new(Vec::new(), completions);
        state.composer.insert("/h");
        assert_eq!(state.active_command_completions().len(), 2);
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(state.composer.text, "/hooks ");
        assert!(state.active_command_completions().is_empty());
    }

    #[test]
    fn slash_palette_renders_above_the_composer() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(
            Vec::new(),
            vec![CommandCompletion {
                command: "/review".into(),
                description: "Review current changes".into(),
            }],
        );
        state.composer.insert("/rev");
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("/review"));
        assert!(rendered.contains("Review current changes"));
        assert!(rendered.contains("Tab complete"));
    }

    #[test]
    fn model_palette_filters_completes_and_submits() {
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.models = vec![
            "gpt-5.4".into(),
            "gpt-5.4-mini".into(),
            "claude-haiku-4-5".into(),
        ];
        state.composer.insert("/model mini");

        assert_eq!(
            state
                .active_model_completions()
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["gpt-5.4-mini"]
        );
        let completed = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(completed.input.is_none());
        assert_eq!(state.composer.text, "/model gpt-5.4-mini");

        let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submitted.input,
            Some(ChatInput::Command(crate::chat::ChatCommand::Model(Some(
                "gpt-5.4-mini".into()
            ))))
        );
    }

    #[test]
    fn shell_mode_is_visually_distinct_and_skips_prompt_completions() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(
            Vec::new(),
            vec![CommandCompletion {
                command: "/help".into(),
                description: "Show help".into(),
            }],
        );
        state.files = vec!["src/main.rs".into()];
        state.composer.insert("!! echo @src/main.rs");

        assert!(state.is_shell_mode());
        assert!(state.active_command_completions().is_empty());
        assert!(state.active_file_completions().is_empty());
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Shell · excluded from context"));
        assert!(rendered.contains("exclude from context"));

        let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submitted.input,
            Some(ChatInput::Shell {
                command: "echo @src/main.rs".into(),
                include_in_context: false,
            })
        );
    }

    #[test]
    fn file_palette_fuzzy_matches_and_inserts_mentions() {
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.files = vec![
            "README.md".into(),
            "src/parse_helpers.rs".into(),
            "src/parser.rs".into(),
        ];
        state.composer.insert("Fix @par");

        assert_eq!(
            state
                .active_file_completions()
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["src/parser.rs", "src/parse_helpers.rs"]
        );
        state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(state.composer.text, "Fix @src/parser.rs ");
        assert!(state.active_file_completions().is_empty());
    }

    #[test]
    fn file_palette_quotes_paths_with_spaces_and_ignores_emails() {
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.files = vec!["docs/error report.md".into()];
        state.composer.insert("Read @\"error rep");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.composer.text, "Read @\"docs/error report.md\" ");

        state.composer.set("Email a@b.com".into());
        assert!(state.active_file_query().is_none());
    }

    #[test]
    fn file_palette_renders_above_the_composer() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.files = vec!["src/parser.rs".into()];
        state.composer.insert("Fix @parser");
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Files"));
        assert!(rendered.contains("src/parser.rs"));
        assert!(rendered.contains("Enter/Tab attach"));
        assert!(rendered.contains("Fix @parser"));
    }

    #[test]
    fn enter_submits_pending_attachments_without_requiring_text() {
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.attachments = vec!["1:image:~/screen.png".into()];

        let outcome = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome.input,
            Some(ChatInput::Task(
                "Inspect the attached file or image.".into()
            ))
        );
    }

    #[test]
    fn short_terminal_keeps_the_composer_visible() {
        let backend = TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let completions = (0..8)
            .map(|index| CommandCompletion {
                command: format!("/command-{index}"),
                description: format!("Command {index}"),
            })
            .collect();
        let mut state = TuiState::new(Vec::new(), completions);
        state.composer.insert("/c");
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Ask WeCode"));
        assert!(rendered.contains("/c"));
    }

    #[test]
    fn full_screen_layout_keeps_header_timeline_and_composer_visible() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.header_primary = "openai · gpt-mini  |  ~/project".into();
        state.header_secondary = "session abc123 · 2 rules · chat-completions".into();
        state.append_user("修复失败的测试");
        state.append_entry("RUN".into(), "cargo test".into(), TuiTone::Normal);
        state.metrics = Some("1.2k in · 80 out · 900 cached".into());
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("WECODE"));
        assert!(rendered.contains("gpt-mini"));
        assert!(rendered.contains('修'));
        assert!(rendered.contains('测'));
        assert!(rendered.contains("Ask WeCode"));
        assert!(rendered.contains("Shift-Enter"));
        assert!(rendered.contains("cached"));
    }

    #[test]
    fn narrow_footer_keeps_file_and_command_discovery_visible() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("send"));
        assert!(rendered.contains("commands"));
        assert!(rendered.contains("files"));
        assert!(rendered.contains("shell"));
    }

    #[test]
    fn plan_panel_and_question_composer_remain_visible_together() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.plan = Some(vec![
            "✓ Inspect repository".into(),
            "◉ Implement parser".into(),
            "○ Run tests".into(),
        ]);
        state.composer_title = Some(" Answer WeCode ".into());
        state.composer_placeholder = Some("Type 1, 2, or another answer".into());
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Plan"));
        assert!(rendered.contains("Implement parser"));
        assert!(rendered.contains("Answer WeCode"));
        assert!(rendered.contains("another answer"));
        assert!(rendered.contains("cancel task"));
    }

    #[test]
    fn attachment_panel_renders_without_hiding_the_composer() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.attachments = vec!["1:image:~/screen.png".into(), "2:file:~/notes.txt".into()];
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Attachments"));
        assert!(rendered.contains("screen.png"));
        assert!(rendered.contains("Ask WeCode"));
        assert!(rendered.contains("send"));
    }

    #[test]
    fn short_terminal_prioritizes_composer_over_attachment_panel() {
        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.attachments = vec!["1:image:~/screen.png".into()];
        state.composer.insert("describe this");
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Ask WeCode"));
        assert!(rendered.contains("describe this"));
    }

    #[test]
    fn welcome_state_looks_like_a_ready_coding_agent() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.welcome = Some(WelcomeCard {
            model: "openai / gpt-mini".into(),
            workspace: "~/project".into(),
            session: "abc123".into(),
            capabilities: "repo · shell · edit · plan".into(),
        });
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("W E C O D E"));
        assert!(rendered.contains("Your repository"));
        assert!(rendered.contains("openai / gpt-mini"));
        assert!(rendered.contains("Describe a task"));
        assert!(rendered.contains("Ask WeCode"));
    }

    #[test]
    fn streamed_response_is_visible_in_the_timeline() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.live_response = Some(LiveResponse {
            reasoning: String::new(),
            text: "I found the failing parser test.".into(),
        });
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("WECODE · RESPONDING"));
        assert!(rendered.contains("failing parser test"));
    }

    #[test]
    fn alt_arrows_move_by_word() {
        let mut composer = Composer::default();
        composer.insert("fix the parser");
        composer.previous_word();
        assert_eq!(composer.cursor, 8);
        composer.previous_word();
        assert_eq!(composer.cursor, 4);
        composer.next_word();
        assert_eq!(composer.cursor, 8);
    }

    #[test]
    fn internal_tool_protocol_is_never_rendered_as_assistant_copy() {
        assert!(looks_like_internal_protocol(
            r#"{"action":"list_files","path":".","depth":2}"#
        ));
        assert!(looks_like_internal_protocol(
            "```json\n{\"action\":\"finish\",\"summary\":\"done\"}\n```"
        ));
        assert!(!looks_like_internal_protocol(
            r#"{"status":"healthy","tests":12}"#
        ));
    }

    #[test]
    fn tool_call_and_result_render_as_one_compact_timeline_item() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new(), Vec::new());
        state.append_user("inspect the repository");
        state.append_tool(1, "LIST".into(), ". · depth 2".into(), TuiTone::Normal);
        state.finish_tool(
            1,
            "directory: .\ndepth: 2\nentries: 42\nsrc/\nsrc/main.rs\nCargo.toml".into(),
            TuiTone::Dim,
        );
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("LIST"));
        assert!(rendered.contains("depth 2"));
        assert!(rendered.contains("42 entries"));
        assert!(!rendered.contains("OUTPUT"));
    }
}
