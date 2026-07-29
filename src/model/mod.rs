mod anthropic;
mod gemini;
mod http;
mod openai;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cache::ResponseCache;
use crate::config::{ModelConfig, ProviderFamily};
use crate::context::Message;
use crate::protocol::{Action, validate_action};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub session_id: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelResponse {
    pub text: String,
    #[serde(default)]
    pub action: Option<Action>,
    pub usage: Usage,
    #[serde(default)]
    pub cache_hit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
}

pub trait ModelStream: Send + Sync {
    fn emit(&self, event: ModelStreamEvent) -> Result<()>;
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest,
        stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse>;
}

#[async_trait]
trait RawModel: Send + Sync {
    async fn complete_raw(
        &self,
        request: &CompletionRequest,
        stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse>;
}

struct CachedModel {
    inner: Arc<dyn RawModel>,
    cache: ResponseCache,
    namespace: String,
}

#[async_trait]
impl Model for CachedModel {
    async fn complete(
        &self,
        request: CompletionRequest,
        stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        let key = self.cache.key(&self.namespace, &request)?;
        if let Some(mut response) = self.cache.get::<ModelResponse>(&key).await? {
            response.cache_hit = true;
            return Ok(response);
        }
        let mut response = self.inner.complete_raw(&request, stream).await?;
        response.cache_hit = false;
        self.cache.put(&key, &response).await?;
        Ok(response)
    }
}

pub fn create_model(
    config: &ModelConfig,
    api_key: Option<String>,
    cache: ResponseCache,
) -> Result<Box<dyn Model>> {
    let namespace = cache_namespace(config, api_key.as_deref())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("wecode/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let inner: Arc<dyn RawModel> = match config.family {
        ProviderFamily::OpenAiCompatible => {
            Arc::new(openai::OpenAiModel::new(config.clone(), api_key, client))
        }
        ProviderFamily::Anthropic => Arc::new(anthropic::AnthropicModel::new(
            config.clone(),
            api_key,
            client,
        )),
        ProviderFamily::Gemini => {
            Arc::new(gemini::GeminiModel::new(config.clone(), api_key, client))
        }
    };
    Ok(Box::new(CachedModel {
        inner,
        cache,
        namespace,
    }))
}

fn cache_namespace(config: &ModelConfig, api_key: Option<&str>) -> Result<String> {
    let credential_scope = api_key
        .map(credential_fingerprint)
        .unwrap_or_else(|| "anonymous".into());
    Ok(format!(
        "{}:{credential_scope}",
        serde_json::to_string(config)?
    ))
}

fn credential_fingerprint(api_key: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(api_key.as_bytes()));
    digest[..16].to_owned()
}

fn merge_adjacent_messages(messages: &[Message]) -> Vec<Message> {
    let mut result: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(last) = result.last_mut()
            && last.role == message.role
        {
            last.content.push_str("\n\n");
            last.content.push_str(&message.content);
            continue;
        }
        result.push(message.clone());
    }
    result
}

pub(crate) fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_file",
            "description": "Read a UTF-8 text file in the workspace with stable line numbers. Use offset and limit to continue through large files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-indexed first line."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum lines to return."}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_files",
            "description": "List a workspace directory deterministically. Respects .gitignore and excludes .git metadata.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path. Defaults to the workspace root."},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 8, "description": "Recursive depth. Defaults to 2."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum entries."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "glob",
            "description": "Find workspace files by glob without spawning a shell. Respects .gitignore and returns deterministic paths.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob such as **/*.rs or *.toml."},
                    "path": {"type": "string", "description": "Workspace-relative search root. Defaults to ."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum matches."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "grep",
            "description": "Search text files in the workspace. Supports regex or literal matching, optional context, glob filtering, .gitignore, and deterministic bounded output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression or literal text."},
                    "path": {"type": "string", "description": "Workspace-relative file or directory. Defaults to ."},
                    "glob": {"type": "string", "description": "Optional file filter such as **/*.rs."},
                    "literal": {"type": "boolean", "description": "Treat pattern as literal text. Defaults to false."},
                    "ignore_case": {"type": "boolean", "description": "Case-insensitive search. Defaults to false."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 5, "description": "Context lines before and after matches."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum matching lines."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "shell",
            "description": "Run one non-interactive shell command in the repository workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute."},
                    "description": {"type": "string", "description": "A short description of the intent."}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apply_patch",
            "description": "Apply a Codex-format patch beginning with *** Begin Patch and ending with *** End Patch.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The complete Codex apply_patch payload."},
                    "description": {"type": "string", "description": "A short description of the edit."}
                },
                "required": ["patch"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "finish",
            "description": "Finish only after the task is complete and relevant verification has run.",
            "parameters": {
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "What changed and how it was verified."}
                },
                "required": ["summary"],
                "additionalProperties": false
            }
        }),
    ]
}

