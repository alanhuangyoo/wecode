use std::io::{self, IsTerminal};
use std::sync::Mutex;

use anyhow::Result;
use console::Style;
use indicatif::{ProgressBar, ProgressStyle};

use crate::events::{Event, EventSink};

pub struct TerminalUi {
    spinner: Mutex<Option<ProgressBar>>,
    interactive: bool,
}

impl TerminalUi {
    pub fn new() -> Self {
        Self {
            spinner: Mutex::new(None),
            interactive: io::stderr().is_terminal(),
        }
    }

    fn stop_spinner(&self) {
        if let Some(spinner) = self.spinner.lock().expect("spinner lock poisoned").take() {
            spinner.finish_and_clear();
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
                eprintln!(
                    "{} {} / {}  {}",
                    Style::new().cyan().bold().apply_to("wecode"),
                    provider,
                    model,
                    Style::new().dim().apply_to(workspace)
                );
            }
            Event::ModelStarted { step } => {
                if self.interactive {
                    let spinner = ProgressBar::new_spinner();
                    spinner.set_style(
                        ProgressStyle::with_template("{spinner:.cyan} {msg}")
                            .expect("valid spinner template")
                            .tick_strings(&["·  ", "·· ", "···"]),
                    );
                    spinner.set_message(format!("step {step}: thinking"));
                    spinner.enable_steady_tick(std::time::Duration::from_millis(160));
                    *self.spinner.lock().expect("spinner lock poisoned") = Some(spinner);
                } else {
                    eprintln!("step {step}: thinking");
                }
            }
            Event::ModelCompleted {
                step,
                cache_hit,
                usage,
            } => {
                self.stop_spinner();
                let cache = if *cache_hit { " exact-cache-hit" } else { "" };
                eprintln!(
                    "{} step {step}: model {} in / {} out / {} cached{}",
                    Style::new().green().apply_to("✓"),
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    cache
                );
            }
            Event::Action {
                kind, description, ..
            } => {
                eprintln!(
                    "{} {}  {}",
                    Style::new().cyan().apply_to("›"),
                    Style::new().bold().apply_to(kind),
                    description
                );
            }
            Event::ToolOutput { output, .. } => {
                eprintln!("{}", Style::new().dim().apply_to(output));
            }
            Event::ContextCompacted { removed_messages } => {
                eprintln!("compacted {removed_messages} older messages");
            }
            Event::Verification { passed, .. } => {
                let text = if *passed {
                    Style::new().green().apply_to("verification passed")
                } else {
                    Style::new().red().apply_to("verification failed")
                };
                eprintln!("{text}");
            }
            Event::RunCompleted {
                success,
                steps,
                duration_ms,
                patch_bytes,
                ..
            } => {
                self.stop_spinner();
                let marker = if *success {
                    Style::new().green().bold().apply_to("completed")
                } else {
                    Style::new().red().bold().apply_to("stopped")
                };
                eprintln!(
                    "{marker}: {steps} steps, {:.1}s, {} patch bytes",
                    *duration_ms as f64 / 1000.0,
                    patch_bytes
                );
            }
            Event::Error { message } => {
                self.stop_spinner();
                eprintln!("{} {message}", Style::new().red().bold().apply_to("error:"));
            }
            Event::AssistantMessage { .. } | Event::ToolCompleted { .. } => {}
        }
        Ok(())
    }
}
