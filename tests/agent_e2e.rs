use std::collections::{BTreeMap, VecDeque};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Notify;
use wecode::agent::{Agent, Conversation, RunOptions};
use wecode::approval::{ApprovalClient, ApprovalDecision};
use wecode::attachments::{prepare_message, resolve_file_mentions};
use wecode::cache::ResponseCache;
#[cfg(unix)]
use wecode::config::McpServerConfig;
use wecode::config::{
    ApprovalPolicy, CacheConfig, Config, LspConfig, LspServerConfig, SkillsConfig,
};
use wecode::context::ImageAttachment;
use wecode::control::CancellationToken;
use wecode::events::{Event, EventSink};
use wecode::input_queue::InputQueue;
use wecode::interaction::{PlanState, UserInputClient, UserInputResponse, resolve_answers};
use wecode::lsp::LspManager;
use wecode::mcp::McpManager;
use wecode::model::{
    CompletionRequest, Model, ModelResponse, ModelStream, ModelToolCall, ToolProfile, Usage,
};
use wecode::protocol::{Action, LspOperation, PlanItem, PlanStatus, QuestionOption, UserQuestion};
use wecode::review::{ReviewRequest, parse_review, review_prompt};
use wecode::skills::SkillCatalog;
use wecode::subagent::{SubagentManager, SubagentStatus};

struct FakeModel {
    responses: Mutex<VecDeque<String>>,
}

#[async_trait]
impl Model for FakeModel {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        let text = self
            .responses
            .lock()
            .expect("fake model lock")
            .pop_front()
            .expect("fake model response");
        Ok(ModelResponse {
            text,
            tool_calls: Vec::new(),
            action: None,
            additional_actions: Vec::new(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

struct NativeFakeModel {
    responses: Mutex<VecDeque<Action>>,
}

struct BatchFakeModel {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    responses: Mutex<VecDeque<Vec<Action>>>,
}

struct CapturingModel {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    responses: Mutex<VecDeque<Action>>,
}

struct NativeCallCapturingModel {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    final_text: String,
}

struct ScriptedResponseModel {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    responses: Mutex<VecDeque<ModelResponse>>,
}

#[async_trait]
impl Model for NativeCallCapturingModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.requests.lock().expect("request capture").push(request);
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        Ok(if first {
            ModelResponse {
                text: String::new(),
                tool_calls: vec![ModelToolCall {
                    id: "call_read_1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "tracked.txt"}),
                    action: Action::ReadFile {
                        path: "tracked.txt".into(),
                        offset: None,
                        limit: None,
                    },
                }],
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            }
        } else {
            ModelResponse {
                text: self.final_text.clone(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            }
        })
    }
}

#[async_trait]
impl Model for ScriptedResponseModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.requests.lock().expect("request capture").push(request);
        Ok(self
            .responses
            .lock()
            .expect("fake model lock")
            .pop_front()
            .expect("fake model response"))
    }
}

#[async_trait]
impl Model for CapturingModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.requests.lock().expect("request capture").push(request);
        let action = self
            .responses
            .lock()
            .expect("fake model lock")
            .pop_front()
            .expect("fake model response");
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action: Some(action),
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

#[async_trait]
impl Model for NativeFakeModel {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        let action = self
            .responses
            .lock()
            .expect("fake model lock")
            .pop_front()
            .expect("fake model response");
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action: Some(action),
            additional_actions: Vec::new(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

#[async_trait]
impl Model for BatchFakeModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.requests.lock().expect("request capture").push(request);
        let mut actions = self
            .responses
            .lock()
            .expect("fake model lock")
            .pop_front()
            .expect("fake model response");
        let action = (!actions.is_empty()).then(|| actions.remove(0));
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action,
            additional_actions: actions,
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &Event) -> Result<()> {
        Ok(())
    }
}

struct BlockingModel {
    started: Arc<Notify>,
}

struct SteeringGateModel {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct GatedChildModel {
    release: Arc<Notify>,
}

struct BackgroundParentModel {
    calls: AtomicUsize,
    release_child: Arc<Notify>,
    child_completed: Arc<Notify>,
}

#[async_trait]
impl Model for GatedChildModel {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.release.notified().await;
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action: Some(Action::Finish {
                summary: "background delegated result".into(),
            }),
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

#[async_trait]
impl Model for BackgroundParentModel {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let action = match call {
            0 => Action::SpawnAgent {
                description: "background inspection".into(),
                prompt: "Inspect the fixture.".into(),
                agent_type: "explore".into(),
                background: true,
                model: None,
            },
            1 => {
                self.release_child.notify_one();
                self.child_completed.notified().await;
                Action::Finish {
                    summary: "premature parent finish".into(),
                }
            }
            _ => Action::Finish {
                summary: "parent incorporated background result".into(),
            },
        };
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action: Some(action),
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

#[async_trait]
impl Model for SteeringGateModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(ModelResponse {
            text: String::new(),
            tool_calls: Vec::new(),
            action: Some(Action::Finish {
                summary: if call == 0 {
                    "premature finish".into()
                } else {
                    "steering applied".into()
                },
            }),
            additional_actions: Vec::new(),
            usage: Usage::default(),
            cache_hit: false,
            stop_reason: Default::default(),
        })
    }
}

#[async_trait]
impl Model for BlockingModel {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        self.started.notify_one();
        std::future::pending().await
    }
}

#[derive(Clone, Default)]
struct CapturingSink {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventSink for CapturingSink {
    fn emit(&self, event: &Event) -> Result<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[tokio::test]
async fn completes_task_verifies_and_collects_untracked_patch() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());