pub(crate) fn action_from_tool_call(name: &str, arguments: Value) -> Result<Action> {
    let get_string = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let action = match name {
        "read_file" | "read" => Action::ReadFile {
            path: get_string("path"),
            offset: arguments
                .get("offset")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        },
        "list_files" | "list_dir" | "ls" => Action::ListFiles {
            path: {
                let value = get_string("path");
                if value.is_empty() { ".".into() } else { value }
            },
            depth: arguments
                .get("depth")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        },
        "glob" | "find" => Action::Glob {
            pattern: get_string("pattern"),
            path: {
                let value = get_string("path");
                if value.is_empty() { ".".into() } else { value }
            },
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        },
        "grep" | "search" => Action::Grep {
            pattern: get_string("pattern"),
            path: {
                let value = get_string("path");
                if value.is_empty() { ".".into() } else { value }
            },
            glob: arguments
                .get("glob")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            literal: arguments
                .get("literal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ignore_case: arguments
                .get("ignore_case")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            context: arguments
                .get("context")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        },
        "shell" => Action::Shell {
            command: get_string("command"),
            description: get_string("description"),
        },
        "apply_patch" | "patch" => Action::Patch {
            patch: get_string("patch"),
            description: get_string("description"),
        },
        "finish" => Action::Finish {
            summary: get_string("summary"),
        },
        _ => anyhow::bail!("provider returned unknown tool call {name:?}"),
    };
    validate_action(&action)?;
    Ok(action)
}

pub(crate) fn action_text(action: &Action) -> String {
    serde_json::to_string(action).expect("Action is always serializable")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::CacheConfig;

    struct FakeRawModel {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RawModel for FakeRawModel {
        async fn complete_raw(
            &self,
            _request: &CompletionRequest,
            _stream: Option<&dyn ModelStream>,
        ) -> Result<ModelResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                text: r#"{"action":"finish","summary":"done"}"#.into(),
                action: None,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    ..Default::default()
                },
                cache_hit: false,
            })
        }
    }

    #[tokio::test]
    async fn identical_requests_call_raw_provider_once() {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let model = CachedModel {
            inner: Arc::new(FakeRawModel {
                calls: calls.clone(),
            }),
            cache: ResponseCache::new(CacheConfig {
                directory: temp.path().join("cache"),
                ..Default::default()
            })
            .unwrap(),
            namespace: "test".into(),
        };
        let request = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("task")],
            session_id: "session".into(),
        };

        let first = model.complete(request.clone(), None).await.unwrap();
        let second = model.complete(request, None).await.unwrap();

        assert!(!first.cache_hit);
        assert!(second.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_namespace_is_scoped_to_credentials_without_exposing_them() {
        let config = ModelConfig::default();
        let first = cache_namespace(&config, Some("test-key-one")).unwrap();
        let second = cache_namespace(&config, Some("test-key-two")).unwrap();

        assert_ne!(first, second);
        assert!(!first.contains("test-key-one"));
        assert_eq!(
            first,
            cache_namespace(&config, Some("test-key-one")).unwrap()
        );
    }

    #[test]
    fn native_tool_definitions_map_to_actions() {
        assert_eq!(tool_definitions().len(), 7);
        assert_eq!(
            action_from_tool_call(
                "shell",
                json!({"command": "cargo test", "description": "verify"})
            )
            .unwrap(),
            Action::Shell {
                command: "cargo test".into(),
                description: "verify".into(),
            }
        );
        let read = action_from_tool_call(
            "read_file",
            json!({"path": "src/lib.rs", "offset": 4, "limit": 20}),
        )
        .unwrap();
        assert_eq!(
            read,
            Action::ReadFile {
                path: "src/lib.rs".into(),
                offset: Some(4),
                limit: Some(20),
            }
        );
        let grep = action_from_tool_call("grep", json!({"pattern": "TODO"})).unwrap();
        assert_eq!(
            grep,
            Action::Grep {
                pattern: "TODO".into(),
                path: ".".into(),
                glob: None,
                literal: false,
                ignore_case: false,
                context: None,
                limit: None,
            }
        );
        assert!(action_from_tool_call("unknown", json!({})).is_err());
    }
}
