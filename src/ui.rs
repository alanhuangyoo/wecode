use std::io::{self, IsTerminal};
use std::sync::Mutex;

use anyhow::Result;
use console::{Style, Term, strip_ansi_codes};
use indicatif::{ProgressBar, ProgressStyle};

use crate::events::{Event, EventSink};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiMode {
    #[default]
    Run,
    Chat,
}

pub struct TerminalUi {
    spinner: Mutex<Option<ProgressBar>>,
    interactive: bool,
    mode: UiMode,
}

impl TerminalUi {
    pub fn new() -> Self {
        Self::with_mode(UiMode::Run)
    }

    pub fn chat() -> Self {
        Self::with_mode(UiMode::Chat)
    }

    fn with_mode(mode: UiMode) -> Self {
        Self {
            spinner: Mutex::new(None),
            interactive: io::stderr().is_terminal(),
            mode,
        }
    }

    fn stop_spinner(&self) {
        if let Some(spinner) = self.spinner.lock().expect("spinner lock poisoned").take() {
            spinner.finish_and_clear();
        }
    }

    fn start_spinner(&self, step: usize) {
        self.stop_spinner();
        if self.interactive {
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
            eprintln!("  Thinking · step {step}");
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
                    print_panel(
                        "WeCode",
                        &format!("{provider} / {model}\n{workspace}"),
                        PanelTone::Cyan,
                        4,
                    );
                }
            }
            Event::ModelStarted { step } => self.start_spinner(*step),
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
                eprintln!(
                    "  {} step {step} · {} in · {} out · {} cached{cache}",
                    Style::new().green().apply_to("✓"),
                    compact_number(usage.input_tokens),
                    compact_number(usage.output_tokens),
                    compact_number(usage.cache_read_tokens),
                );
            }
            Event::Action {
                kind,
                description,
                detail,
                ..
            } => {
                self.stop_spinner();
                match kind.as_str() {
                    "shell" => print_panel(
                        &format!("Shell · {description}"),
                        detail,
                        PanelTone::Cyan,
                        8,
                    ),
                    "patch" => print_panel(
                        &format!("Edit · {description}"),
                        detail,
                        PanelTone::Yellow,
                        8,
                    ),
                    "finish" => print_panel("WeCode", detail, PanelTone::Green, 20),
                    _ => print_panel(kind, detail, PanelTone::Cyan, 8),
                }
            }
            Event::ToolCompleted {
                exit_code,
                duration_ms,
                truncated_bytes,
                ..
            } => {
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
                eprintln!(
                    "  ↳ {status} · {:.1}s{truncated}",
                    *duration_ms as f64 / 1000.0
                );
            }
            Event::ToolOutput { output, .. } => {
                print_panel("Output", output, PanelTone::Dim, 14);
            }
            Event::ContextCompacted { removed_messages } => {
                eprintln!(
                    "  {} compacted {removed_messages} older messages",
                    Style::new().yellow().apply_to("↻")
                );
            }
            Event::Verification { passed, .. } => {
                let text = if *passed {
                    Style::new()
                        .green()
                        .bold()
                        .apply_to("✓ verification passed")
                } else {
                    Style::new().red().bold().apply_to("✗ verification failed")
                };
                eprintln!("  {text}");
            }
            Event::RunCompleted {
                success,
                steps,
                duration_ms,
                patch_bytes,
                cache_hits,
                ..
            } => {
                self.stop_spinner();
                let marker = if *success {
                    Style::new().green().bold().apply_to("✓ Done")
                } else {
                    Style::new().red().bold().apply_to("■ Stopped")
                };
                eprintln!(
                    "  {marker} · {steps} steps · {:.1}s · {} patch · {cache_hits} cache hits\n",
                    *duration_ms as f64 / 1000.0,
                    human_bytes(*patch_bytes as u64),
                );
            }
            Event::Error { message } => {
                self.stop_spinner();
                print_panel("Error", message, PanelTone::Red, 12);
            }
            Event::AssistantMessage { .. } => {}
        }
        Ok(())
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

fn print_panel(title: &str, content: &str, tone: PanelTone, max_lines: usize) {
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
    eprintln!(
        "{} {} {}",
        title_style.apply_to("╭─"),
        title_style.apply_to(clean_title),
        title_style.apply_to(rule),
    );
    for line in visible_lines(content, inner, max_lines) {
        let padding = " ".repeat(inner.saturating_sub(line.chars().count()));
        eprintln!(
            "{} {}{} {}",
            title_style.apply_to("│"),
            line_style.apply_to(line),
            padding,
            title_style.apply_to("│"),
        );
    }
    eprintln!(
        "{}",
        title_style.apply_to(format!("╰{}╯", "─".repeat(width.saturating_sub(2))))
    );
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
}
