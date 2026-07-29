use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::context::{Message, Role};
use crate::protocol::{Action, PlanItem, UserQuestion};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub explanation: Option<String>,
    pub items: Vec<PlanItem>,
}

#[derive(Clone, Default)]
pub struct PlanState {
    inner: Arc<Mutex<PlanSnapshot>>,
}

impl std::fmt::Debug for PlanState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlanState")
            .field("current", &self.current())
            .finish()
    }
}

impl PlanState {
    pub fn update(&self, explanation: Option<String>, items: Vec<PlanItem>) -> PlanSnapshot {
        let snapshot = PlanSnapshot { explanation, items };
        *self.inner.lock().expect("plan state lock poisoned") = snapshot.clone();
        snapshot
    }

    pub fn clear(&self) {
        *self.inner.lock().expect("plan state lock poisoned") = PlanSnapshot::default();
    }

    pub fn current(&self) -> PlanSnapshot {
        self.inner.lock().expect("plan state lock poisoned").clone()
    }

    pub fn restore(messages: &[Message]) -> Self {
        let state = Self::default();
        for message in messages {
            if message.role == Role::User
                && message.content.starts_with("[wecode-context-summary-v1]")
                && let Some(items) = plan_from_summary(&message.content)
            {
                state.update(None, items);
                continue;
            }
            if message.role != Role::Assistant {
                continue;
            }
            let actions = serde_json::from_str::<Action>(&message.content)
                .map(|action| vec![action])
                .or_else(|_| serde_json::from_str::<Vec<Action>>(&message.content));
            let Ok(actions) = actions else {
                continue;
            };
            for action in actions {
                if let Action::UpdatePlan { explanation, plan } = action {
                    state.update(explanation, plan);
                }
            }
        }
        state
    }
}

fn plan_from_summary(summary: &str) -> Option<Vec<PlanItem>> {
    let mut in_plan = false;
    let mut items = Vec::new();
    for line in summary.lines() {
        if line == "Current plan:" {
            in_plan = true;
            continue;
        }
        if in_plan && !line.starts_with("- ") {
            if !line.is_empty() {
                break;
            }
            continue;
        }
        if !in_plan {
            continue;
        }
        let value = line.strip_prefix("- ")?;
        let (status, step) = value.split_once("] ")?;
        let status = match status.strip_prefix('[')? {
            "pending" => crate::protocol::PlanStatus::Pending,
            "in progress" => crate::protocol::PlanStatus::InProgress,
            "completed" => crate::protocol::PlanStatus::Completed,
            _ => return None,
        };
        if !step.is_empty() {
            items.push(PlanItem {
                step: step.to_owned(),
                status,
            });
        }
    }
    (!items.is_empty()).then_some(items)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInputRequest {
    pub id: u64,
    pub questions: Vec<UserQuestion>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserAnswer {
    pub question_id: String,
    pub answer: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserInputResponse {
    Answered(Vec<UserAnswer>),
    Cancelled { reason: String },
}

pub struct UserInputEnvelope {
    pub request: UserInputRequest,
    response: Option<oneshot::Sender<UserInputResponse>>,
}

impl UserInputEnvelope {
    pub fn resolve(mut self, response: UserInputResponse) {
        if let Some(sender) = self.response.take() {
            let _ = sender.send(response);
        }
    }
}

impl Drop for UserInputEnvelope {
    fn drop(&mut self) {
        if let Some(sender) = self.response.take() {
            let _ = sender.send(UserInputResponse::Cancelled {
                reason: "question was abandoned".into(),
            });
        }
    }
}

#[derive(Clone)]
pub struct UserInputClient {
    inner: Arc<UserInputState>,
}

impl std::fmt::Debug for UserInputClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserInputClient")
            .finish_non_exhaustive()
    }
}

struct UserInputState {
    next_id: AtomicU64,
    sender: mpsc::UnboundedSender<UserInputEnvelope>,
}

impl UserInputClient {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<UserInputEnvelope>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(UserInputState {
                    next_id: AtomicU64::new(0),
                    sender,
                }),
            },
            receiver,
        )
    }

    pub fn prepare(&self, questions: Vec<UserQuestion>) -> UserInputRequest {
        UserInputRequest {
            id: self
                .inner
                .next_id
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1),
            questions,
        }
    }

    pub async fn request(&self, request: UserInputRequest) -> UserInputResponse {
        let (sender, receiver) = oneshot::channel();
        if self
            .inner
            .sender
            .send(UserInputEnvelope {
                request,
                response: Some(sender),
            })
            .is_err()
        {
            return UserInputResponse::Cancelled {
                reason: "no interactive input reviewer is available".into(),
            };
        }
        receiver
            .await
            .unwrap_or_else(|_| UserInputResponse::Cancelled {
                reason: "interactive input reviewer disconnected".into(),
            })
    }
}