    let model = NativeFakeModel {
        responses: Mutex::new(VecDeque::from([
            Action::Shell {
                command: "printf hello > result.txt".into(),
                description: "create result".into(),
            },
            Action::Shell {
                command: "test \"$(cat result.txt)\" = hello".into(),
                description: "check result".into(),
            },
            Action::Finish {
                summary: "created and checked result.txt".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache = CacheConfig {
        directory: temp.path().join("cache"),
        ..Default::default()
    };
    // Ensure constructing the cache remains valid in the same isolated fixture.
    ResponseCache::new(config.cache.clone()).unwrap();

    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );
    let result = agent
        .run(
            "create result.txt",
            RunOptions {
                verify: Some("test \"$(cat result.txt)\" = hello".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 3);
    assert!(result.patch.contains("result.txt"));
    assert!(result.patch.contains("+hello"));
}

#[tokio::test]
async fn native_tool_action_applies_codex_patch_and_finishes() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let model = NativeFakeModel {
        responses: Mutex::new(VecDeque::from([
            Action::Patch {
                patch:
                    "*** Begin Patch\n*** Add File: native.txt\n+native tools work\n*** End Patch"
                        .into(),
                description: "add native tool fixture".into(),
            },
            Action::Finish {
                summary: "created native.txt with a native apply_patch action".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run(
            "create native.txt",
            RunOptions {
                verify: Some("test \"$(cat native.txt)\" = \"native tools work\"".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 2);
    assert!(result.patch.contains("native.txt"));
    assert!(result.patch.contains("+native tools work"));
}

#[tokio::test]
async fn failed_final_verification_reopens_a_native_tool_loop() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedResponseModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            ModelResponse {
                text: "The requested file is ready.".into(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            },
            ModelResponse {
                text: String::new(),
                tool_calls: vec![ModelToolCall {
                    id: "call_create_result".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({
                        "command": "printf hello > result.txt",
                        "description": "create the requested file"
                    }),
                    action: Action::Shell {
                        command: "printf hello > result.txt".into(),
                        description: "create the requested file".into(),
                    },
                }],
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            },
            ModelResponse {
                text: "Created result.txt and verified its contents.".into(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run(
            "create result.txt",
            RunOptions {
                verify: Some("test \"$(cat result.txt)\" = hello".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 3);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("result.txt")).unwrap(),
        "hello"
    );
    let requests = requests.lock().unwrap();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("FINAL VERIFICATION FAILED"))
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(
                |message| message.tool_result.as_ref().is_some_and(|result| {
                    result.call_id == "call_create_result" && result.name == "shell"
                })
            )
    );
}

#[tokio::test]
async fn interactive_plan_updates_state_and_model_context() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let plan = PlanState::default();
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::SearchTools {
                query: "plan multi-step work".into(),
                limit: None,
            },
            Action::UpdatePlan {
                explanation: Some("Track the work visibly.".into()),
                plan: vec![
                    PlanItem {
                        step: "Inspect the fixture".into(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Report the result".into(),
                        status: PlanStatus::InProgress,
                    },
                ],
            },
            Action::Finish {
                summary: "premature".into(),
            },
            Action::UpdatePlan {
                explanation: Some("All planned work is complete.".into()),
                plan: vec![
                    PlanItem {
                        step: "Inspect the fixture".into(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Report the result".into(),
                        status: PlanStatus::Completed,
                    },
                ],
            },
            Action::Finish {
                summary: "plan was updated".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run(
            "show a plan",
            RunOptions {
                plan: Some(plan.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    assert!(
        plan.current()
            .items
            .iter()
            .all(|item| item.status == PlanStatus::Completed)
    );
    let requests = requests.lock().unwrap();
    assert!(
        !requests[0]
            .enabled_tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|tool| tool == "update_plan")
    );
    assert!(
        requests[1]
            .enabled_tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|tool| tool == "update_plan")
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content.contains("PLAN UPDATED:"))
    );
    assert!(
        requests[3]
            .messages
            .iter()
            .any(|message| message.content.contains("PLAN INCOMPLETE:"))
    );
    assert_eq!(requests.len(), 5);
}

#[tokio::test]
async fn interactive_prompt_does_not_turn_greetings_into_repository_scans() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::write(
        temp.path().join("AGENTS.md"),
        "Use repository conventions.\n",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join(".wecode")).unwrap();
    std::fs::write(
        temp.path().join(".wecode/MEMORY.md"),
        "The user prefers focused validation.\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = NativeCallCapturingModel {
        calls: AtomicUsize::new(1),
        requests: requests.clone(),
        final_text: "你好，有什么可以帮你？".into(),
    };
    let mut config = Config::default();
    config.model.model = "gpt-5.4-mini".into();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent.run("你好", RunOptions::default()).await.unwrap();

    assert_eq!(result.steps, 1);
    assert_eq!(result.summary, "你好，有什么可以帮你？");
    let requests = requests.lock().unwrap();
    assert!(requests[0].system.contains("boundary of every request"));
    assert!(
        requests[0]
            .system
            .contains("Treat short requests as sufficient direction")
    );
    assert!(
        requests[0]
            .system
            .contains("When uncertain, investigate to find the truth")
    );
    assert!(
        requests[0]
            .system
            .contains("merely because a working directory exists")
    );
    assert!(
        requests[0]
            .system
            .contains("id=\"base_instructions\" stability=\"stable\"")
    );
    assert!(
        requests[0]
            .system
            .contains("id=\"world_state\" stability=\"volatile\"")
    );
    assert!(requests[0].system.contains("Provider: openai"));
    assert!(requests[0].system.contains("Model: gpt-5.4-mini"));
    assert!(requests[0].system.contains("Use repository conventions."));
    assert!(
        requests[0]
            .system
            .contains("The user prefers focused validation.")
    );
    assert_eq!(
        requests[0].messages[0].content,
        "<user_request>\n你好\n</user_request>\n"
    );
    assert_eq!(
        requests[0]
            .enabled_tools
            .as_ref()
            .unwrap()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        wecode::tool_registry::AUTONOMOUS_CORE_TOOLS
    );
    assert!(
        !requests[0]
            .enabled_tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|tool| tool == "finish")
    );
}

#[tokio::test]
async fn interactive_native_tool_result_keeps_the_provider_call_id() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = NativeCallCapturingModel {
        calls: AtomicUsize::new(0),
        requests: requests.clone(),
        final_text: "Inspected tracked.txt successfully.".into(),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run("inspect tracked.txt", RunOptions::default())
        .await
        .unwrap();
    assert!(result.success);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let assistant_call = requests[1]
        .messages
        .iter()
        .find_map(|message| message.tool_calls.first())
        .expect("assistant tool call");
    let tool_result = requests[1]
        .messages
        .iter()
        .find_map(|message| message.tool_result.as_ref())
        .expect("native tool result");
    assert_eq!(assistant_call.id, "call_read_1");
    assert_eq!(tool_result.call_id, assistant_call.id);
    assert_eq!(tool_result.name, "read_file");
}

#[tokio::test]
async fn native_mixed_parallel_batch_preserves_order_without_protocol_repair() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::default();
    let events = sink.events.clone();
    let model = ScriptedResponseModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            ModelResponse {
                text: String::new(),
                tool_calls: vec![
                    ModelToolCall {
                        id: "call_list_1".into(),
                        name: "list_files".into(),
                        arguments: serde_json::json!({"path": ".", "depth": 1}),
                        action: Action::ListFiles {
                            path: ".".into(),
                            depth: Some(1),
                            limit: None,
                        },
                    },
                    ModelToolCall {
                        id: "call_shell_2".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({
                            "command": "printf runtime-ok",
                            "description": "inspect runtime"
                        }),
                        action: Action::Shell {
                            command: "printf runtime-ok".into(),
                            description: "inspect runtime".into(),
                        },
                    },
                ],
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            },
            ModelResponse {
                text: "Inspection complete.".into(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
                stop_reason: Default::default(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(sink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run("inspect independent local facts", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 2);
    assert_eq!(result.harness.history_repairs, 0);
    let requests = requests.lock().unwrap();
    let results = requests[1]
        .messages
        .iter()
        .filter_map(|message| message.tool_result.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        ["call_list_1", "call_shell_2"]
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message.tool_result.is_some())
            .any(|message| message.content.contains("runtime-ok"))
    );
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::ToolProtocolRepaired { .. }))
    );
}

#[tokio::test]
async fn interactive_question_waits_for_and_returns_the_user_choice() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::RequestUserInput {
                questions: vec![UserQuestion {
                    id: "approach".into(),
                    header: "Approach".into(),
                    question: "Which approach should I use?".into(),
                    options: vec![
                        QuestionOption {
                            label: "Compatible".into(),
                            description: "Preserve the public API.".into(),
                        },
                        QuestionOption {
                            label: "Rewrite".into(),
                            description: "Allow a breaking redesign.".into(),
                        },
                    ],
                }],
            },
            Action::Finish {
                summary: "used the compatible approach".into(),
            },
        ])),
    };
    let (user_input, mut input_requests) = UserInputClient::channel();
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );
    let run = agent.run(
        "ask before choosing",
        RunOptions {
            user_input: Some(user_input),
            ..Default::default()
        },
    );
    tokio::pin!(run);
    let envelope = tokio::select! {
        request = input_requests.recv() => request.expect("user input request"),
        result = &mut run => panic!("run ended before user input: {result:?}"),
    };
    let answers = resolve_answers(&envelope.request, "1").unwrap();
    envelope.resolve(UserInputResponse::Answered(answers));
    let result = run.await.unwrap();

    assert!(result.success);
    let requests = requests.lock().unwrap();
    assert!(
        requests[0]
            .enabled_tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|tool| tool == "request_user_input")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("approach: Compatible"))
    );
}

#[tokio::test]
async fn first_class_file_tools_feed_bounded_observations_back_to_the_model() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn benchmark_ready() -> bool {\n    true\n}\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::default();
    let events = sink.events.clone();
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::ListFiles {
                path: ".".into(),
                depth: Some(2),
                limit: Some(100),
            },
            Action::Grep {
                pattern: "benchmark_ready".into(),
                path: ".".into(),
                glob: Some("**/*.rs".into()),
                literal: true,
                ignore_case: false,
                context: Some(1),
                limit: Some(20),
            },
            Action::ReadFile {
                path: "src/lib.rs".into(),
                offset: Some(1),
                limit: Some(20),
            },
            Action::Finish {
                summary: "inspected the project with first-class file tools".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(sink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run("inspect the benchmark helper", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 4);
    let requests = requests.lock().unwrap();
    assert!(requests[1].messages.iter().any(|message| {
        message.content.contains("directory: .") && message.content.contains("src/lib.rs")
    }));
    assert!(requests[2].messages.iter().any(|message| {
        message
            .content
            .contains("src/lib.rs:1:pub fn benchmark_ready()")
    }));
    assert!(requests[3].messages.iter().any(|message| {
        message.content.contains("lines: 1-3 of 3") && message.content.contains("     2\t    true")
    }));
    let events = events.lock().unwrap();
    for expected in ["list_files", "grep", "read_file"] {
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Action { kind, .. } if kind == expected))
        );
    }
}

#[tokio::test]
async fn independent_read_tools_share_one_parallel_model_step() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/alpha.rs"),
        "pub const ALPHA: &str = \"parallel-alpha\";\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/beta.rs"),
        "pub const BETA: &str = \"parallel-beta\";\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::default();
    let events = sink.events.clone();
    let model = BatchFakeModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            vec![
                Action::ReadFile {
                    path: "src/alpha.rs".into(),
                    offset: None,
                    limit: None,
                },
                Action::Grep {
                    pattern: "parallel-beta".into(),
                    path: "src".into(),
                    glob: Some("**/*.rs".into()),
                    literal: true,
                    ignore_case: false,
                    context: None,
                    limit: None,
                },
            ],
            vec![Action::Finish {
                summary: "parallel inspection complete".into(),
            }],
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(sink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run("inspect both fixtures", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 2);
    let requests = requests.lock().unwrap();
    let observation = &requests[1].messages.last().unwrap().content;
    assert!(observation.contains("TOOL RESULT 1/2 [read_file"));
    assert!(observation.contains("parallel-alpha"));
    assert!(observation.contains("TOOL RESULT 2/2 [grep"));
    assert!(observation.contains("parallel-beta"));
    let actions = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Event::Action { step, kind, .. } if *step == 1 => Some(kind.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(actions, ["read_file", "grep"]);
}

#[tokio::test]
async fn independent_shell_tools_share_one_parallel_model_step() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = BatchFakeModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            vec![
                Action::Shell {
                    command: "printf shell-alpha".into(),
                    description: "first probe".into(),
                },
                Action::Shell {
                    command: "printf shell-beta".into(),
                    description: "second probe".into(),
                },
            ],
            vec![Action::Finish {
                summary: "parallel shell inspection complete".into(),
            }],
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run("run two independent probes", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 2);
    assert_eq!(result.harness.tool_counts.get("shell"), Some(&2));
    let requests = requests.lock().unwrap();
    let observation = &requests[1].messages.last().unwrap().content;
    assert!(observation.contains("TOOL RESULT 1/2 [shell"));
    assert!(observation.contains("shell-alpha"));
    assert!(observation.contains("TOOL RESULT 2/2 [shell"));
    assert!(observation.contains("shell-beta"));
}

#[tokio::test]
async fn foreground_subagent_result_reaches_the_parent_model() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let parent_model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::SearchTools {
                query: "delegate subagent inspection".into(),
                limit: None,
            },
            Action::SpawnAgent {
                description: "inspect fixture".into(),
                prompt: "Report what the fixture contains.".into(),
                agent_type: "explore".into(),
                background: false,
                model: None,
            },
            Action::Finish {
                summary: "parent used delegated result".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let manager = SubagentManager::new_with_model_factory(
        config.clone(),
        temp.path().to_path_buf(),
        |_model, profile| {
            assert_eq!(profile, ToolProfile::ReadOnlySubagent);
            Ok(Box::new(NativeFakeModel {
                responses: Mutex::new(VecDeque::from([Action::Finish {
                    summary: "delegated fixture result".into(),
                }])),
            }))
        },
    )
    .unwrap();
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(parent_model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run(
            "delegate the fixture inspection",
            RunOptions {
                subagents: Some(manager.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            !requests[0]
                .enabled_tools
                .as_ref()
                .unwrap()
                .iter()
                .any(|tool| tool == "spawn_agent")
        );
        assert!(
            requests[1]
                .enabled_tools
                .as_ref()
                .unwrap()
                .iter()
                .any(|tool| tool == "spawn_agent")
        );
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| message.content.contains("delegated fixture result"))
        );
    }
    assert_eq!(
        manager.summaries().await[0].status,
        SubagentStatus::Completed
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn background_subagent_notification_reopens_parent_finish() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let release_child = Arc::new(Notify::new());
    let child_completed = Arc::new(Notify::new());
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let manager =
        SubagentManager::new_with_model_factory(config.clone(), temp.path().to_path_buf(), {
            let release_child = release_child.clone();
            move |_model, profile| {
                assert_eq!(profile, ToolProfile::ReadOnlySubagent);
                Ok(Box::new(GatedChildModel {
                    release: release_child.clone(),
                }))
            }
        })
        .unwrap();
    manager.set_event_handler({
        let child_completed = child_completed.clone();
        move |event| {
            if event.status == SubagentStatus::Completed {
                child_completed.notify_one();
            }
        }
    });
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(BackgroundParentModel {
            calls: AtomicUsize::new(0),
            release_child,
            child_completed,
        }),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run(
            "delegate in the background",
            RunOptions {
                subagents: Some(manager.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 3);
    assert_eq!(result.summary, "parent incorporated background result");
    assert!(manager.take_notifications().is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn interactive_conversation_preserves_follow_up_context() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::Finish {
                summary: "first task complete".into(),
            },
            Action::Finish {
                summary: "follow-up complete".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );
    let mut conversation = Conversation::default();

    agent
        .run_in_conversation(
            "inspect the project",
            RunOptions {
                session_id: Some("stable-chat-session".into()),
                images: vec![ImageAttachment {
                    media_type: "image/png".into(),
                    data: "Zmlyc3Q=".into(),
                    name: "first.png".into(),
                }],
                ..Default::default()
            },
            &mut conversation,
        )
        .await
        .unwrap();
    agent
        .run_in_conversation(
            "now explain the result",
            RunOptions {
                session_id: Some("stable-chat-session".into()),
                images: vec![ImageAttachment {
                    media_type: "image/jpeg".into(),
                    data: "c2Vjb25k".into(),
                    name: "second.jpg".into(),
                }],
                ..Default::default()
            },
            &mut conversation,
        )
        .await
        .unwrap();

    assert_eq!(conversation.message_count(), 4);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].session_id, "stable-chat-session");
    assert_eq!(requests[1].session_id, "stable-chat-session");
    assert_eq!(requests[0].messages[0].images[0].name, "first.png");
    assert!(requests[1].messages.iter().any(|message| {
        message
            .images
            .iter()
            .any(|image| image.name == "second.jpg")
    }));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("first task complete"))
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("now explain the result"))
    );
}

