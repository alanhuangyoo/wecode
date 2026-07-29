mod anthropic;
mod gemini;
mod http;
mod openai;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::ResponseCache;
use crate::config::{ModelConfig, ProviderFamily};
use crate::context::Message;
use crate::protocol::Action;
pub use crate::tool_registry::ToolProfile;
pub(crate) use crate::tool_registry::ToolRegistry;

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
    #[serde(default)]
    pub additional_actions: Vec<Action>,
    pub usage: Usage,
    #[serde(default)]
    pub cache_hit: bool,
}

impl ModelResponse {
    pub fn take_actions(&mut self) -> Vec<Action> {
        let mut actions =
            Vec::with_capacity(self.additional_actions.len() + usize::from(self.action.is_some()));
        if let Some(action) = self.action.take() {
            actions.push(action);
        }
        actions.append(&mut self.additional_actions);
        actions
    }
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
    create_model_with_profile(config, api_key, cache, ToolProfile::Coding)
}

pub fn create_model_with_profile(
    config: &ModelConfig,
    api_key: Option<String>,
    cache: ResponseCache,
    tool_profile: ToolProfile,
) -> Result<Box<dyn Model>> {
    create_model_with_tools(config, api_key, cache, tool_profile, Vec::new())
}

pub fn create_model_with_tools(
    config: &ModelConfig,
    api_key: Option<String>,
    cache: ResponseCache,
    tool_profile: ToolProfile,
    extra_tools: Vec<serde_json::Value>,
) -> Result<Box<dyn Model>> {
    let namespace =
        cache_namespace_with_tools(config, api_key.as_deref(), tool_profile, &extra_tools)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("wecode/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let inner: Arc<dyn RawModel> = match config.family {
        ProviderFamily::OpenAiCompatible => Arc::new(openai::OpenAiModel::new(
            config.clone(),
            api_key,
            client,
            tool_profile,
            extra_tools,
        )),
        ProviderFamily::Anthropic => Arc::new(anthropic::AnthropicModel::new(
            config.clone(),
            api_key,
            client,
            tool_profile,
            extra_tools,
        )),
        ProviderFamily::Gemini => Arc::new(gemini::GeminiModel::new(
            config.clone(),
            api_key,
            client,
            tool_profile,
            extra_tools,
        )),
    };
    Ok(Box::new(CachedModel {
        inner,
        cache,
        namespace,
    }))
}

fn cache_namespace_with_tools(
    config: &ModelConfig,
    api_key: Option<&str>,
    tool_profile: ToolProfile,
    extra_tools: &[serde_json::Value],
) -> Result<String> {
    let namespace = cache_namespace(config, api_key, tool_profile)?;
    if extra_tools.is_empty() {
        return Ok(namespace);
    }
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(extra_tools)?));
    Ok(format!("{namespace}:tools:{}", &digest[..16]))
}

fn cache_namespace(
    config: &ModelConfig,
    api_key: Option<&str>,
    tool_profile: ToolProfile,
) -> Result<String> {
    let credential_scope = api_key
        .map(credential_fingerprint)
        .unwrap_or_else(|| "anonymous".into());
    let config = serde_json::to_string(config)?;
    Ok(match tool_profile {
        ToolProfile::Coding => format!("{config}:{credential_scope}"),
        ToolProfile::Interactive => format!("{config}:interactive:{credential_scope}"),
        ToolProfile::ReadOnlySubagent => {
            format!("{config}:subagent-read-only:{credential_scope}")
        }
    })
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

pub(crate) fn tool_definitions(
    profile: ToolProfile,
    extra_tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut definitions = ToolRegistry::for_profile(profile).definitions();
    definitions.extend_from_slice(extra_tools);
    definitions
}

pub(crate) fn action_from_tool_call(name: &str, arguments: serde_json::Value) -> Result<Action> {
    ToolRegistry::parse_call(name, arguments)
}

pub(crate) fn action_text(action: &Action) -> String {
    serde_json::to_string(action).expect("Action is always serializable")
}

pub(crate) fn action_batch_text(actions: &[Action]) -> String {
    match actions {
        [action] => action_text(action),
        actions => serde_json::to_string(actions).expect("Action is always serializable"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

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
                additional_actions: Vec::new(),
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
        let first = cache_namespace(&config, Some("test-key-one"), ToolProfile::Coding).unwrap();
        let second = cache_namespace(&config, Some("test-key-two"), ToolProfile::Coding).unwrap();

        assert_ne!(first, second);
        assert!(!first.contains("test-key-one"));
        assert_eq!(
            first,
            format!(
                "{}:{}",
                serde_json::to_string(&config).unwrap(),
                credential_fingerprint("test-key-one")
            )
        );
        assert_eq!(
            first,
            cache_namespace(&config, Some("test-key-one"), ToolProfile::Coding).unwrap()
        );
        assert_ne!(
            first,
            cache_namespace(&config, Some("test-key-one"), ToolProfile::Interactive).unwrap()
        );
    }

    #[test]
    fn native_tool_definitions_map_to_actions() {
        assert_eq!(tool_definitions(ToolProfile::Coding, &[]).len(), 7);
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

    #[test]
    fn dynamic_tools_change_only_the_extended_cache_namespace() {
        let config = ModelConfig::default();
        let base =
            cache_namespace_with_tools(&config, Some("test-key"), ToolProfile::Interactive, &[])
                .unwrap();
        assert_eq!(
            base,
            cache_namespace(&config, Some("test-key"), ToolProfile::Interactive).unwrap()
        );
        let extended = cache_namespace_with_tools(
            &config,
            Some("test-key"),
            ToolProfile::Interactive,
            &[json!({
                "name": "mcp__fixture__echo",
                "description": "echo",
                "parameters": {"type": "object"}
            })],
        )
        .unwrap();
        assert_ne!(base, extended);
        assert!(extended.contains(":tools:"));
    }
}
