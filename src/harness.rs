use std::collections::BTreeMap;

use crate::context::{Message, ToolCallMessage};
use crate::model::{
    CompletionRequest, ModelResponse, ModelToolCall, ToolProfile, action_batch_text,
};
use crate::prompt_context::PromptContext;
use crate::protocol::{Action, parse_action};
use crate::tool_registry::ToolRegistry;

const TOOL_LOOP_NUDGE_THRESHOLD: usize = 3;
const TOOL_LOOP_STOP_THRESHOLD: usize = 5;

#[derive(Clone, Debug)]
pub struct AgentHarness {
    session_id: String,
    prompt_context: PromptContext,
}

#[derive(Clone, Debug)]
pub struct ToolTurnLedger {
    calls: Vec<ToolCallMessage>,
    next_result: usize,
}

impl ToolTurnLedger {
    pub fn record(
        messages: &mut Vec<Message>,
        assistant_content: impl Into<String>,
        calls: Vec<ToolCallMessage>,
    ) -> Self {
        messages.push(Message::assistant_tool_calls(
            assistant_content,
            calls.clone(),
        ));
        Self {
            calls,
            next_result: 0,
        }
    }

    pub fn record_result(
        &mut self,
        messages: &mut Vec<Message>,
        content: impl Into<String>,
        is_error: bool,
    ) -> bool {
        let Some(call) = self.calls.get(self.next_result) else {
            return false;
        };
        messages.push(Message::tool_result(
            call.id.clone(),
            call.name.clone(),
            content,
            is_error,
        ));
        self.next_result += 1;
        true
    }

    pub fn fail_remaining(&mut self, messages: &mut Vec<Message>, content: &str) {
        while self.record_result(messages, content, true) {}
    }

    pub fn seal(mut self, messages: &mut Vec<Message>) -> usize {
        let mut synthetic_results = 0;
        while self.record_result(
            messages,
            "Tool execution was interrupted before a result was recorded. Re-evaluate the current \
             state before deciding whether to retry.",
            true,
        ) {
            synthetic_results += 1;
        }
        synthetic_results
    }
}

impl AgentHarness {
    pub fn new(session_id: impl Into<String>, prompt_context: PromptContext) -> Self {
        Self {
            session_id: session_id.into(),
            prompt_context,
        }
    }

    pub fn create_turn_state(
        &self,
        messages: &[Message],
        active_tools: Option<&[String]>,
    ) -> TurnState {
        TurnState {
            session_id: self.session_id.clone(),
            prompt_context: self.prompt_context.clone(),
            messages: messages.to_vec(),
            active_tools: active_tools.map(<[String]>::to_vec),
        }
    }

    pub fn prompt_context(&self) -> &PromptContext {
        &self.prompt_context
    }
}

#[derive(Clone, Debug)]
pub struct TurnState {
    pub session_id: String,
    pub prompt_context: PromptContext,
    pub messages: Vec<Message>,
    pub active_tools: Option<Vec<String>>,
}

