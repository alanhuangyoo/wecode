use std::collections::VecDeque;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Notify;
use wecode::agent::{Agent, Conversation, RunOptions};
use wecode::approval::{ApprovalClient, ApprovalDecision};
use wecode::cache::ResponseCache;
use wecode::config::{ApprovalPolicy, CacheConfig, Config};
use wecode::control::CancellationToken;
use wecode::events::{Event, EventSink};
use wecode::input_queue::InputQueue;
use wecode::model::{CompletionRequest, Model, ModelResponse, ModelStream, Usage};
use wecode::protocol::Action;

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
            action: None,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            cache_hit: false,
        })
    }
}

struct NativeFakeModel {
    responses: Mutex<VecDeque<Action>>,
}

struct CapturingModel {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    responses: Mutex<VecDeque<Action>>,
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
            action: Some(action),
            usage: Usage::default(),
            cache_hit: false,
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
            action: Some(action),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            cache_hit: false,
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
            action: Some(Action::Finish {
                summary: if call == 0 {
                    "premature finish".into()
                } else {
                    "steering applied".into()
                },
            }),
            usage: Usage::default(),
            cache_hit: false,
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

    let model = FakeModel {
        responses: Mutex::new(VecDeque::from([
            r#"{"action":"shell","command":"printf hello > result.txt","description":"create result"}"#.into(),
            r#"{"action":"shell","command":"test \"$(cat result.txt)\" = hello","description":"check result"}"#.into(),
            r#"{"action":"finish","summary":"created and checked result.txt"}"#.into(),
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
            queue.steer("also explain the result");
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