pub fn resolve_answers(request: &UserInputRequest, input: &str) -> Result<Vec<UserAnswer>, String> {
    let values = if request.questions.len() == 1 {
        vec![input.trim()]
    } else {
        let values = input.split(';').map(str::trim).collect::<Vec<_>>();
        if values.len() != request.questions.len() {
            return Err(format!(
                "answer all {} questions in order, separated with semicolons",
                request.questions.len()
            ));
        }
        values
    };
    if values.iter().any(|value| value.is_empty()) {
        return Err("answers cannot be empty".into());
    }
    Ok(request
        .questions
        .iter()
        .zip(values)
        .map(|(question, value)| UserAnswer {
            question_id: question.id.clone(),
            answer: resolve_option(question, value),
        })
        .collect())
}

fn resolve_option(question: &UserQuestion, value: &str) -> String {
    value
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| question.options.get(index))
        .map(|option| option.label.clone())
        .unwrap_or_else(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PlanStatus, QuestionOption};

    #[test]
    fn restores_the_latest_plan_from_conversation_actions() {
        let messages = vec![
            Message::assistant(
                r#"{"action":"update_plan","plan":[{"step":"inspect","status":"in_progress"}]}"#,
            ),
            Message::user("Plan updated."),
            Message::assistant(
                r#"{"action":"update_plan","explanation":"done","plan":[{"step":"inspect","status":"completed"}]}"#,
            ),
        ];
        assert_eq!(
            PlanState::restore(&messages).current(),
            PlanSnapshot {
                explanation: Some("done".into()),
                items: vec![PlanItem {
                    step: "inspect".into(),
                    status: PlanStatus::Completed,
                }],
            }
        );
    }

    #[test]
    fn restores_a_plan_that_survived_context_compaction() {
        let messages = vec![Message::user(
            "[wecode-context-summary-v1]\n\
             Earlier context was compacted locally.\n\
             Current plan:\n\
             - [completed] inspect\n\
             - [in progress] implement\n\
             - [pending] test\n\
             \nFiles and edits:\n- Inspected `src/lib.rs`.",
        )];
        assert_eq!(
            PlanState::restore(&messages).current().items,
            vec![
                PlanItem {
                    step: "inspect".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    step: "implement".into(),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    step: "test".into(),
                    status: PlanStatus::Pending,
                },
            ]
        );
    }

    #[tokio::test]
    async fn question_channel_maps_numeric_choices_and_freeform_answers() {
        let (client, mut requests) = UserInputClient::channel();
        let request = client.prepare(vec![UserQuestion {
            id: "strategy".into(),
            header: "Strategy".into(),
            question: "Which strategy?".into(),
            options: vec![
                QuestionOption {
                    label: "Safe".into(),
                    description: "Keep compatibility.".into(),
                },
                QuestionOption {
                    label: "Fast".into(),
                    description: "Prefer speed.".into(),
                },
            ],
        }]);
        let task = tokio::spawn({
            let client = client.clone();
            let request = request.clone();
            async move { client.request(request).await }
        });
        let envelope = requests.recv().await.unwrap();
        let answers = resolve_answers(&envelope.request, "2").unwrap();
        envelope.resolve(UserInputResponse::Answered(answers));
        assert_eq!(
            task.await.unwrap(),
            UserInputResponse::Answered(vec![UserAnswer {
                question_id: "strategy".into(),
                answer: "Fast".into(),
            }])
        );
    }
}