#[tokio::test]
async fn file_mentions_reach_the_initial_model_context_as_bounded_attachments() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::write(
        temp.path().join("parser note.txt"),
        "The parser fails on trailing commas.\n",
    )
    .unwrap();
    let (task, attachments) =
        resolve_file_mentions("Inspect @\"parser note.txt\"", temp.path()).unwrap();
    let (task, images) = prepare_message(&task, attachments).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([Action::Finish {
            summary: "attachment inspected".into(),
        }])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run(
            &task,
            RunOptions {
                images,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].messages[0]
            .content
            .contains("The parser fails on trailing commas.")
    );
    assert!(requests[0].messages[0].content.contains("<attached_file"));
}

#[tokio::test]
async fn project_instructions_are_injected_into_the_initial_context() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::write(
        temp.path().join("AGENTS.md"),
        "Run cargo fmt before finishing.",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([Action::Finish {
            summary: "instructions loaded".into(),
        }])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );

    agent
        .run("inspect the project", RunOptions::default())
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].messages[0]
            .content
            .contains("Run cargo fmt before finishing.")
    );
    assert!(
        requests[0].messages[0]
            .content
            .contains("<project_instructions")
    );
}

#[tokio::test]
async fn cancellation_stops_a_model_call_and_preserves_the_user_task() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let started = Arc::new(Notify::new());
    let sink = CapturingSink::default();
    let events = sink.events.clone();
    let cancellation = CancellationToken::new();
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(BlockingModel {
            started: started.clone(),
        }),
        Box::new(sink),
        temp.path().canonicalize().unwrap(),
    );
    let cancel_task = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            started.notified().await;
            cancellation.cancel();
        }
    });
    let mut conversation = Conversation::default();

    let result = agent
        .run_in_conversation(
            "remember this cancelled task",
            RunOptions {
                cancellation: Some(cancellation),
                ..Default::default()
            },
            &mut conversation,
        )
        .await
        .unwrap();
    cancel_task.await.unwrap();

    assert!(!result.success);
    assert_eq!(result.reason, "cancelled");
    assert_eq!(conversation.message_count(), 1);
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::RunCancelled { step: 1 }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::RunCompleted {
            success: false,
            reason,
            ..
        } if reason == "cancelled"
    )));
}

