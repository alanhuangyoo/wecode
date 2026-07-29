use std::collections::VecDeque;
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use wecode::agent::{Agent, RunOptions};
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
