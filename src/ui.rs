use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use console::{Style, Term, strip_ansi_codes};
use indicatif::{ProgressBar, ProgressStyle};
use rustyline::ExternalPrinter;

use crate::events::{Event, EventSink};
use crate::tui::{TuiHandle, TuiTone};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiMode {
    #[default]
    Run,
    Chat,
}

#[derive(Clone)]
pub struct TerminalOutput {
    inner: Arc<TerminalOutputInner>,
}

enum TerminalOutputInner {
    Stdout,
    Stderr,
    External(Mutex<Box<dyn ExternalPrinter + Send>>),
    Tui(TuiHandle),
}

impl TerminalOutput {
    pub fn stdout() -> Self {
        Self {
            inner: Arc::new(TerminalOutputInner::Stdout),
        }
    }

    pub fn stderr() -> Self {
        Self {
            inner: Arc::new(TerminalOutputInner::Stderr),
        }
    }

    pub fn external(printer: Box<dyn ExternalPrinter + Send>) -> Self {
        Self {
            inner: Arc::new(TerminalOutputInner::External(Mutex::new(printer))),
        }
    }

    pub fn tui(handle: TuiHandle) -> Self {
        Self {
            inner: Arc::new(TerminalOutputInner::Tui(handle)),
        }
    }

    pub fn print(&self, message: impl Into<String>) -> Result<()> {
        let message = message.into();
        match self.inner.as_ref() {
            TerminalOutputInner::Stdout => {
                let mut output = io::stdout().lock();
                output.write_all(message.as_bytes())?;
                output.flush()?;
            }
            TerminalOutputInner::Stderr => {
                let mut output = io::stderr().lock();
                output.write_all(message.as_bytes())?;
                output.flush()?;
            }
            TerminalOutputInner::External(printer) => {
                printer
                    .lock()
                    .expect("terminal output lock poisoned")
                    .print(message)?;
            }
            TerminalOutputInner::Tui(handle) => handle.append(message),
        }
        Ok(())
    }

    fn is_external(&self) -> bool {
        matches!(self.inner.as_ref(), TerminalOutputInner::External(_))
    }

    fn is_tui(&self) -> bool {
        matches!(self.inner.as_ref(), TerminalOutputInner::Tui(_))
    }

    pub fn set_tui_header(&self, primary: String, secondary: String) -> bool {
        let TerminalOutputInner::Tui(handle) = self.inner.as_ref() else {
            return false;
        };
        handle.set_header(primary, secondary);
        true
    }

    pub fn clear_tui(&self) -> bool {
        let TerminalOutputInner::Tui(handle) = self.inner.as_ref() else {
            return false;
        };
        handle.clear();
        true
    }

    pub fn set_tui_status(&self, status: Option<String>) {
        if let TerminalOutputInner::Tui(handle) = self.inner.as_ref() {
            handle.set_status(status);
        }
    }

    pub fn set_tui_metrics(&self, metrics: Option<String>) {
        if let TerminalOutputInner::Tui(handle) = self.inner.as_ref() {
            handle.set_metrics(metrics);
        }
    }

    pub fn tui_entry(
        &self,
        label: impl Into<String>,
        text: impl Into<String>,
        tone: TuiTone,
    ) -> bool {
        let TerminalOutputInner::Tui(handle) = self.inner.as_ref() else {
            return false;
        };
        handle.entry(label.into(), text.into(), tone);
        true
    }
}

pub struct TerminalUi {
    spinner: Mutex<Option<ProgressBar>>,
    stream_preview: Mutex<String>,
    interactive: bool,
    allow_deltas: bool,
    mode: UiMode,
    output: TerminalOutput,
}

impl TerminalUi {
    pub fn new() -> Self {
        Self::with_mode(UiMode::Run, TerminalOutput::stderr(), true)
    }

    pub fn benchmark() -> Self {
        Self::with_mode(UiMode::Run, TerminalOutput::stderr(), false)
    }

    pub fn chat(output: TerminalOutput) -> Self {
        Self::with_mode(UiMode::Chat, output, true)
    }

    fn with_mode(mode: UiMode, output: TerminalOutput, allow_deltas: bool) -> Self {
        Self {
            spinner: Mutex::new(None),
            stream_preview: Mutex::new(String::new()),
            interactive: io::stderr().is_terminal(),
            allow_deltas,
            mode,
            output,
        }
    }

    fn stop_spinner(&self) {
        self.output.set_tui_status(None);
        if let Some(spinner) = self.spinner.lock().expect("spinner lock poisoned").take() {
            spinner.finish_and_clear();
        }
    }