#[tokio::test]
async fn steering_arriving_during_sampling_reopens_the_active_run() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::default();
    let events = sink.events.clone();
    let queue = InputQueue::new();
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(SteeringGateModel {
            calls: AtomicUsize::new(0),
            requests: requests.clone(),
            started: started.clone(),
            release: release.clone(),
        }),
        Box::new(sink),
        temp.path().canonicalize().unwrap(),
    );
    let enqueue = tokio::spawn({
        let queue = queue.clone();
        async move {
            started.notified().await;
            queue.steer_with_images(
                "also explain the result",
                vec![ImageAttachment {
                    media_type: "image/webp".into(),
                    data: "c3RlZXI=".into(),
                    name: "steering.webp".into(),
                }],
            );
            release.notify_one();
        }
    });
    let mut conversation = Conversation::default();

    let result = agent
        .run_in_conversation(
            "inspect the repository",
            RunOptions {
                input_queue: Some(queue),
                ..Default::default()
            },
            &mut conversation,
        )
        .await
        .unwrap();
    enqueue.await.unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 2);
    assert_eq!(result.summary, "steering applied");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("also explain the result"))
    );
    assert!(requests[1].messages.iter().any(|message| {
        message
            .images
            .iter()
            .any(|image| image.name == "steering.webp")
    }));
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::SteeringDelivered { step: 1, count: 1 }))
    );
}

