use std::fs::{File, OpenOptions};
use std::future::Future;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::approval::{ApprovalClient, ApprovalDecision, ApprovalKind, RiskLevel, classify_shell};
use crate::config::Config;
use crate::context::{ContextWindow, Message};
use crate::control::CancellationToken;
use crate::events::{Event, EventSink, JsonlSink};
use crate::executor::{ExecutionResult, Executor};
use crate::file_tools::FileTools;
use crate::git::{collect_patch, head_id};
use crate::input_queue::{InputQueue, QueuedInput};
use crate::instructions;
use crate::model::{CompletionRequest, Model, ModelStream, ModelStreamEvent, Usage, action_text};
use crate::protocol::{Action, parse_action};

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub verify: Option<String>,
    pub patch_out: Option<PathBuf>,
    pub result_out: Option<PathBuf>,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub cancellation: Option<CancellationToken>,
    pub input_queue: Option<InputQueue>,
    pub approval: Option<ApprovalClient>,
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

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }
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
        self.run_inner(task, options, None).await
    }

    pub async fn run_in_conversation(
        &mut self,
        task: &str,
        options: RunOptions,
        conversation: &mut Conversation,
    ) -> Result<RunResult> {
        self.run_inner(task, options, Some(conversation)).await
    }

    async fn run_inner(
        &mut self,
        task: &str,
        options: RunOptions,
        conversation: Option<&mut Conversation>,
    ) -> Result<RunResult> {
        let started = Instant::now();
        let persistent_session = options.session_id.is_some();
        let session_id = match &options.session_id {
            Some(session_id) => session_id.clone(),
            None => self.session_id(task).await,
        };
        let recorder = self.open_recorder(&session_id, persistent_session)?;
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
        let file_tools = FileTools::new(
            self.workspace.clone(),
            self.config.agent.command_output_bytes,
        );
        let context = ContextWindow::new(
            self.config.agent.context_max_tokens,
            self.config.agent.context_keep_messages,
        );
        let mut messages = match conversation.as_deref() {
            Some(conversation) if !conversation.messages.is_empty() => {
                let mut messages = conversation.messages.clone();
                messages.push(Message::user(follow_up_prompt(
                    task,
                    options.verify.as_deref(),
                )));
                messages
            }
            _ => vec![Message::user(initial_prompt(
                task,
                &self.workspace,
                options.verify.as_deref(),
            )?)],
        };
        let mut format_errors = 0;
        let mut verify_failures = 0;
        let mut total_usage = Usage::default();
        let mut cache_hits = 0;
        let mut summary = String::new();
        let mut reason = "step_limit".to_string();
        let mut success = false;
        let mut steps = 0;
        let cancellation = options.cancellation.clone();
        let input_queue = options.input_queue.clone();
        let approval = options.approval.clone();

        for step in 1..=self.config.agent.max_steps {
            if cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                summary = "cancelled by user".into();
                reason = "cancelled".into();
                self.emit(&recorder, Event::RunCancelled { step })?;
                break;
            }
            if started.elapsed().as_secs() >= self.config.agent.wall_time_limit_seconds {
                reason = "wall_time_limit".into();
                break;
            }
            steps = step;
            self.deliver_steering(step, &input_queue, &recorder, &mut messages)?;
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
            let stream = self.sink.wants_model_deltas().then(|| EventModelStream {
                sink: self.sink.as_ref(),
                step,
            });
            let request = CompletionRequest {
                system: SYSTEM_PROMPT.to_owned(),
                messages: messages.clone(),
                session_id: session_id.clone(),
            };
            let completion = self.model.complete(
                request,
                stream.as_ref().map(|stream| stream as &dyn ModelStream),
            );
            let response = if let Some(cancellation) = &cancellation {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => None,
                    response = completion => Some(response),
                }
            } else {
                Some(completion.await)
            };
            let Some(response) = response else {
                summary = "cancelled by user".into();
                reason = "cancelled".into();
                self.emit(&recorder, Event::RunCancelled { step })?;
                break;
            };
            let response = match response {
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
                    detail: action_detail(&action),
                },
            )?;

            match action {
                Action::ReadFile {
                    path,
                    offset,
                    limit,
                } => {
                    let result = cancellable_execution(
                        &cancellation,
                        file_tools.read_file(&path, offset, limit),
                    )
                    .await;
                    let Some(result) = result else {
                        summary = "cancelled by user".into();
                        reason = "cancelled".into();
                        self.emit(&recorder, Event::RunCancelled { step })?;
                        break;
                    };
                    self.handle_file_execution(step, result, &recorder, &mut messages)?;
                }
                Action::ListFiles { path, depth, limit } => {
                    let result = cancellable_execution(
                        &cancellation,
                        file_tools.list_files(&path, depth, limit),
                    )
                    .await;
                    let Some(result) = result else {
                        summary = "cancelled by user".into();
                        reason = "cancelled".into();
                        self.emit(&recorder, Event::RunCancelled { step })?;
                        break;
                    };
                    self.handle_file_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Glob {
                    pattern,
                    path,
                    limit,
                } => {
                    let result = cancellable_execution(
                        &cancellation,
                        file_tools.glob(&pattern, &path, limit),
                    )
                    .await;
                    let Some(result) = result else {
                        summary = "cancelled by user".into();
                        reason = "cancelled".into();
                        self.emit(&recorder, Event::RunCancelled { step })?;
                        break;
                    };
                    self.handle_file_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Grep {
                    pattern,
                    path,
                    glob,
                    literal,
                    ignore_case,
                    context,
                    limit,
                } => {
                    let result = cancellable_execution(
                        &cancellation,
                        file_tools.grep(
                            &pattern,
                            &path,
                            glob.as_deref(),
                            literal,
                            ignore_case,
                            context,
                            limit,
                        ),
                    )
                    .await;
                    let Some(result) = result else {
                        summary = "cancelled by user".into();
                        reason = "cancelled".into();
                        self.emit(&recorder, Event::RunCancelled { step })?;
                        break;
                    };
                    self.handle_file_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Shell { command, .. } => {
                    match self
                        .authorize(
                            step,
                            ApprovalKind::Shell,
                            classify_shell(&command),
                            "Run shell command",
                            &command,
                            format!("shell:{command}"),
                            &approval,
                            &cancellation,
                            &recorder,
                        )
                        .await?
                    {
                        Authorization::Allowed => {}
                        Authorization::Denied(reason) => {
                            self.handle_permission_denial(step, &reason, &recorder, &mut messages)?;
                            continue;
                        }
                        Authorization::Cancelled => {
                            summary = "cancelled by user".into();
                            reason = "cancelled".into();
                            self.emit(&recorder, Event::RunCancelled { step })?;
                            break;
                        }
                    }
                    let result =
                        cancellable_execution(&cancellation, executor.shell(&command)).await;
                    let Some(result) = result else {
                        summary = "cancelled by user".into();
                        reason = "cancelled".into();
                        self.emit(&recorder, Event::RunCancelled { step })?;
                        break;
                    };
                    self.handle_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Patch { patch, .. } => {
                    match self
                        .authorize(
                            step,
                            ApprovalKind::Patch,
                            RiskLevel::WorkspaceWrite,
                            "Apply workspace patch",
                            &action_detail(&Action::Patch {
                                patch: patch.clone(),
                                description: String::new(),
                            }),
                            "patch:workspace",
                            &approval,
                            &cancellation,
                            &recorder,
                        )
                        .await?
                    {
                        Authorization::Allowed => {}
                        Authorization::Denied(reason) => {
                            self.handle_permission_denial(step, &reason, &recorder, &mut messages)?;
                            continue;
                        }
                        Authorization::Cancelled => {
                            summary = "cancelled by user".into();
                            reason = "cancelled".into();
                            self.emit(&recorder, Event::RunCancelled { step })?;
                            break;
                        }
                    }
                    let result = executor.apply_patch(&patch).await;
                    self.handle_execution(step, result, &recorder, &mut messages)?;
                }
                Action::Finish {
                    summary: proposed_summary,
                } => {
                    if self.deliver_steering(step, &input_queue, &recorder, &mut messages)? > 0 {
                        continue;
                    }
                    if let Some(verify) = &options.verify {
                        let verification = executor.shell(verify);
                        let result = if let Some(cancellation) = &cancellation {
                            tokio::select! {
                                biased;
                                _ = cancellation.cancelled() => None,
                                result = verification => Some(result),
                            }
                        } else {
                            Some(verification.await)
                        };
                        let Some(result) = result else {
                            summary = "cancelled by user".into();
                            reason = "cancelled".into();
                            self.emit(&recorder, Event::RunCancelled { step })?;
                            break;
                        };
                        let result = result?;
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
        if let Some(conversation) = conversation {
            conversation.messages = messages;
        }
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
        self.handle_execution_inner(step, result, recorder, messages, false)
    }

    fn handle_file_execution(
        &self,
        step: usize,
        result: Result<ExecutionResult>,
        recorder: &JsonlSink,
        messages: &mut Vec<Message>,
    ) -> Result<()> {
        self.handle_execution_inner(step, result, recorder, messages, true)
    }

    fn handle_execution_inner(
        &self,
        step: usize,
        result: Result<ExecutionResult>,
        recorder: &JsonlSink,
        messages: &mut Vec<Message>,
        compact_output: bool,
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
                if compact_output {
                    let mut output = result.stdout;
                    if !result.stderr.is_empty() {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&result.stderr);
                    }
                    output
                } else {
                    result.observation()
                }
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

    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        step: usize,
        kind: ApprovalKind,
        risk: RiskLevel,
        summary: &str,
        detail: &str,
        fingerprint: impl Into<String>,
        approval: &Option<ApprovalClient>,
        cancellation: &Option<CancellationToken>,
        recorder: &JsonlSink,
    ) -> Result<Authorization> {
        if !self
            .config
            .agent
            .approval_policy
            .requires_approval(kind, risk)
        {
            return Ok(Authorization::Allowed);
        }
        let Some(approval) = approval else {
            return Ok(Authorization::Denied(format!(
                "{} action requires interactive approval under policy {:?}, but no approval channel is available",
                kind.as_str(),
                self.config.agent.approval_policy
            )));
        };
        let request = approval.prepare(kind, risk, summary, detail, fingerprint.into());
        self.emit(
            recorder,
            Event::ApprovalRequested {
                id: request.id,
                step,
                kind: kind.as_str().into(),
                risk: risk.as_str().into(),
                summary: request.summary.clone(),
                detail: request.detail.clone(),
            },
        )?;
        let decision = approval.request(request.clone());
        let decision = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(Authorization::Cancelled),
                decision = decision => decision,
            }
        } else {
            decision.await
        };
        self.emit(
            recorder,
            Event::ApprovalResolved {
                id: request.id,
                step,
                decision: decision.as_str().into(),
            },
        )?;
        Ok(match decision {
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowSession => Authorization::Allowed,
            ApprovalDecision::Deny { reason } => Authorization::Denied(reason),
        })
    }

    fn handle_permission_denial(
        &self,
        step: usize,
        reason: &str,
        recorder: &JsonlSink,
        messages: &mut Vec<Message>,
    ) -> Result<()> {
        let observation = format!(
            "PERMISSION DENIED: {reason}\nChoose a safer approach or explain why the requested operation is necessary."
        );
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

    fn deliver_steering(
        &self,
        step: usize,
        input_queue: &Option<InputQueue>,
        recorder: &JsonlSink,
        messages: &mut Vec<Message>,
    ) -> Result<usize> {
        let Some(input_queue) = input_queue else {
            return Ok(0);
        };
        let inputs = input_queue.take_steering(self.config.agent.steering_mode.take_all());
        if inputs.is_empty() {
            return Ok(0);
        }
        let count = inputs.len();
        messages.push(Message::user(steering_prompt(&inputs)));
        self.emit(recorder, Event::SteeringDelivered { step, count })?;
        Ok(count)
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

    fn open_recorder(&self, session_id: &str, append: bool) -> Result<JsonlSink> {
        let directory = self.config.agent.trajectory_directory.clone();
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{session_id}.jsonl"));
        let file = if append {
            OpenOptions::new().create(true).append(true).open(path)?
        } else {
            File::create(path)?
        };
        Ok(JsonlSink::new(Box::new(file)))
    }

    fn emit(&self, recorder: &JsonlSink, event: Event) -> Result<()> {
        self.sink.emit(&event)?;
        recorder.emit(&event)
    }
}

enum Authorization {
    Allowed,
    Denied(String),
    Cancelled,
}

struct EventModelStream<'a> {
    sink: &'a dyn EventSink,
    step: usize,
}

impl ModelStream for EventModelStream<'_> {
    fn emit(&self, event: ModelStreamEvent) -> Result<()> {
        let (text, reasoning) = match event {
            ModelStreamEvent::TextDelta(text) => (text, false),
            ModelStreamEvent::ReasoningDelta(text) => (text, true),
        };
        self.sink.emit(&Event::ModelDelta {
            step: self.step,
            text,
            reasoning,
        })
    }
}

fn initial_prompt(task: &str, workspace: &std::path::Path, verify: Option<&str>) -> Result<String> {
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
    prompt.push_str(&instructions::discover(workspace)?.render());
    Ok(prompt)
}

fn follow_up_prompt(task: &str, verify: Option<&str>) -> String {
    let mut prompt = format!("Follow-up request:\n{task}\n");
    if let Some(verify) = verify {
        prompt.push_str("\nThe controller will require this final verification command to pass:\n");
        prompt.push_str(verify);
        prompt.push('\n');
    }
    prompt
}

fn steering_prompt(inputs: &[QueuedInput]) -> String {
    let mut prompt = String::from(
        "The user sent this steering update while the current task was running. Apply it to the current work:\n",
    );
    for input in inputs {
        prompt.push_str("\n- ");
        prompt.push_str(input.text.trim());
    }
    prompt
}

fn action_detail(action: &Action) -> String {
    match action {
        Action::ReadFile {
            path,
            offset,
            limit,
        } => match (offset, limit) {
            (Some(offset), Some(limit)) => {
                format!(
                    "{path}:{offset}-{}",
                    offset.saturating_add(*limit).saturating_sub(1)
                )
            }
            (Some(offset), None) => format!("{path}:{offset}"),
            _ => path.clone(),
        },
        Action::ListFiles { path, depth, .. } => {
            format!("{path} · depth {}", depth.unwrap_or(2))
        }
        Action::Glob { pattern, path, .. } => format!("{pattern} in {path}"),
        Action::Grep {
            pattern,
            path,
            glob,
            ..
        } => {
            let filter = glob
                .as_deref()
                .map(|glob| format!(" · {glob}"))
                .unwrap_or_default();
            format!("/{pattern}/ in {path}{filter}")
        }
        Action::Shell { command, .. } => command.clone(),
        Action::Patch { patch, .. } => {
            let files = patch
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>();
            if files.is_empty() {
                "workspace patch".into()
            } else {
                files.join(", ")
            }
        }
        Action::Finish { summary } => summary.clone(),
    }
}

async fn cancellable_execution<F>(
    cancellation: &Option<CancellationToken>,
    execution: F,
) -> Option<Result<ExecutionResult>>
where
    F: Future<Output = Result<ExecutionResult>>,
{
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            result = execution => Some(result),
        }
    } else {
        Some(execution.await)
    }
}