    fn start_spinner(&self, step: usize) -> Result<()> {
        self.stop_spinner();
        self.stream_preview
            .lock()
            .expect("stream preview lock poisoned")
            .clear();
        if self.output.is_tui() {
            self.output
                .set_tui_status(Some(format!("⠋ Thinking · step {step}")));
        } else if self.output.is_external() {
            self.output.print(format!("  ⠋ Thinking · step {step}\n"))?;
        } else if self.interactive {
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(
                ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                    .expect("valid spinner template")
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            spinner.set_message(format!("Thinking · step {step}"));
            spinner.enable_steady_tick(std::time::Duration::from_millis(90));
            *self.spinner.lock().expect("spinner lock poisoned") = Some(spinner);
        } else {
            self.output.print(format!("  Thinking · step {step}\n"))?;
        }
        Ok(())
    }

    fn show_delta(&self, text: &str, reasoning: bool) {
        if !self.interactive || self.output.is_external() || text.is_empty() {
            return;
        }
        let mut preview = self
            .stream_preview
            .lock()
            .expect("stream preview lock poisoned");
        preview.push_str(text);
        *preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");
        if preview.chars().count() > 88 {
            *preview = preview
                .chars()
                .rev()
                .take(88)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
        }
        let label = if reasoning { "Thinking" } else { "Streaming" };
        if self.output.is_tui() {
            self.output
                .set_tui_status(Some(format!("● {label} · {preview}")));
        } else if let Some(spinner) = self.spinner.lock().expect("spinner lock poisoned").as_ref() {
            spinner.set_message(format!("{label} · {preview}"));
        }
    }
}

impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for TerminalUi {
    fn emit(&self, event: &Event) -> Result<()> {
        match event {
            Event::RunStarted {
                provider,
                model,
                workspace,
                ..
            } => {
                if self.mode == UiMode::Run {
                    self.output.print(render_panel(
                        "WeCode",
                        &format!("{provider} / {model}\n{workspace}"),
                        PanelTone::Cyan,
                        4,
                    ))?;
                }
            }
            Event::ModelStarted { step } => self.start_spinner(*step)?,
            Event::ModelDelta {
                text, reasoning, ..
            } => self.show_delta(text, *reasoning),
            Event::ModelCompleted {
                step,
                cache_hit,
                usage,
            } => {
                self.stop_spinner();
                let cache = if *cache_hit {
                    " · exact cache"
                } else if usage.cache_read_tokens > 0 {
                    " · prompt cache"
                } else {
                    ""
                };
                let metrics = format!(
                    "{} in · {} out · {} cached{cache}",
                    compact_number(usage.input_tokens),
                    compact_number(usage.output_tokens),
                    compact_number(usage.cache_read_tokens),
                );
                if self.output.is_tui() {
                    self.output.set_tui_metrics(Some(metrics));
                    return Ok(());
                }
                self.output.print(format!(
                    "  {} step {step} · {} in · {} out · {} cached{cache}\n",
                    Style::new().green().apply_to("✓"),
                    compact_number(usage.input_tokens),
                    compact_number(usage.output_tokens),
                    compact_number(usage.cache_read_tokens),
                ))?;
            }
            Event::Action {
                kind,
                description,
                detail,
                ..
            } => {
                self.stop_spinner();
                if self.output.is_tui() {
                    let (label, text, tone) = match kind.as_str() {
                        "read_file" => ("READ".into(), detail.to_owned(), TuiTone::Normal),
                        "list_files" => ("LIST".into(), detail.to_owned(), TuiTone::Normal),
                        "glob" => ("GLOB".into(), detail.to_owned(), TuiTone::Normal),
                        "grep" => ("GREP".into(), detail.to_owned(), TuiTone::Normal),
                        "shell" => (
                            format!("RUN · {description}"),
                            detail.to_owned(),
                            TuiTone::Normal,
                        ),
                        "patch" => (
                            format!("EDIT · {description}"),
                            detail.to_owned(),
                            TuiTone::Warning,
                        ),
                        "finish" => ("WECODE".into(), detail.to_owned(), TuiTone::Success),
                        _ => (kind.to_uppercase(), detail.to_owned(), TuiTone::Normal),
                    };
                    self.output.tui_entry(label, text, tone);
                    return Ok(());
                }
                let panel = match kind.as_str() {
                    "shell" => render_panel(
                        &format!("Shell · {description}"),
                        detail,
                        PanelTone::Cyan,
                        8,
                    ),
                    "patch" => render_panel(
                        &format!("Edit · {description}"),
                        detail,
                        PanelTone::Yellow,
                        8,
                    ),
                    "finish" => render_panel("WeCode", detail, PanelTone::Green, 20),
                    _ => render_panel(kind, detail, PanelTone::Cyan, 8),
                };
                self.output.print(panel)?;
            }
            Event::ApprovalRequested {
                kind,
                risk,
                summary,
                detail,
                ..
            } => {
                self.stop_spinner();
                if self.mode == UiMode::Run {
                    self.output.print(render_panel(
                        &format!("Approval · {kind} · {risk}"),
                        &format!("{summary}\n{detail}"),
                        PanelTone::Yellow,
                        10,
                    ))?;
                }
            }
            Event::ApprovalResolved { decision, .. } => {
                if self
                    .output
                    .tui_entry("APPROVAL", decision, TuiTone::Warning)
                {
                    return Ok(());
                }
                self.output.print(format!(
                    "  {} approval {decision}\n",
                    Style::new().yellow().apply_to("◆")
                ))?;
            }
            Event::ToolCompleted {
                exit_code,
                duration_ms,
                truncated_bytes,
                ..
            } => {
                if self.output.is_tui() && *exit_code == Some(0) && *truncated_bytes == 0 {
                    return Ok(());
                }
                let status = match exit_code {
                    Some(0) => Style::new().green().apply_to("exit 0").to_string(),
                    Some(code) => Style::new()
                        .red()
                        .apply_to(format!("exit {code}"))
                        .to_string(),
                    None => Style::new().yellow().apply_to("no exit code").to_string(),
                };
                let truncated = if *truncated_bytes > 0 {
                    format!(" · {} truncated", compact_number(*truncated_bytes as u64))
                } else {
                    String::new()
                };
                let plain_status = match exit_code {
                    Some(0) => "exit 0".to_owned(),
                    Some(code) => format!("exit {code}"),
                    None => "no exit code".to_owned(),
                };
                if self.output.tui_entry(
                    "RESULT",
                    format!(
                        "{plain_status} · {:.1}s{truncated}",
                        *duration_ms as f64 / 1000.0
                    ),
                    if *exit_code == Some(0) {
                        TuiTone::Success
                    } else {
                        TuiTone::Warning
                    },
                ) {
                    return Ok(());
                }
                self.output.print(format!(
                    "  ↳ {status} · {:.1}s{truncated}\n",
                    *duration_ms as f64 / 1000.0
                ))?;
            }
            Event::ToolOutput { output, .. } => {
                let recovering = output.starts_with("FORMAT ERROR:");
                if self.output.tui_entry(
                    if recovering { "RECOVERING" } else { "OUTPUT" },
                    output,
                    if recovering {
                        TuiTone::Warning
                    } else {
                        TuiTone::Dim
                    },
                ) {
                    return Ok(());
                }
                self.output
                    .print(render_panel("Output", output, PanelTone::Dim, 14))?;
            }
            Event::ContextCompacted { removed_messages } => {
                if self.output.tui_entry(
                    "CONTEXT",
                    format!("compacted {removed_messages} older messages"),
                    TuiTone::Warning,
                ) {
                    return Ok(());
                }
                self.output.print(format!(
                    "  {} compacted {removed_messages} older messages\n",
                    Style::new().yellow().apply_to("↻")
                ))?;
            }
            Event::SteeringDelivered { count, .. } => {
                if self.output.tui_entry(
                    "STEER",
                    format!(
                        "applied {count} steering message{}",
                        if *count == 1 { "" } else { "s" }
                    ),
                    TuiTone::Normal,
                ) {
                    return Ok(());
                }
                self.output.print(format!(
                    "  {} applied {count} steering message{}\n",
                    Style::new().cyan().apply_to("↪"),
                    if *count == 1 { "" } else { "s" }
                ))?;
            }
            Event::RunCancelled { .. } => {
                self.stop_spinner();
                if self.output.tui_entry(
                    "CANCELLED",
                    "workspace changes made before cancellation were preserved",
                    TuiTone::Warning,
                ) {
                    return Ok(());
                }
                self.output.print(format!(
                    "  {} cancelled · workspace changes made before cancellation were preserved\n",
                    Style::new().yellow().bold().apply_to("■")
                ))?;
            }
            Event::Verification { passed, .. } => {
                if self.output.tui_entry(
                    "VERIFY",
                    if *passed { "passed" } else { "failed" },
                    if *passed {
                        TuiTone::Success
                    } else {
                        TuiTone::Error
                    },
                ) {
                    return Ok(());
                }
                let text = if *passed {
                    Style::new()
                        .green()
                        .bold()
                        .apply_to("✓ verification passed")
                } else {
                    Style::new().red().bold().apply_to("✗ verification failed")
                };
                self.output.print(format!("  {text}\n"))?;
            }
            Event::RunCompleted {
                success,
                steps,
                duration_ms,
                patch_bytes,
                cache_hits,
                usage,
                ..
            } => {
                self.stop_spinner();
                self.output.set_tui_metrics(Some(format!(
                    "{} in · {} out · {} cached",
                    compact_number(usage.input_tokens),
                    compact_number(usage.output_tokens),
                    compact_number(usage.cache_read_tokens),
                )));
                let marker = if *success {
                    Style::new().green().bold().apply_to("✓ Done")
                } else {
                    Style::new().red().bold().apply_to("■ Stopped")
                };
                if self.output.tui_entry(
                    if *success { "DONE" } else { "STOPPED" },
                    format!(
                        "{steps} steps · {:.1}s · {} patch · {cache_hits} cache hits",
                        *duration_ms as f64 / 1000.0,
                        human_bytes(*patch_bytes as u64),
                    ),
                    if *success {
                        TuiTone::Success
                    } else {
                        TuiTone::Error
                    },
                ) {
                    return Ok(());
                }
                self.output.print(format!(
                    "  {marker} · {steps} steps · {:.1}s · {} patch · {cache_hits} cache hits\n\n",
                    *duration_ms as f64 / 1000.0,
                    human_bytes(*patch_bytes as u64),
                ))?;
            }
            Event::Error { message } => {
                self.stop_spinner();
                if self.output.tui_entry("ERROR", message, TuiTone::Error) {
                    return Ok(());
                }
                self.output
                    .print(render_panel("Error", message, PanelTone::Red, 12))?;
            }
            Event::AssistantMessage { .. } => {}
        }
        Ok(())
    }

    fn wants_model_deltas(&self) -> bool {
        self.allow_deltas && self.interactive
    }
}

#[derive(Clone, Copy)]
enum PanelTone {
    Cyan,
    Green,
    Yellow,
    Red,
    Dim,
}

fn render_panel(title: &str, content: &str, tone: PanelTone, max_lines: usize) -> String {
    let width = panel_width();
    let inner = width.saturating_sub(4).max(20);
    let title_style = match tone {
        PanelTone::Cyan => Style::new().cyan().bold(),
        PanelTone::Green => Style::new().green().bold(),
        PanelTone::Yellow => Style::new().yellow().bold(),
        PanelTone::Red => Style::new().red().bold(),
        PanelTone::Dim => Style::new().dim().bold(),
    };
    let line_style = match tone {
        PanelTone::Red => Style::new().red(),
        PanelTone::Dim => Style::new().dim(),
        _ => Style::new(),
    };
    let clean_title = truncate_chars(title, inner.saturating_sub(4));
    let rule = "─".repeat(width.saturating_sub(clean_title.chars().count() + 5));
    let mut output = format!(
        "{} {} {}\n",
        title_style.apply_to("╭─"),
        title_style.apply_to(clean_title),
        title_style.apply_to(rule),
    );
    for line in visible_lines(content, inner, max_lines) {
        let padding = " ".repeat(inner.saturating_sub(line.chars().count()));
        output.push_str(&format!(
            "{} {}{} {}\n",
            title_style.apply_to("│"),
            line_style.apply_to(line),
            padding,
            title_style.apply_to("│"),
        ));
    }
    output.push_str(&format!(
        "{}\n",
        title_style.apply_to(format!("╰{}╯", "─".repeat(width.saturating_sub(2))))
    ));
    output
}

fn panel_width() -> usize {
    let columns = Term::stderr().size().1 as usize;
    columns.clamp(36, 100)
}

fn visible_lines(content: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut result = Vec::new();
    for raw in content.lines() {
        let clean = strip_ansi_codes(raw).replace('\t', "    ");
        let mut chars = clean.chars().peekable();
        if chars.peek().is_none() {
            result.push(String::new());
        }
        while chars.peek().is_some() {
            result.push(chars.by_ref().take(width).collect());
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    if result.len() > max_lines {
        let omitted = result.len() - max_lines + 1;
        result.truncate(max_lines.saturating_sub(1));
        result.push(format!("… {omitted} more lines"));
    }
    result
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

fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn human_bytes(value: u64) -> String {
    if value >= 1_048_576 {
        format!("{:.1} MiB", value as f64 / 1_048_576.0)
    } else if value >= 1_024 {
        format!("{:.1} KiB", value as f64 / 1_024.0)
    } else {
        format!("{value} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_preview_is_bounded() {
        let output = (0..30)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = visible_lines(&output, 40, 5);
        assert_eq!(lines.len(), 5);
        assert!(lines.last().unwrap().contains("more lines"));
    }

    #[test]
    fn formats_compact_metrics() {
        assert_eq!(compact_number(999), "999");
        assert_eq!(compact_number(1_500), "1.5k");
        assert_eq!(human_bytes(2_048), "2.0 KiB");
    }

    #[test]
    fn benchmark_renderer_never_requests_model_deltas() {
        assert!(!TerminalUi::benchmark().wants_model_deltas());
    }
}