#[tokio::test]
async fn denied_elevated_command_is_returned_to_the_model() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::Shell {
                command: "curl https://example.com".into(),
                description: "fetch remote data".into(),
            },
            Action::Finish {
                summary: "used a safer approach".into(),
            },
        ])),
    };
    let (approval, mut approvals) = ApprovalClient::channel();
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );
    let run = agent.run(
        "inspect without network access",
        RunOptions {
            approval: Some(approval),
            ..Default::default()
        },
    );
    tokio::pin!(run);
    let request = tokio::select! {
        request = approvals.recv() => request.expect("approval request"),
        result = &mut run => panic!("run ended before approval: {result:?}"),
    };
    assert_eq!(request.request.detail, "curl https://example.com");
    request.resolve(ApprovalDecision::Deny {
        reason: "network access is not allowed".into(),
    });
    let result = run.await.unwrap();

    assert!(result.success);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message
            .content
            .contains("PERMISSION DENIED: network access is not allowed")
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn interactive_agent_executes_a_discovered_read_only_mcp_tool() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let script = temp.path().join("mcp-fixture.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"inspect","description":"Inspect fixture","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}}'
      ;;
    *'"method":"tools/call"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"mcp-observation"}]}}'
      ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::SearchTools {
                query: "fixture integration inspect".into(),
                limit: None,
            },
            Action::McpCall {
                server: "fixture".into(),
                tool: "inspect".into(),
                arguments: serde_json::json!({}),
            },
            Action::Finish {
                summary: "MCP result used".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    config.mcp.servers.insert(
        "fixture".into(),
        McpServerConfig {
            command: script.display().to_string(),
            ..Default::default()
        },
    );
    let workspace = temp.path().canonicalize().unwrap();
    let manager = McpManager::connect(&config.mcp, &workspace).await;
    let mut agent = Agent::new_with_mcp(
        config,
        Box::new(model),
        Box::new(NullSink),
        workspace,
        ToolProfile::Interactive,
        manager.clone(),
    );

    let result = agent
        .run("use the MCP fixture", RunOptions::default())
        .await
        .unwrap();
    assert!(result.success);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1]
                .enabled_tools
                .as_ref()
                .unwrap()
                .iter()
                .any(|tool| tool == "mcp__fixture__inspect")
        );
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| message.content.contains("mcp-observation"))
        );
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn interactive_agent_progressively_loads_a_matching_skill() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let skill_directory = temp.path().join(".wecode/skills/reviewer");
    std::fs::create_dir_all(&skill_directory).unwrap();
    std::fs::write(
        skill_directory.join("SKILL.md"),
        "---\nname: reviewer\ndescription: Review Rust changes for correctness.\n---\n# Review instructions\nCheck error paths and tests.\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::SearchTools {
                query: "review Rust correctness".into(),
                limit: None,
            },
            Action::LoadSkill {
                name: "reviewer".into(),
                path: None,
                offset: None,
                limit: None,
            },
            Action::Finish {
                summary: "Skill instructions followed".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    config.skills = SkillsConfig {
        discover_user: false,
        compatibility_directories: false,
        ..Default::default()
    };
    let workspace = temp.path().canonicalize().unwrap();
    let skills = SkillCatalog::discover(&workspace, &config.skills).unwrap();
    assert_eq!(skills.len(), 1);
    let manager = McpManager::connect(&config.mcp, &workspace).await;
    let mut agent = Agent::new_with_extensions(
        config,
        Box::new(model),
        Box::new(NullSink),
        workspace,
        ToolProfile::Interactive,
        manager.clone(),
        skills,
    );

    let result = agent
        .run("review this repository", RunOptions::default())
        .await
        .unwrap();
    assert!(result.success);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].system.contains("<name>reviewer</name>"));
        assert!(
            requests[0]
                .system
                .contains("Review Rust changes for correctness.")
        );
        assert!(!requests[0].system.contains("# Review instructions"));
        assert!(
            requests[1]
                .enabled_tools
                .as_ref()
                .unwrap()
                .iter()
                .any(|tool| tool == "load_skill")
        );
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.content.contains("skill `reviewer`"))
        );
        assert!(requests[2].messages.iter().any(|message| {
            message.content.contains("# Review instructions")
                && message.content.contains("Check error paths and tests.")
        }));
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn interactive_plain_text_is_a_smooth_final_response() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let model = FakeModel {
        responses: Mutex::new(VecDeque::from(["The review is complete.".into()])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run("review the repository", RunOptions::default())
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.steps, 1);
    assert_eq!(result.summary, "The review is complete.");
}

