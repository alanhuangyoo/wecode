use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, RawModel, Usage, action_from_tool_call, action_text,
    tool_definitions,
};
use crate::config::{ModelConfig, PromptCacheMode, WireApi};
use crate::context::Role;
use crate::model::http::send_json;

pub struct OpenAiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiModel {
    pub fn new(config: ModelConfig, api_key: Option<String>, client: reqwest::Client) -> Self {
        Self {
            config,
            api_key,
            client,
        }
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        if let Some(api_key) = &self.api_key {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .context("API key contains invalid header characters")?,
            );
        }
        Ok(headers)
    }

    fn chat_body(&self, request: &CompletionRequest) -> Value {
        let mut messages = vec![json!({
            "role": "system",
            "content": request.system,
        })];
        messages.extend(request.messages.iter().map(|message| {
            json!({
                "role": match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                "content": message.content,
            })
        }));
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": self.config.max_output_tokens,
        });
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if self.config.send_prompt_cache_key && self.config.prompt_cache != PromptCacheMode::Off {
            body["prompt_cache_key"] = json!(clamp_cache_key(&request.session_id));
            if self.config.prompt_cache == PromptCacheMode::Long {
                body["prompt_cache_retention"] = json!("24h");
            }
        }
        if self.config.native_tools {
            body["tools"] = Value::Array(
                tool_definitions()
                    .into_iter()
                    .map(|definition| json!({"type": "function", "function": definition}))
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(false);
        }
        body
    }

    fn responses_body(&self, request: &CompletionRequest) -> Value {
        let input: Vec<Value> = request
            .messages
            .iter()
            .map(|message| {
                json!({
                    "role": match message.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    },
                    "content": message.content,
                })
            })
            .collect();
        let mut body = json!({
            "model": self.config.model,
            "instructions": request.system,
            "input": input,
            "max_output_tokens": self.config.max_output_tokens,
            "store": false,
        });
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if self.config.prompt_cache != PromptCacheMode::Off {
            body["prompt_cache_key"] = json!(clamp_cache_key(&request.session_id));
            if self.config.prompt_cache == PromptCacheMode::Long {
                body["prompt_cache_retention"] = json!("24h");
            }
        }
        if self.config.native_tools {
            body["tools"] = Value::Array(
                tool_definitions()
                    .into_iter()
                    .map(|mut definition| {
                        definition["type"] = json!("function");
                        definition
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(false);
        }
        body
    }
}

#[async_trait]
impl RawModel for OpenAiModel {
    async fn complete_raw(&self, request: &CompletionRequest) -> Result<ModelResponse> {
        let (endpoint, body) = match self.config.wire_api {
            WireApi::ChatCompletions => ("chat/completions", self.chat_body(request)),
            WireApi::Responses => ("responses", self.responses_body(request)),
        };
        let url = format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            endpoint
        );
        let value = send_json(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &body,
        )
        .await?;
        match self.config.wire_api {
            WireApi::ChatCompletions => parse_chat_response(value),
            WireApi::Responses => parse_responses_response(value),
        }
    }
}

fn parse_chat_response(value: Value) -> Result<ModelResponse> {
    let action = value
        .pointer("/choices/0/message/tool_calls/0/function")
        .and_then(parse_openai_tool_call)
        .transpose()?;
    let mut text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty()
        && let Some(action) = &action
    {
        text = action_text(action);
    }
    if text.is_empty() {
        bail!("OpenAI-compatible response contained neither text nor a supported tool call");
    }
    let usage = Usage {
        input_tokens: u64_at(&value, "/usage/prompt_tokens"),
        output_tokens: u64_at(&value, "/usage/completion_tokens"),
        cache_read_tokens: u64_at(&value, "/usage/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: 0,
    };
    Ok(ModelResponse {
        text,
        action,
        usage,
        cache_hit: false,
    })
}

fn parse_responses_response(value: Value) -> Result<ModelResponse> {
    let mut action = None;
    let mut text = value
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty()
        && let Some(output) = value.get("output").and_then(Value::as_array)
    {
        for item in output {
            if action.is_none() && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                action = parse_openai_tool_call(item).transpose()?;
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                        text.push_str(part_text);
                    }
                }
            }
        }
    }
    if text.is_empty()
        && let Some(action) = &action
    {
        text = action_text(action);
    }
    if text.is_empty() {
        bail!("OpenAI Responses result contained neither text nor a supported tool call");
    }
    let usage = Usage {
        input_tokens: u64_at(&value, "/usage/input_tokens"),
        output_tokens: u64_at(&value, "/usage/output_tokens"),
        cache_read_tokens: u64_at(&value, "/usage/input_tokens_details/cached_tokens"),
        cache_write_tokens: 0,
    };
    Ok(ModelResponse {
        text,
        action,
        usage,
        cache_hit: false,
    })
}

fn parse_openai_tool_call(function: &Value) -> Option<Result<crate::protocol::Action>> {
    let name = function.get("name")?.as_str()?;
    let arguments = function.get("arguments")?;
    let arguments = match arguments {
        Value::String(value) => match serde_json::from_str(value) {
            Ok(value) => value,
            Err(error) => return Some(Err(error.into())),
        },
        value => value.clone(),
    };
    Some(action_from_tool_call(name, arguments))
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn clamp_cache_key(value: &str) -> String {
    value.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_openai_response_shapes() {
        let chat = parse_chat_response(json!({
            "choices": [{"message": {"content": "{\"action\":\"finish\",\"summary\":\"ok\"}"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 8}}
        }))
        .unwrap();
        assert_eq!(chat.usage.cache_read_tokens, 8);

        let responses = parse_responses_response(json!({
            "output": [{"content": [{"type": "output_text", "text": "done"}]}],
            "usage": {"input_tokens": 4, "output_tokens": 1}
        }))
        .unwrap();
        assert_eq!(responses.text, "done");
    }

    #[test]
    fn parses_native_tools_from_chat_and_responses() {
        let chat = parse_chat_response(json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"command\":\"cargo test\",\"description\":\"verify\"}"
                    }
                }]
            }}],
            "usage": {}
        }))
        .unwrap();
        assert_eq!(
            chat.action,
            Some(crate::protocol::Action::Shell {
                command: "cargo test".into(),
                description: "verify".into(),
            })
        );

        let responses = parse_responses_response(json!({
            "output": [{
                "type": "function_call",
                "name": "finish",
                "arguments": "{\"summary\":\"all checks pass\"}"
            }],
            "usage": {}
        }))
        .unwrap();
        assert_eq!(
            responses.action,
            Some(crate::protocol::Action::Finish {
                summary: "all checks pass".into(),
            })
        );
    }
}
