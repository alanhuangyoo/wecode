use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, RawModel, Usage, action_from_tool_call, action_text,
    merge_adjacent_messages, tool_definitions,
};
use crate::config::ModelConfig;
use crate::context::Role;
use crate::model::http::send_json;

pub struct GeminiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl GeminiModel {
    pub fn new(config: ModelConfig, api_key: Option<String>, client: reqwest::Client) -> Self {
        Self {
            config,
            api_key,
            client,
        }
    }

    fn body(&self, request: &CompletionRequest) -> Value {
        let contents: Vec<Value> = merge_adjacent_messages(&request.messages)
            .iter()
            .map(|message| {
                json!({
                    "role": match message.role {
                        Role::User => "user",
                        Role::Assistant => "model",
                    },
                    "parts": [{"text": message.content}],
                })
            })
            .collect();
        let mut generation = json!({
            "maxOutputTokens": self.config.max_output_tokens,
        });
        if let Some(temperature) = self.config.temperature {
            generation["temperature"] = json!(temperature);
        }
        let mut body = json!({
            "systemInstruction": {"parts": [{"text": request.system}]},
            "contents": contents,
            "generationConfig": generation,
        });
        if self.config.native_tools {
            body["tools"] = json!([{
                "functionDeclarations": tool_definitions()
            }]);
            body["toolConfig"] = json!({
                "functionCallingConfig": {"mode": "AUTO"}
            });
        }
        body
    }
}

#[async_trait]
impl RawModel for GeminiModel {
    async fn complete_raw(&self, request: &CompletionRequest) -> Result<ModelResponse> {
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.config.base_url.trim_end_matches('/'),
            self.config.model
        );
        let mut headers = HeaderMap::new();
        if let Some(api_key) = &self.api_key {
            headers.insert(
                HeaderName::from_static("x-goog-api-key"),
                HeaderValue::from_str(api_key)
                    .context("API key contains invalid header characters")?,
            );
        }
        let value = send_json(
            &self.client,
            reqwest::Method::POST,
            &url,
            headers,
            &self.body(request),
        )
        .await?;
        parse_response(value)
    }
}

fn parse_response(value: Value) -> Result<ModelResponse> {
    let action = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|part| part.get("functionCall"))
        .map(|call| {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .context("Gemini functionCall did not contain a name")?;
            action_from_tool_call(name, call.get("args").cloned().unwrap_or_else(|| json!({})))
        })
        .transpose()?;
    let mut text = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.is_empty()
        && let Some(action) = &action
    {
        text = action_text(action);
    }
    if text.is_empty() {
        anyhow::bail!("Gemini response contained neither text nor a supported tool call");
    }
    Ok(ModelResponse {
        text,
        action,
        usage: Usage {
            input_tokens: at(&value, "/usageMetadata/promptTokenCount"),
            output_tokens: at(&value, "/usageMetadata/candidatesTokenCount"),
            cache_read_tokens: at(&value, "/usageMetadata/cachedContentTokenCount"),
            cache_write_tokens: 0,
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
    fn parses_native_function_call() {
        let response = parse_response(json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "finish",
                            "args": {"summary": "done"}
                        }
                    }]
                }
            }],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 2}
        }))
        .unwrap();

        assert_eq!(
            response.action,
            Some(crate::protocol::Action::Finish {
                summary: "done".into(),
            })
        );
        assert_eq!(response.usage.output_tokens, 2);
    }
}
