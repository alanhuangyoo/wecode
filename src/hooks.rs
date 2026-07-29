use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::{HookCommandConfig, HooksConfig};
use crate::executor::{ExecutionResult, Executor};

const MAX_ADDITIONAL_CONTEXT_BYTES: usize = 16 * 1_024;
const MAX_PROMPT_BYTES: usize = 64 * 1_024;
const MAX_STOP_REASON_BYTES: usize = 4 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    Stop,
    SessionEnd,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: PathBuf,
    pub hook_event_name: &'static str,
    pub provider: String,
    pub model: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

impl HookInput {
    pub fn new(
        event: HookEvent,
        session_id: impl Into<String>,
        cwd: PathBuf,
        provider: impl Into<String>,
        model: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            cwd,
            hook_event_name: event.as_str(),
            provider: provider.into(),
            model: model.into(),
            source: source.into(),
            prompt: None,
            stop_reason: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookStatus {
    Started,
    Completed,
    Failed,
    Blocked,
    TimedOut,
}

impl HookStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::TimedOut => "timed out",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookReport {
    pub event: HookEvent,
    pub index: usize,
    pub label: String,
    pub status: HookStatus,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    pub suppress_output: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HookOutcome {
    pub blocked: bool,
    pub reason: Option<String>,
    pub additional_context: Vec<String>,
    pub reports: Vec<HookReport>,
}

#[derive(Clone, Debug)]
pub struct HookSummary {
    pub event: HookEvent,
    pub index: usize,
    pub label: String,
    pub matcher: Option<String>,
    pub asynchronous: bool,
    pub fail_closed: bool,
}

#[derive(Clone)]
pub struct HookRunner {
    inner: Arc<HookRunnerInner>,
}

struct HookRunnerInner {
    config: HooksConfig,
    workspace: PathBuf,
    secret_env: Option<String>,
}

impl HookRunner {
    pub fn new(config: HooksConfig, workspace: PathBuf, secret_env: Option<String>) -> Self {
        Self {
            inner: Arc::new(HookRunnerInner {
                config,
                workspace,
                secret_env,
            }),
        }
    }

    pub fn len(&self) -> usize {
        if self.inner.config.enabled {
            self.inner
                .config
                .command_count()
                .saturating_sub(self.disabled_count())
        } else {
            0
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn summaries(&self) -> Vec<HookSummary> {
        if !self.inner.config.enabled {
            return Vec::new();
        }
        let mut summaries = Vec::new();
        for event in [
            HookEvent::SessionStart,
            HookEvent::UserPromptSubmit,
            HookEvent::Stop,
            HookEvent::SessionEnd,
        ] {
            for (index, hook) in self.handlers(event).iter().enumerate() {
                if !hook.enabled {
                    continue;
                }
                summaries.push(HookSummary {
                    event,
                    index: index + 1,
                    label: hook_label(hook, index),
                    matcher: hook.matcher.clone(),
                    asynchronous: hook.r#async,
                    fail_closed: hook.fail_closed,
                });
            }
        }
        summaries
    }

    pub async fn run(&self, event: HookEvent, mut input: HookInput) -> Result<HookOutcome> {
        if !self.inner.config.enabled {
            return Ok(HookOutcome::default());
        }
        input.prompt = input
            .prompt
            .map(|prompt| truncate_bytes(&prompt, MAX_PROMPT_BYTES));
        input.stop_reason = input
            .stop_reason
            .map(|reason| truncate_bytes(&reason, MAX_STOP_REASON_BYTES));
        let matcher_value = matcher_value(event, &input);
        let input_json = serde_json::to_vec(&input).context("failed to serialize hook input")?;
        let mut outcome = HookOutcome::default();
        for (index, hook) in self.handlers(event).iter().enumerate() {
            if !hook.enabled || !matches_hook(hook, matcher_value)? {
                continue;
            }
            if hook.r#async {
                let label = hook_label(hook, index);
                let runner = self.clone();
                let hook = hook.clone();
                let input_json = input_json.clone();
                tokio::spawn(async move {
                    let _ = runner.execute(event, index, &hook, &input_json).await;
                });
                outcome.reports.push(HookReport {
                    event,
                    index: index + 1,
                    label,
                    status: HookStatus::Started,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    suppress_output: true,
                });
                continue;
            }
            let executed = self.execute(event, index, hook, &input_json).await;
            let parsed = match executed {
                Ok(parsed) => parsed,
                Err(error) => ParsedHook {
                    report: HookReport {
                        event,
                        index: index + 1,
                        label: hook_label(hook, index),
                        status: HookStatus::Failed,
                        duration_ms: 0,
                        stdout: String::new(),
                        stderr: error.to_string(),
                        suppress_output: false,
                    },
                    blocked: hook.fail_closed,
                    reason: hook
                        .fail_closed
                        .then(|| format!("hook failed closed: {error}")),
                    additional_context: None,
                },
            };
            if let Some(context) = parsed.additional_context {
                outcome.additional_context.push(context);
            }
            if parsed.blocked {
                outcome.blocked = true;
                if outcome.reason.is_none() {
                    outcome.reason = parsed.reason;
                }
            }
            outcome.reports.push(parsed.report);
            if outcome.blocked {
                break;
            }
        }
        Ok(outcome)
    }

    fn handlers(&self, event: HookEvent) -> &[HookCommandConfig] {
        match event {
            HookEvent::SessionStart => &self.inner.config.session_start,
            HookEvent::UserPromptSubmit => &self.inner.config.user_prompt_submit,
            HookEvent::Stop => &self.inner.config.stop,
            HookEvent::SessionEnd => &self.inner.config.session_end,
        }
    }

    fn disabled_count(&self) -> usize {
        [
            &self.inner.config.session_start,
            &self.inner.config.user_prompt_submit,
            &self.inner.config.stop,
            &self.inner.config.session_end,
        ]
        .into_iter()
        .flatten()
        .filter(|hook| !hook.enabled)
        .count()
    }

    async fn execute(
        &self,
        event: HookEvent,
        index: usize,
        hook: &HookCommandConfig,
        input_json: &[u8],
    ) -> Result<ParsedHook> {
        let command = command_for_platform(hook);
        let executor = Executor::new(
            self.inner.workspace.clone(),
            Duration::from_secs(hook.timeout_seconds),
            self.inner.config.max_output_bytes,
            false,
            self.inner.secret_env.clone(),
        );
        let result = executor
            .shell_with_input(command, input_json)
            .await
            .with_context(|| format!("failed to run {} hook", event.as_str()))?;
        Ok(parse_result(event, index, hook, result))
    }
}

struct ParsedHook {
    report: HookReport,
    blocked: bool,
    reason: Option<String>,
    additional_context: Option<String>,
}

fn parse_result(
    event: HookEvent,
    index: usize,
    hook: &HookCommandConfig,
    result: ExecutionResult,
) -> ParsedHook {
    let mut status = if result.timed_out {
        HookStatus::TimedOut
    } else if result.exit_code == Some(0) {
        HookStatus::Completed
    } else if result.exit_code == Some(2) {
        HookStatus::Blocked
    } else {
        HookStatus::Failed
    };
    let mut blocked =
        status == HookStatus::Blocked || (hook.fail_closed && status != HookStatus::Completed);
    let mut reason = blocked.then(|| {
        nonempty(&result.stderr)
            .or_else(|| nonempty(&result.stdout))
            .unwrap_or_else(|| format!("{} hook blocked the operation", event.as_str()))
    });
    let mut additional_context = None;
    let mut suppress_output = false;

    let trimmed = result.stdout.trim();
    if result.exit_code == Some(0) && trimmed.starts_with('{') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                let continue_run = value
                    .get("continue")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let decision = value.get("decision").and_then(Value::as_str);
                let permission_decision = value
                    .pointer("/hookSpecificOutput/permissionDecision")
                    .or_else(|| value.pointer("/hook_specific_output/permission_decision"))
                    .and_then(Value::as_str);
                if !continue_run
                    || matches!(decision, Some("block" | "deny"))
                    || permission_decision == Some("deny")
                {
                    blocked = true;
                    status = HookStatus::Blocked;
                }
                reason = string_field(&value, &["reason", "stopReason", "stop_reason"])
                    .or_else(|| {
                        string_pointer(
                            &value,
                            &[
                                "/hookSpecificOutput/permissionDecisionReason",
                                "/hook_specific_output/permission_decision_reason",
                            ],
                        )
                    })
                    .or(reason);
                additional_context =
                    string_field(&value, &["additionalContext", "additional_context"]).or_else(
                        || {
                            string_pointer(
                                &value,
                                &[
                                    "/hookSpecificOutput/additionalContext",
                                    "/hook_specific_output/additional_context",
                                ],
                            )
                        },
                    );
                suppress_output = value
                    .get("suppressOutput")
                    .or_else(|| value.get("suppress_output"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            Err(error) if hook.fail_closed => {
                blocked = true;
                status = HookStatus::Blocked;
                reason = Some(format!("hook returned invalid JSON: {error}"));
            }
            Err(_) => {
                status = HookStatus::Failed;
            }
        }
    }
    additional_context = additional_context
        .map(|context| truncate_bytes(&context, MAX_ADDITIONAL_CONTEXT_BYTES))
        .filter(|context| !context.trim().is_empty());
    if blocked
        && reason
            .as_ref()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        reason = Some(format!("{} hook blocked the operation", event.as_str()));
    }
    ParsedHook {
        report: HookReport {
            event,
            index: index + 1,
            label: hook_label(hook, index),
            status,
            duration_ms: result.duration_ms,
            stdout: result.stdout,
            stderr: result.stderr,
            suppress_output,
        },
        blocked,
        reason,
        additional_context,
    }
}

fn matches_hook(hook: &HookCommandConfig, value: &str) -> Result<bool> {
    hook.matcher
        .as_ref()
        .map(|pattern| {
            Regex::new(pattern)
                .with_context(|| format!("invalid hook matcher {pattern:?}"))
                .map(|matcher| matcher.is_match(value))
        })
        .unwrap_or(Ok(true))
}

fn matcher_value(event: HookEvent, input: &HookInput) -> &str {
    match event {
        HookEvent::SessionStart | HookEvent::SessionEnd => &input.source,
        HookEvent::UserPromptSubmit => input.prompt.as_deref().unwrap_or_default(),
        HookEvent::Stop => input.stop_reason.as_deref().unwrap_or_default(),
    }
}

fn command_for_platform(hook: &HookCommandConfig) -> &str {
    #[cfg(windows)]
    if let Some(command) = &hook.command_windows {
        return command;
    }
    &hook.command
}

fn hook_label(hook: &HookCommandConfig, index: usize) -> String {
    hook.status_message
        .clone()
        .unwrap_or_else(|| format!("hook {}", index + 1))
}

fn nonempty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

fn string_field(value: &Value, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(Value::as_str))
        .and_then(nonempty)
}

fn string_pointer(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .and_then(nonempty)
}

fn truncate_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    const ELLIPSIS: &str = "…";
    let mut end = limit.saturating_sub(ELLIPSIS.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &value[..end])
}

pub fn append_context(prompt: &str, contexts: &[String]) -> String {
    if contexts.is_empty() {
        return prompt.to_owned();
    }
    format!(
        "{prompt}\n\n<hook_context>\n{}\n</hook_context>",
        contexts.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prompt_hook_receives_json_and_adds_context() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let command = r#"input=$(cat); case "$input" in *UserPromptSubmit*) printf '%s' '{"additionalContext":"review carefully"}';; *) exit 9;; esac"#;
        #[cfg(windows)]
        let command = "echo {}";
        let config = HooksConfig {
            user_prompt_submit: vec![HookCommandConfig {
                command: command.into(),
                fail_closed: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let runner = HookRunner::new(config, temp.path().into(), None);
        let mut input = HookInput::new(
            HookEvent::UserPromptSubmit,
            "session",
            temp.path().into(),
            "provider",
            "model",
            "interactive",
        );
        input.prompt = Some("review".into());
        let outcome = runner
            .run(HookEvent::UserPromptSubmit, input)
            .await
            .unwrap();
        #[cfg(unix)]
        {
            assert!(!outcome.blocked);
            assert_eq!(outcome.additional_context, vec!["review carefully"]);
            assert_eq!(outcome.reports[0].status, HookStatus::Completed);
        }
        #[cfg(windows)]
        {
            assert!(!outcome.blocked);
            assert_eq!(outcome.reports[0].status, HookStatus::Completed);
        }
    }

    #[tokio::test]
    async fn exit_two_and_json_decisions_block() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let commands = vec![
            HookCommandConfig {
                command: "printf blocked >&2; exit 2".into(),
                ..Default::default()
            },
            HookCommandConfig {
                command: r#"printf '%s' '{"decision":"block","reason":"policy"}'"#.into(),
                ..Default::default()
            },
        ];
        #[cfg(windows)]
        let commands = Vec::new();
        for hook in commands {
            let config = HooksConfig {
                stop: vec![hook],
                ..Default::default()
            };
            let runner = HookRunner::new(config, temp.path().into(), None);
            let mut input = HookInput::new(
                HookEvent::Stop,
                "session",
                temp.path().into(),
                "provider",
                "model",
                "finished",
            );
            input.stop_reason = Some("finished".into());
            let outcome = runner.run(HookEvent::Stop, input).await.unwrap();
            assert!(outcome.blocked);
            assert!(outcome.reason.is_some());
        }
    }

    #[test]
    fn appends_context_without_mutating_plain_prompts() {
        assert_eq!(append_context("task", &[]), "task");
        assert!(append_context("task", &["policy".into()]).contains("<hook_context>"));
    }

    #[test]
    fn truncates_hook_input_at_utf8_boundaries() {
        let value = "你".repeat(MAX_PROMPT_BYTES);
        let truncated = truncate_bytes(&value, MAX_PROMPT_BYTES);
        assert!(truncated.len() <= MAX_PROMPT_BYTES);
        assert!(truncated.ends_with('…'));
    }
}