impl TurnState {
    pub fn completion_request(self) -> CompletionRequest {
        CompletionRequest {
            system: self.prompt_context.render(),
            messages: self.messages,
            session_id: self.session_id,
            enabled_tools: self.active_tools,
        }
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct HarnessMetrics {
    pub model_turns: usize,
    pub compactions: usize,
    pub tool_calls: usize,
    pub tool_counts: BTreeMap<String, usize>,
    pub recoveries: usize,
    pub finish_attempts: usize,
    pub loop_nudges: usize,
    pub history_repairs: usize,
}

#[derive(Clone, Debug)]
pub struct DecodedTurn {
    pub assistant_text: String,
    pub model_text: String,
    pub native_calls: Vec<ModelToolCall>,
    pub disposition: TurnDisposition,
}

#[derive(Clone, Debug)]
pub enum TurnDisposition {
    Execute(Vec<Action>),
    Complete { summary: String },
    Recover { observation: String },
    Stop { reason: String },
}

#[derive(Clone, Debug)]
pub struct TurnDecoder {
    profile: ToolProfile,
    native_tools: bool,
    max_format_errors: usize,
    format_errors: usize,
    last_tool_signature: Option<String>,
    repeated_tool_turns: usize,
    metrics: HarnessMetrics,
}

impl TurnDecoder {
    pub fn new(profile: ToolProfile, native_tools: bool, max_format_errors: usize) -> Self {
        Self {
            profile,
            native_tools,
            max_format_errors,
            format_errors: 0,
            last_tool_signature: None,
            repeated_tool_turns: 0,
            metrics: HarnessMetrics::default(),
        }
    }

    pub fn decode(&mut self, response: &mut ModelResponse) -> DecodedTurn {
        self.metrics.model_turns = self.metrics.model_turns.saturating_add(1);
        let native_calls = response.take_tool_calls();
        let mut actions = response.take_actions();
        if !native_calls.is_empty() {
            actions = native_calls
                .iter()
                .map(|call| call.action.clone())
                .collect();
        }
        let assistant_text = if actions.is_empty() {
            response.text.clone()
        } else {
            action_batch_text(&actions)
        };
        let model_text = response.text.clone();

        if response.stop_reason.is_truncated() {
            self.metrics.recoveries = self.metrics.recoveries.saturating_add(1);
            return DecodedTurn {
                assistant_text,
                model_text,
                native_calls,
                disposition: TurnDisposition::Recover {
                    observation: "MODEL OUTPUT TRUNCATED: The previous response reached its output \
                                  limit. Re-issue any intended tool call with complete arguments, or \
                                  continue the answer from the last complete point."
                        .into(),
                },
            };
        }
        if response.stop_reason.is_blocked() {
            return DecodedTurn {
                assistant_text,
                model_text,
                native_calls,
                disposition: TurnDisposition::Stop {
                    reason: format!("model_{:?}", response.stop_reason).to_ascii_lowercase(),
                },
            };
        }

        if self.native_tools {
            if actions.is_empty() {
                self.format_errors = 0;
                self.reset_tool_loop();
                let summary = response.text.trim().to_owned();
                self.metrics.finish_attempts = self.metrics.finish_attempts.saturating_add(1);
                return DecodedTurn {
                    assistant_text,
                    model_text,
                    native_calls,
                    disposition: TurnDisposition::Complete { summary },
                };
            }
        } else if actions.is_empty() {
            match parse_action(&response.text) {
                Ok(action) => {
                    self.format_errors = 0;
                    actions.push(action);
                }
                Err(_)
                    if !response.text.trim().is_empty()
                        && (self.profile == ToolProfile::Review
                            || (self.profile == ToolProfile::Interactive
                                && !looks_like_action_attempt(&response.text))) =>
                {
                    self.format_errors = 0;
                    self.reset_tool_loop();
                    self.metrics.finish_attempts = self.metrics.finish_attempts.saturating_add(1);
                    return DecodedTurn {
                        assistant_text,
                        model_text,
                        native_calls,
                        disposition: TurnDisposition::Complete {
                            summary: response.text.trim().to_owned(),
                        },
                    };
                }
                Err(error) => {
                    self.format_errors = self.format_errors.saturating_add(1);
                    self.metrics.recoveries = self.metrics.recoveries.saturating_add(1);
                    let disposition = if self.format_errors >= self.max_format_errors {
                        TurnDisposition::Stop {
                            reason: "format_error_limit".into(),
                        }
                    } else {
                        TurnDisposition::Recover {
                            observation: format!(
                                "FORMAT ERROR: {error}\nReturn exactly one valid JSON action matching the system schema."
                            ),
                        }
                    };
                    return DecodedTurn {
                        assistant_text,
                        model_text,
                        native_calls,
                        disposition,
                    };
                }
            }
        }

        if let Err(error) = ToolRegistry::validate_batch(&actions) {
            self.metrics.recoveries = self.metrics.recoveries.saturating_add(1);
            return DecodedTurn {
                assistant_text,
                model_text,
                native_calls,
                disposition: TurnDisposition::Recover {
                    observation: format!("TOOL BATCH ERROR: {error}."),
                },
            };
        }

        if let [Action::Finish { summary }] = actions.as_slice() {
            self.reset_tool_loop();
            self.metrics.finish_attempts = self.metrics.finish_attempts.saturating_add(1);
            return DecodedTurn {
                assistant_text,
                model_text,
                native_calls,
                disposition: TurnDisposition::Complete {
                    summary: summary.clone(),
                },
            };
        }

        if let Some(disposition) = self.observe_tool_loop(&actions) {
            return DecodedTurn {
                assistant_text,
                model_text,
                native_calls,
                disposition,
            };
        }

        self.metrics.tool_calls = self.metrics.tool_calls.saturating_add(
            actions
                .iter()
                .filter(|action| !matches!(action, Action::Finish { .. }))
                .count(),
        );
        for action in actions
            .iter()
            .filter(|action| !matches!(action, Action::Finish { .. }))
        {
            let count = self
                .metrics
                .tool_counts
                .entry(action.kind().to_owned())
                .or_default();
            *count = count.saturating_add(1);
        }
        DecodedTurn {
            assistant_text,
            model_text,
            native_calls,
            disposition: TurnDisposition::Execute(actions),
        }
    }

    pub fn metrics(&self) -> &HarnessMetrics {
        &self.metrics
    }

    pub fn uses_native_tools(&self) -> bool {
        self.profile == ToolProfile::Interactive && self.native_tools
    }

    pub fn record_history_repairs(&mut self, count: usize) {
        self.metrics.history_repairs = self.metrics.history_repairs.saturating_add(count);
    }

    pub fn record_compaction(&mut self) {
        self.metrics.model_turns = self.metrics.model_turns.saturating_add(1);
        self.metrics.compactions = self.metrics.compactions.saturating_add(1);
    }

    fn observe_tool_loop(&mut self, actions: &[Action]) -> Option<TurnDisposition> {
        let signature = serde_json::to_string(actions).expect("validated actions are serializable");
        if self.last_tool_signature.as_deref() == Some(signature.as_str()) {
            self.repeated_tool_turns = self.repeated_tool_turns.saturating_add(1);
        } else {
            self.last_tool_signature = Some(signature);
            self.repeated_tool_turns = 1;
        }
        if self.repeated_tool_turns >= TOOL_LOOP_STOP_THRESHOLD {
            self.metrics.recoveries = self.metrics.recoveries.saturating_add(1);
            return Some(TurnDisposition::Stop {
                reason: "repeated_tool_call_limit".into(),
            });
        }
        if self.repeated_tool_turns == TOOL_LOOP_NUDGE_THRESHOLD {
            self.metrics.recoveries = self.metrics.recoveries.saturating_add(1);
            self.metrics.loop_nudges = self.metrics.loop_nudges.saturating_add(1);
            return Some(TurnDisposition::Recover {
                observation: "LOOP GUARD: The same tool call was requested repeatedly with no \
                              change in arguments. Use the existing results, change the approach, \
                              or explain the blocker instead of repeating it."
                    .into(),
            });
        }
        None
    }

    fn reset_tool_loop(&mut self) {
        self.last_tool_signature = None;
        self.repeated_tool_turns = 0;
    }
}

fn looks_like_action_attempt(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || trimmed.starts_with("```")
        || text.contains("\"action\"")
}

pub(crate) fn tool_observation_is_error(observation: &str) -> bool {
    observation.starts_with("TOOL ERROR:")
        || observation.contains("\nTOOL ERROR:")
        || observation.starts_with("PERMISSION DENIED:")
        || observation.contains(" UNAVAILABLE:")
        || observation.starts_with("USER INPUT CANCELLED:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;
    use crate::model::ToolProfile;
    use crate::model::{ModelResponse, StopReason, Usage};
    use crate::prompt_context::PromptContextOptions;
    use crate::protocol::Action;

    #[test]
    fn turn_snapshots_keep_the_prompt_prefix_stable() {
        let temp = tempfile::tempdir().unwrap();
        let model = ModelConfig::default();
        let prompt = PromptContext::build(PromptContextOptions {
            profile: ToolProfile::Interactive,
            model: &model,
            workspace: temp.path(),
            instructions: None,
            skills_prompt: None,
            additional_prompt: None,
        })
        .unwrap();
        let harness = AgentHarness::new("session-1", prompt);
        let active_tools = vec!["read_file".into()];
        let first = harness
            .create_turn_state(&[Message::user("first")], Some(&active_tools))
            .completion_request();
        let second = harness
            .create_turn_state(
                &[Message::user("first"), Message::assistant("second")],
                Some(&active_tools),
            )
            .completion_request();

        assert_eq!(first.system, second.system);
        assert_eq!(first.enabled_tools, second.enabled_tools);
        assert_ne!(first.messages, second.messages);
    }

    #[test]
    fn decoder_accepts_plain_interactive_final_text() {
        let mut decoder = TurnDecoder::new(ToolProfile::Interactive, true, 3);
        let mut response = ModelResponse {
            text: "The action decoder is implemented and the repository is clean.".into(),
            tool_calls: Vec::new(),
            action: None,
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        };
        let decoded = decoder.decode(&mut response);
        assert!(matches!(
            decoded.disposition,
            TurnDisposition::Complete { ref summary }
                if summary.contains("action decoder")
        ));
        assert_eq!(decoder.metrics().finish_attempts, 1);
    }

    #[test]
    fn native_mode_never_executes_tool_json_from_plain_text() {
        let mut decoder = TurnDecoder::new(ToolProfile::Interactive, true, 3);
        let mut response = ModelResponse {
            text: r#"{"action":"shell","command":"rm -rf .","description":"bad"}"#.into(),
            tool_calls: Vec::new(),
            action: None,
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        };

        let decoded = decoder.decode(&mut response);
        assert!(matches!(
            decoded.disposition,
            TurnDisposition::Complete { summary } if summary.contains("\"action\":\"shell\"")
        ));
        assert_eq!(decoder.metrics().tool_calls, 0);
    }

    #[test]
    fn repeated_identical_tool_calls_are_nudged_then_stopped() {
        let mut decoder = TurnDecoder::new(ToolProfile::Interactive, true, 3);
        for turn in 1..=TOOL_LOOP_STOP_THRESHOLD {
            let action = Action::ReadFile {
                path: "README.md".into(),
                offset: None,
                limit: None,
            };
            let mut response = ModelResponse {
                text: "tool".into(),
                tool_calls: Vec::new(),
                action: Some(action),
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            };
            let disposition = decoder.decode(&mut response).disposition;
            match turn {
                TOOL_LOOP_NUDGE_THRESHOLD => {
                    assert!(matches!(disposition, TurnDisposition::Recover { .. }));
                }
                TOOL_LOOP_STOP_THRESHOLD => {
                    assert!(matches!(
                        disposition,
                        TurnDisposition::Stop { ref reason }
                            if reason == "repeated_tool_call_limit"
                    ));
                }
                _ => assert!(matches!(disposition, TurnDisposition::Execute(_))),
            }
        }
        assert_eq!(decoder.metrics().loop_nudges, 1);
    }

    #[test]
    fn truncated_model_output_is_recovered_without_execution() {
        let mut decoder = TurnDecoder::new(ToolProfile::Interactive, true, 3);
        let mut response = ModelResponse {
            text: "partial".into(),
            tool_calls: Vec::new(),
            action: Some(Action::Shell {
                command: "cargo test".into(),
                description: "verify".into(),
            }),
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: StopReason::MaxTokens,
        };

        assert!(matches!(
            decoder.decode(&mut response).disposition,
            TurnDisposition::Recover { .. }
        ));
        assert_eq!(decoder.metrics().tool_calls, 0);
    }

    #[test]
    fn tool_turn_ledger_binds_results_by_provider_call_order() {
        let mut messages = vec![Message::user("inspect")];
        let mut ledger = ToolTurnLedger::record(
            &mut messages,
            "checking",
            vec![
                ToolCallMessage {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "README.md"}),
                },
                ToolCallMessage {
                    id: "call-2".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern": "TODO"}),
                },
            ],
        );
        assert!(ledger.record_result(&mut messages, "contents", false));
        assert!(ledger.record_result(&mut messages, "TOOL ERROR: no matches", true));

        assert_eq!(ledger.seal(&mut messages), 0);
        assert_eq!(
            messages[2]
                .tool_result
                .as_ref()
                .map(|result| result.call_id.as_str()),
            Some("call-1")
        );
        assert_eq!(
            messages[3]
                .tool_result
                .as_ref()
                .map(|result| (result.call_id.as_str(), result.is_error)),
            Some(("call-2", true))
        );
    }

    #[test]
    fn decoder_stops_after_bounded_format_recovery() {
        let mut decoder = TurnDecoder::new(ToolProfile::Coding, false, 2);
        for expected_recover in [true, false] {
            let mut response = ModelResponse {
                text: "not an action".into(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            };
            let decoded = decoder.decode(&mut response);
            assert_eq!(
                matches!(decoded.disposition, TurnDisposition::Recover { .. }),
                expected_recover
            );
        }
        assert_eq!(decoder.metrics().recoveries, 2);
    }
}