#[tokio::test]
async fn automatic_compaction_uses_a_tool_free_model_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let response = |text: &str| ModelResponse {
        text: text.into(),
        tool_calls: Vec::new(),
        action: None,
        additional_actions: Vec::new(),
        usage: Usage::default(),
        cache_hit: false,
        stop_reason: Default::default(),
    };
    let model = ScriptedResponseModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            response("## Goal\nContinue the parser work\n\n## Next Steps\n1. Finish the parser"),
            response("Compaction preserved the work."),
        ])),
    };
    let mut config = Config::default();
    config.agent.context_max_tokens = 1;
    config.agent.context_keep_messages = 4;
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );
    let history = (0..12)
        .flat_map(|index| {
            [
                serde_json::json!({"role": "assistant", "content": format!("analysis {index}")}),
                serde_json::json!({"role": "user", "content": format!("result {index}")}),
            ]
        })
        .collect::<Vec<_>>();
    let mut messages = vec![serde_json::json!({"role": "user", "content": "original task"})];
    messages.extend(history);
    let mut conversation: Conversation =
        serde_json::from_value(serde_json::json!({"messages": messages})).unwrap();

    let result = agent
        .run_in_conversation(
            "continue",
            RunOptions {
                session_id: Some("compaction-test".into()),
                ..RunOptions::default()
            },
            &mut conversation,
        )
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.harness.compactions, 1);
    assert_eq!(result.harness.model_turns, 2);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .enabled_tools
            .as_ref()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        requests[0]
            .system
            .contains("context summarization assistant")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.starts_with("[wecode-context-summary-v2]"))
    );
}

