use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::context::{ContextWindow, Message};
use crate::events::{Event, EventSink, JsonlSink};
use crate::executor::{ExecutionResult, Executor};
use crate::git::{collect_patch, head_id};
use crate::model::{CompletionRequest, Model, Usage, action_text};
use crate::protocol::{Action, parse_action};

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub verify: Option<String>,
    pub patch_out: Option<PathBuf>,
    pub result_out: Option<PathBuf>,
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunResult {
    pub session_id: String,
    pub task_id: Option<String>,
    pub success: bool,
    pub reason: String,
    pub summary: String,
    pub steps: usize,
    pub duration_ms: u128,
    pub patch: String,
    pub cache_hits: usize,
    pub usage: Usage,
}

pub struct Agent {
    config: Config,
    model: Box<dyn Model>,
    sink: Box<dyn EventSink>,
    workspace: PathBuf,
}

impl Agent {
    pub fn new(
        config: Config,
        model: Box<dyn Model>,
        sink: Box<dyn EventSink>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            config,
            model,
            sink,
            workspace,
        }
    }

    pub async fn run(&mut self, task: &str, options: RunOptions) -> Result<RunResult> {
        let started = Instant::now();
        let session_id = self.session_id(task).await;
        let recorder = self.open_recorder(&session_id)?;
        self.emit(
            &recorder,
            Event::RunStarted {
                session_id: session_id.clone(),
                task_id: options.task_id.clone(),
                workspace: self.workspace.display().to_string(),
                provider: self.config.model.provider.clone(),
                model: self.config.model.model.clone(),
            },
        )?;

        let executor = Executor::new(
            self.workspace.clone(),
            self.config.command_timeout(),
            self.config.agent.command_output_bytes,
            self.config.agent.deny_dangerous_commands,
            Some(self.config.model.api_key_env.clone()),
        );
        let context = ContextWindow::new(
            self.config.agent.context_max_tokens,
            self.config.agent.context_keep_messages,
        );
        let mut messages = vec![Message::user(initial_prompt(
            task,
            &self.workspace,
            options.verify.as_deref(),
        ))];
        let mut format_errors = 0;
        let mut verify_failures = 0;
        let mut total_usage = Usage::default();
        let mut cache_hits = 0;
        let mut summary = String::new();
        let mut reason = "step_limit".to_string();
        let mut success = false;
        let mut steps = 0;

        for step in 1..=self.config.agent.max_steps {
            if started.elapsed().as_secs() >= self.config.agent.wall_time_limit_seconds {
                reason = "wall_time_limit".into();
                break;
            }
            steps = step;
            let removed = context.compact(&mut messages);
            if removed > 0 {
                self.emit(
                    &recorder,
                    Event::ContextCompacted {
                        removed_messages: removed,
                    },
                )?;
            }
            self.emit(&recorder, Event::ModelStarted { step })?;
            let response = match self
                .model
                .complete(CompletionRequest {
                    system: SYSTEM_PROMPT.to_owned(),
                    messages: messages.clone(),
                    session_id: session_id.clone(),
                })
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    summary = format!("model request failed: {error:#}");
                    reason = "model_error".into();
                    self.emit(
                        &recorder,
                        Event::Error {
                            message: error.to_string(),
                        },
                    )?;
                    break;
                }
            };
            total_usage.add(response.usage);
            cache_hits += usize::from(response.cache_hit);
            self.emit(
                &recorder,
                Event::ModelCompleted {
                    step,
                    cache_hit: response.cache_hit,
                    usage: response.usage,
                },
            )?;
            let normalized_response = response
                .action
                .as_ref()
                .map(action_text)
                .unwrap_or_else(|| response.text.clone());
            self.emit(
                &recorder,
                Event::AssistantMessage {
                    step,
                    text: normalized_response.clone(),
                },
            )?;
            messages.push(Message::assistant(normalized_response));

            let action = match response.action {
                Some(action) => action,
                None => match parse_action(&response.text) {
                    Ok(action) => {
                        format_errors = 0;
                        action
                    }
                    Err(error) => {
                        format_errors += 1;
                        if format_errors >= self.config.agent.max_format_errors {
                            reason = "format_error_limit".into();
                            break;
                        }
                        let observation = format!(
                            "FORMAT ERROR: {error}\nReturn exactly one valid JSON action matching the system schema."
                        );
                        self.emit(
                            &recorder,
                            Event::ToolOutput {
                                step,
                                output: observation.clone(),
                            },
                        )?;
                        messages.push(Message::user(observation));
                        continue;
                    }
                },
            };
            self.emit(
                &recorder,
                Event::Action {
                    step,
                    kind: action.kind().into(),
                    description: action.description().into(),
                },
            )?;

            match action {
                Action::Shell { command, .. } => {
                    let result = executor.shell(&command).await;
                    self.handle_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Patch { patch, .. } => {
                    let result = executor.apply_patch(&patch).await;
                    self.handle_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Finish {
                    summary: proposed_summary,
                } => {
                    if let Some(verify) = &options.verify {
                        let result = executor.shell(verify).await?;
                        self.emit(
                            &recorder,
                            Event::Verification {
                                passed: result.success(),
                                exit_code: result.exit_code,
                            },
                        )?;
                        if !result.success()
                            && verify_failures < self.config.agent.verify_retries
                            && step < self.config.agent.max_steps
                        {
                            verify_failures += 1;
                            let observation = format!(
                                "FINAL VERIFICATION FAILED. Continue working and fix the failure.\n{}",
                                result.observation()
                            );
                            self.emit(
                                &recorder,
                                Event::ToolOutput {
                                    step,
                                    output: observation.clone(),
                                },
                            )?;
                            messages.push(Message::user(observation));
                            continue;
                        }
                        if !result.success() {
                            summary = proposed_summary;
                            reason = "verification_failed".into();
                            break;
                        }
                    }
                    success = true;
                    summary = proposed_summary;
                    reason = "finished".into();
                    break;
                }
            }
        }

        let patch = collect_patch(&self.workspace).await.unwrap_or_default();
        if let Some(path) = &options.patch_out {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, &patch)
                .await
                .with_context(|| format!("failed to write patch {}", path.display()))?;
        }
        let result = RunResult {
            session_id,
            task_id: options.task_id,
            success,
            reason: reason.clone(),
            summary,
            steps,
            duration_ms: started.elapsed().as_millis(),
            patch,
            cache_hits,
            usage: total_usage,
        };
        if let Some(path) = &options.result_out {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, serde_json::to_vec_pretty(&result)?)
                .await
                .with_context(|| format!("failed to write result {}", path.display()))?;
        }
        self.emit(
            &recorder,
            Event::RunCompleted {
                success,
                reason,
                steps,
                duration_ms: result.duration_ms,
                patch_bytes: result.patch.len(),
                cache_hits,
                usage: total_usage,
            },
        )?;
        Ok(result)
    }

    fn handle_execution(
        &self,
        step: usize,
        result: Result<ExecutionResult>,
        recorder: &JsonlSink,
        messages: &mut Vec<Message>,
    ) -> Result<()> {
        let observation = match result {
            Ok(result) => {
                self.emit(
                    recorder,
                    Event::ToolCompleted {
                        step,
                        exit_code: result.exit_code,
                        duration_ms: result.duration_ms,
                        truncated_bytes: result.truncated_bytes,
                    },
                )?;
                result.observation()
            }
            Err(error) => format!("TOOL ERROR: {error}"),
        };
        self.emit(
            recorder,
            Event::ToolOutput {
                step,
                output: observation.clone(),
            },
        )?;
        messages.push(Message::user(observation));
        Ok(())
    }

    async fn session_id(&self, task: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.workspace.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(task.as_bytes());
        hasher.update([0]);
        hasher.update(self.config.model.provider.as_bytes());
        hasher.update([0]);
        hasher.update(self.config.model.model.as_bytes());
        if let Some(head) = head_id(&self.workspace).await {
            hasher.update([0]);
            hasher.update(head.as_bytes());
        }
        if let Ok(worktree_patch) = collect_patch(&self.workspace).await {
            hasher.update([0]);
            hasher.update(worktree_patch.as_bytes());
        }
        format!("{:x}", hasher.finalize())[..24].to_owned()
    }

    fn open_recorder(&self, session_id: &str) -> Result<JsonlSink> {
        let directory = self.config.agent.trajectory_directory.clone();
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{session_id}.jsonl"));
        let file = File::create(path)?;
        Ok(JsonlSink::new(Box::new(file)))
    }

    fn emit(&self, recorder: &JsonlSink, event: Event) -> Result<()> {
        self.sink.emit(&event)?;
        recorder.emit(&event)
    }
}

fn initial_prompt(task: &str, workspace: &std::path::Path, verify: Option<&str>) -> String {
    let mut prompt = format!(
        "Task:\n{task}\n\nWorkspace: {}\nPlatform: {} / {}\n",
        workspace.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Some(verify) = verify {
        prompt.push_str("\nThe controller will require this final verification command to pass:\n");
        prompt.push_str(verify);
        prompt.push('\n');
    }
    prompt
}
