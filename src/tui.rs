use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::{Context, Result};
use console::strip_ansi_codes;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
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
    Clear,
    Header {
        primary: String,
        secondary: String,
    },
    Metrics(Option<String>),
    Status(Option<String>),
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

    pub fn clear(&self) {
        let _ = self.sender.send(TuiMessage::Clear);
    }

    pub fn set_header(&self, primary: String, secondary: String) {
        let _ = self.sender.send(TuiMessage::Header { primary, secondary });
    }

    pub fn set_status(&self, status: Option<String>) {
        let _ = self.sender.send(TuiMessage::Status(status));
    }

    pub fn set_metrics(&self, metrics: Option<String>) {
        let _ = self.sender.send(TuiMessage::Metrics(metrics));
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
) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let history = load_history(&history_path);
    let mut state = TuiState::new(history);
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

        if !event::poll(Duration::from_millis(40))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let outcome = state.handle_key(key);
                redraw = outcome.changed;
                if let Some(input) = outcome.input {
                    if let ChatInput::Task(text) | ChatInput::FollowUp(text) = &input {
                        save_history_entry(&history_path, text);
                        state.history.push(text.clone());
                        state.history_index = None;
                        state.append_user(text);
                    }
                    let done = matches!(input, ChatInput::Exit);
                    if inputs.send(Ok(input)).is_err() || done {
                        break;
                    }
                }
            }
            Event::Paste(text) => {
                state.composer.insert(&text);
                redraw = true;
            }
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
            Ok(TuiMessage::Clear) => {
                state.transcript.clear();
                state.metrics = None;
                state.scroll = 0;
                state.status = None;
                changed = true;
            }
            Ok(TuiMessage::Header { primary, secondary }) => {
                state.header_primary = primary;
                state.header_secondary = secondary;
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
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
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
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone)]
enum TranscriptKind {
    User,
    Agent,
    Custom { label: String, tone: TuiTone },
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
}

struct TuiState {
    composer: Composer,
    header_primary: String,
    header_secondary: String,
    history: Vec<String>,
    history_index: Option<usize>,
    metrics: Option<String>,
    scroll: u16,
    status: Option<String>,
    transcript: Vec<TranscriptEntry>,
}

impl TuiState {
    fn new(history: Vec<String>) -> Self {
        Self {
            composer: Composer::default(),
            header_primary: format!("WeCode {}", env!("CARGO_PKG_VERSION")),
            header_secondary: "Lightweight coding agent".into(),
            history,
            history_index: None,
            metrics: None,
            scroll: 0,
            status: None,
            transcript: Vec::new(),
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
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter => {
                if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) {
                    self.composer.insert("\n");
                    return KeyOutcome::changed();
                }
                let text = self.composer.take();
                if text.trim().is_empty() {
                    return KeyOutcome::changed();
                }
                if modifiers.contains(KeyModifiers::ALT) {
                    KeyOutcome::input(ChatInput::FollowUp(text))
                } else {
                    KeyOutcome::input(parse_input(text.trim()))
                }
            }
            KeyCode::Char(character) => {
                self.composer.insert_char(character);
                KeyOutcome::changed()
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                KeyOutcome::changed()
            }
            KeyCode::Delete => {
                self.composer.delete();
                KeyOutcome::changed()
            }
            KeyCode::Left => {
                self.composer.left();
                KeyOutcome::changed()
            }
            KeyCode::Right => {
                self.composer.right();
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], state);
    draw_transcript(frame, chunks[1], state);
    draw_composer(frame, chunks[2], state);
    draw_footer(frame, chunks[3], state);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
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
                &state.header_primary,
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                &state.header_secondary,
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
    if state.transcript.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Ask WeCode to inspect, change, test, or explain this repository.",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Examples: “fix the failing tests” · “explain this codebase”",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        for entry in &state.transcript {
            let (label, color) = match &entry.kind {
                TranscriptKind::User => ("YOU", Color::Blue),
                TranscriptKind::Agent => ("WECODE", Color::Cyan),
                TranscriptKind::Custom { label, tone } => (
                    label.as_str(),
                    match tone {
                        TuiTone::Normal => Color::Cyan,
                        TuiTone::Success => Color::Green,
                        TuiTone::Warning => Color::Yellow,
                        TuiTone::Error => Color::Red,
                        TuiTone::Dim => Color::DarkGray,
                    },
                ),
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {label}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            for line in wrap_plain(&entry.text, width) {
                lines.push(Line::from(format!("  {line}")));
            }
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

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let running = state.status.is_some();
    let color = if running { Color::Yellow } else { Color::Cyan };
    let title = if running {
        " Steer active task "
    } else {
        " Message "
    };
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

    if state.composer.text.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                if running {
                    "Type to steer, or Alt-Enter to queue the next task"
                } else {
                    "Ask WeCode to do anything in this repository"
                },
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        frame.set_cursor_position(Position::new(inner.x, inner.y));
        return;
    }

    frame.render_widget(
        Paragraph::new(state.composer.text.as_str()).wrap(Wrap { trim: false }),
        inner,
    );
    let (cursor_x, cursor_y) =
        cursor_position(&state.composer.text, state.composer.cursor, inner.width);
    frame.set_cursor_position(Position::new(
        inner.x.saturating_add(cursor_x),
        inner
            .y
            .saturating_add(cursor_y)
            .min(inner.bottom().saturating_sub(1)),
    ));
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    if let Some(status) = &state.status {
        let spans = vec![
            Span::raw(" "),
            Span::styled(
                truncate(status, area.width.saturating_sub(57) as usize),
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
        let mut spans = vec![
            key(" Enter "),
            hint("send"),
            separator(),
            key(" Ctrl-J "),
            hint("newline"),
            separator(),
            key(" Alt-Enter "),
            hint("follow-up"),
        ];
        if let Some(metrics) = &state.metrics {
            spans.push(Span::raw("    "));
            spans.push(Span::styled(
                truncate(metrics, area.width.saturating_sub(62) as usize),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            spans.extend([separator(), key(" / "), hint("commands")]);
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
    fn full_screen_layout_keeps_header_timeline_and_composer_visible() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::new(Vec::new());
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
        assert!(rendered.contains("Message"));
        assert!(rendered.contains("Alt-Enter"));
        assert!(rendered.contains("cached"));
    }
}
