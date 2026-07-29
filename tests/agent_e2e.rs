use std::collections::VecDeque;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use wecode::agent::{Agent, Conversation, RunOptions};
use wecode::cache::ResponseCache;
use wecode::config::{CacheConfig, Config};
use wecode::events::{Event, EventSink};
use wecode::model::{CompletionRequest, Model, ModelResponse, Usage};
use wecode::protocol::Action;

struct FakeModel {
    responses: Mutex<VecDeque<String>>,
}

#[async_trait]
impl Model for FakeModel {
    async fn complete(&self, _request: CompletionRequest) -> Result<ModelResponse> {
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
    async fn complete(&self, request: CompletionRequest) -> Result<ModelResponse> {
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
    async fn complete(&self, _request: CompletionRequest) -> Result<ModelResponse> {
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