#[tokio::test]
async fn isolated_review_reads_context_and_returns_validated_findings() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::write(
        temp.path().join("src.rs"),
        "pub fn divide(value: u64, divisor: u64) -> u64 {\n    value / divisor\n}\n",
    )
    .unwrap();
    let patch = "\
diff --git a/src.rs b/src.rs
new file mode 100644
--- /dev/null
+++ b/src.rs
@@ -0,0 +1,3 @@
+pub fn divide(value: u64, divisor: u64) -> u64 {
+    value / divisor
+}
";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let summary = serde_json::json!({
        "findings": [{
            "title": "[P1] Guard a zero divisor",
            "body": "Calling this API with a zero divisor panics instead of returning a controlled error.",
            "confidence_score": 0.99,
            "priority": 1,
            "code_location": {
                "path": "src.rs",
                "line_range": {"start": 2, "end": 2}
            }
        }],
        "overall_correctness": "patch is incorrect",
        "overall_explanation": "The new API has one input-triggered panic.",
        "overall_confidence_score": 0.98
    })
    .to_string();
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::ReadFile {
                path: "src.rs".into(),
                offset: None,
                limit: None,
            },
            Action::Finish { summary },
        ])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Review,
    );
    let prompt = review_prompt(
        &ReviewRequest::parse("panic behavior").unwrap(),
        patch,
        90_000,
    );

    let result = agent.run(&prompt, RunOptions::default()).await.unwrap();
    let parsed = parse_review(&result.summary, temp.path(), patch);

    assert!(result.success);
    assert!(parsed.structured);
    assert_eq!(parsed.output.findings.len(), 1);
    assert_eq!(parsed.output.findings[0].priority, 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].system.contains("read-only code reviewer"));
    assert!(
        requests[0].messages[0]
            .content
            .contains("UNTRUSTED GIT PATCH")
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("value / divisor"))
    );
}

