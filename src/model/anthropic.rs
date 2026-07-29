use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, RawModel, Usage, action_from_tool_call, action_text,
    merge_adjacent_messages, tool_definitions,
};
use crate::config::{ModelConfig, PromptCacheMode};
use crate::context::Role;
use crate::model::http::send_json;

pub struct AnthropicModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(config: ModelConfig, api_key: Option<String>, client: reqwest::Client) -> Self {
        Self {
            config,
            api_key,
            client,
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        if let Some(api_key) = &self.api_key {
            headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(api_key)
                    .context("API key contains invalid header characters")?,
            );
        }
        Ok(headers)
    }

    fn body(&self, request: &CompletionRequest) -> Value {
        let cache_control = match self.config.prompt_cache {
            PromptCacheMode::Off => None,
            PromptCacheMode::Auto => Some(json!({"type": "ephemeral"})),
            PromptCacheMode::Long => Some(json!({"type": "ephemeral", "ttl": "1h"})),
        };
        let mut system = json!({"type": "text", "text": request.system});
        if let Some(cache_control) = &cache_control {
            system["cache_control"] = cache_control.clone();
        }

        let normalized = merge_adjacent_messages(&request.messages);
        let last_index = normalized.len().saturating_sub(1);
        let messages: Vec<Value> = normalized
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let mut block = json!({"type": "text", "text": message.content});
                if index == last_index
                    && let Some(cache_control) = &cache_control
                {
                    block["cache_control"] = cache_control.clone();
                }
                json!({
                    "role": match message.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    },
                    "content": [block],
                })
            })
            .collect();
        let mut body = json!({
            "model": self.config.model,
            "system": [system],
            "messages": messages,
            "max_tokens": self.config.max_output_tokens,
        });
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if self.config.native_tools {
            body["tools"] = Value::Array(
                tool_definitions()
                    .into_iter()
                    .map(|definition| {
                        json!({
                            "name": definition["name"],
                            "description": definition["description"],
                            "input_schema": definition["parameters"],
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!({"type": "auto"});
        }
        body
    }
}

#[async_trait]
impl RawModel for AnthropicModel {
    async fn complete_raw(&self, request: &CompletionRequest) -> Result<ModelResponse> {
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        let value = send_json(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &self.body(request),
        )
        .await?;
        parse_response(value)
    }
}

fn parse_response(value: Value) -> Result<ModelResponse> {
    let action = value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|block| {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .context("Anthropic tool_use block did not contain a name")?;
            action_from_tool_call(
                name,
                block.get("input").cloned().unwrap_or_else(|| json!({})),
            )
        })
        .transpose()?;
    let mut text = value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.is_empty()
        && let Some(action) = &action
    {
        text = action_text(action);
    }
    if text.is_empty() {
        anyhow::bail!("Anthropic response contained neither text nor a supported tool call");
    }
    Ok(ModelResponse {
        text,
        action,
        usage: Usage {
            input_tokens: at(&value, "/usage/input_tokens"),
            output_tokens: at(&value, "/usage/output_tokens"),
            cache_read_tokens: at(&value, "/usage/cache_read_input_tokens"),
            cache_write_tokens: at(&value, "/usage/cache_creation_input_tokens"),
        },
        cache_hit: false,
    })
}

fn at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_tool_use() {
        let response = parse_response(json!({
            "content": [{
                "type": "tool_use",
                "id": "tool_1",
                "name": "apply_patch",
                "input": {
                    "patch": "*** Begin Patch\n*** Add File: a.txt\n+ok\n*** End Patch",
                    "description": "add fixture"
                }
            }],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        }))
        .unwrap();

        assert!(matches!(
            response.action,
            Some(crate::protocol::Action::Patch { .. })
        ));
        assert_eq!(response.usage.input_tokens, 10);
    }
}
