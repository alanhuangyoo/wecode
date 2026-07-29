use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, ModelStream, ModelStreamEvent, RawModel, ToolProfile, Usage,
    action_batch_text, action_from_tool_call, merge_adjacent_messages, tool_definitions,
};
use crate::config::ModelConfig;
use crate::context::Role;
use crate::model::http::{RetryPolicy, SseEvent, send_json, send_sse};

pub struct GeminiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    tool_profile: ToolProfile,
}

impl GeminiModel {
    pub fn new(
        config: ModelConfig,
        api_key: Option<String>,
        client: reqwest::Client,
        tool_profile: ToolProfile,
    ) -> Self {
        Self {
            config,
            api_key,
            client,
            tool_profile,
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
                "functionDeclarations": tool_definitions(self.tool_profile)
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
    async fn complete_raw(
        &self,
        request: &CompletionRequest,
        stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        if self.config.streaming
            && let Some(stream) = stream
        {
            return self.complete_stream(request, stream).await;
        }
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
            RetryPolicy::from_config(&self.config),
        )
        .await?;
        parse_response(value)
    }
}

impl GeminiModel {
    async fn complete_stream(
        &self,
        request: &CompletionRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse> {
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
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
        let mut state = GeminiStreamState::default();
        send_sse(
            &self.client,
            reqwest::Method::POST,
            &url,
            headers,
            &self.body(request),
            RetryPolicy::from_config(&self.config),
            |event| ingest_stream_event(event, stream, &mut state),
        )
        .await?;
        state.finish()
    }
}

#[derive(Default)]
struct GeminiStreamState {
    text: String,
    tool_calls: Vec<(String, Value)>,
    usage: Usage,
}

impl GeminiStreamState {
    fn finish(self) -> Result<ModelResponse> {
        let mut actions = self
            .tool_calls
            .into_iter()
            .map(|(name, arguments)| action_from_tool_call(&name, arguments))
            .collect::<Result<Vec<_>>>()?;
        let text = if self.text.is_empty() {
            (!actions.is_empty())
                .then(|| action_batch_text(&actions))
                .context("Gemini stream contained neither text nor a tool call")?
        } else {
            self.text
        };
        let action = (!actions.is_empty()).then(|| actions.remove(0));
        Ok(ModelResponse {
            text,
            action,
            additional_actions: actions,
            usage: self.usage,
            cache_hit: false,
        })
    }
}

fn ingest_stream_event(
    event: SseEvent,
    stream: &dyn ModelStream,
    state: &mut GeminiStreamState,
) -> Result<()> {
    if event.data == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(&event.data).context("invalid Gemini SSE event")?;
    if let Some(usage) = value.get("usageMetadata") {
        state.usage = Usage {
            input_tokens: usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write_tokens: 0,
        };
    }
    for part in value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                emit_delta(stream, ModelStreamEvent::ReasoningDelta(text.to_owned()))?;
            } else {
                state.text.push_str(text);
                emit_delta(stream, ModelStreamEvent::TextDelta(text.to_owned()))?;
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if !name.is_empty() {
                state
                    .tool_calls
                    .push((name, call.get("args").cloned().unwrap_or_else(|| json!({}))));
            }
        }
    }
    Ok(())
}

fn emit_delta(stream: &dyn ModelStream, event: ModelStreamEvent) -> Result<()> {
    let empty = match &event {
        ModelStreamEvent::TextDelta(text) | ModelStreamEvent::ReasoningDelta(text) => {
            text.is_empty()
        }
    };
    if !empty {
        stream.emit(event)?;
    }
    Ok(())
}

fn parse_response(value: Value) -> Result<ModelResponse> {
    let mut actions = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("functionCall"))
        .map(|call| {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .context("Gemini functionCall did not contain a name")?;
            action_from_tool_call(name, call.get("args").cloned().unwrap_or_else(|| json!({})))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut text = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.is_empty() && !actions.is_empty() {
        text = action_batch_text(&actions);
    }
    if text.is_empty() {
        anyhow::bail!("Gemini response contained neither text nor a supported tool call");
    }
    let action = (!actions.is_empty()).then(|| actions.remove(0));
    Ok(ModelResponse {
        text,
        action,
        additional_actions: actions,
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
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingStream(Mutex<Vec<ModelStreamEvent>>);

    impl ModelStream for RecordingStream {
        fn emit(&self, event: ModelStreamEvent) -> Result<()> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

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

    #[test]
    fn preserves_multiple_gemini_function_calls() {
        let mut response = parse_response(json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "list_files", "args": {"path": "."}}},
                        {"functionCall": {"name": "grep", "args": {"pattern": "TODO"}}}
                    ]
                }
            }],
            "usageMetadata": {}
        }))
        .unwrap();

        assert!(matches!(
            response.take_actions().as_slice(),
            [
                crate::protocol::Action::ListFiles { .. },
                crate::protocol::Action::Grep { .. }
            ]
        ));
    }

    #[test]
    fn assembles_gemini_text_and_function_call_chunks() {
        let stream = RecordingStream::default();
        let mut state = GeminiStreamState::default();
        for data in [
            r#"{"candidates":[{"content":{"parts":[{"text":"plan ","thought":true},{"text":"working "}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"finish","args":{"summary":"done"}}}]}}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"cachedContentTokenCount":3}}"#,
        ] {
            ingest_stream_event(
                SseEvent {
                    event: None,
                    data: data.into(),
                },
                &stream,
                &mut state,
            )
            .unwrap();
        }
        let response = state.finish().unwrap();

        assert_eq!(response.text, "working ");
        assert_eq!(
            response.action,
            Some(crate::protocol::Action::Finish {
                summary: "done".into(),
            })
        );
        assert_eq!(response.usage.cache_read_tokens, 3);
        assert_eq!(
            *stream.0.lock().unwrap(),
            [
                ModelStreamEvent::ReasoningDelta("plan ".into()),
                ModelStreamEvent::TextDelta("working ".into()),
            ]
        );
    }
}