#[tokio::test]
async fn review_accepts_plain_structured_json_without_format_retries() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let summary = r#"{"findings":[],"overall_correctness":"patch is correct","overall_explanation":"No actionable findings.","overall_confidence_score":0.9}"#;
    let model = FakeModel {
        responses: Mutex::new(VecDeque::from([summary.into()])),
    };
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Review,
    );

    let result = agent
        .run("review the supplied patch", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.steps, 1);
    assert_eq!(result.summary, summary);
}

#[tokio::test]
async fn interactive_lsp_result_reaches_the_next_model_turn() {
    if Command::new("clangd").arg("--version").output().is_err() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    std::fs::write(
        temp.path().join("sample.c"),
        "static int add(int a, int b) { return a + b; }\nint main(void) { return add(1, 2); }\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::Lsp {
                operation: LspOperation::DocumentSymbols,
                path: "sample.c".into(),
                line: None,
                character: None,
                query: None,
            },
            Action::Finish {
                summary: "found the C symbols".into(),
            },
        ])),
    };
    let lsp = LspManager::new(
        LspConfig {
            auto_detect: false,
            servers: BTreeMap::from([(
                "clangd".into(),
                LspServerConfig {
                    command: "clangd".into(),
                    extensions: BTreeMap::from([(".c".into(), "c".into())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        },
        temp.path().to_path_buf(),
        None,
    )
    .unwrap();
    let mut config = Config::default();
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new_with_profile(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
        ToolProfile::Interactive,
    );

    let result = agent
        .run(
            "inspect the symbols in sample.c",
            RunOptions {
                lsp: Some(lsp.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.success);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|message| {
            message.content.contains("main") && message.content.contains("add")
        }));
    }
    lsp.shutdown().await;
}

#[tokio::test]
async fn noninteractive_untrusted_policy_denies_workspace_write_without_hanging() {
    let temp = tempfile::tempdir().unwrap();
    init_fixture(temp.path());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = CapturingModel {
        requests: requests.clone(),
        responses: Mutex::new(VecDeque::from([
            Action::Shell {
                command: "printf blocked > denied.txt".into(),
                description: "write a file".into(),
            },
            Action::Finish {
                summary: "write was denied".into(),
            },
        ])),
    };
    let mut config = Config::default();
    config.agent.approval_policy = ApprovalPolicy::UnlessTrusted;
    config.agent.trajectory_directory = temp.path().join("trajectories");
    config.cache.directory = temp.path().join("cache");
    let mut agent = Agent::new(
        config,
        Box::new(model),
        Box::new(NullSink),
        temp.path().canonicalize().unwrap(),
    );

    let result = agent
        .run("try a write", RunOptions::default())
        .await
        .unwrap();

    assert!(result.success);
    assert!(!temp.path().join("denied.txt").exists());
    assert!(
        requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|message| { message.content.contains("no approval channel is available") })
    );
}

fn init_fixture(directory: &std::path::Path) {
    git(directory, &["init"]);
    std::fs::write(directory.join("README.md"), "fixture\n").unwrap();
    git(directory, &["add", "README.md"]);
    git(
        directory,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ],
    );
}

fn git(directory: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}
