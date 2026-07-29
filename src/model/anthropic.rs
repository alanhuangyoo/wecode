use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, ModelStream, ModelStreamEvent, RawModel, ToolProfile, Usage,
    action_batch_text, action_from_tool_call, merge_adjacent_messages, tool_definitions,
};
use crate::config::{ModelConfig, PromptCacheMode};
use crate::context::Role;
use crate::model::http::{RetryPolicy, SseEvent, send_json, send_sse};

pub struct AnthropicModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    tool_profile: ToolProfile,
    extra_tools: Vec<Value>,
}

impl AnthropicModel {
    pub fn new(
        config: ModelConfig,
        api_key: Option<String>,
        client: reqwest::Client,
        tool_profile: ToolProfile,
        extra_tools: Vec<Value>,
    ) -> Self {
        Self {
            config,
            api_key,
            client,
            tool_profile,
            extra_tools,
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
                tool_definitions(self.tool_profile, &self.extra_tools)
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
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        let value = send_json(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &self.body(request),
            RetryPolicy::from_config(&self.config),
        )
        .await?;
        parse_response(value)
    }
}

impl AnthropicModel {
    async fn complete_stream(
        &self,
        request: &CompletionRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse> {
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        let mut body = self.body(request);
        body["stream"] = json!(true);
        let mut state = AnthropicStreamState::default();
        send_sse(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &body,
            RetryPolicy::from_config(&self.config),
            |event| ingest_stream_event(event, stream, &mut state),
        )
        .await?;
        state.finish()
    }
}

#[derive(Default)]
struct AnthropicStreamState {
    text: String,
    tool_calls: BTreeMap<usize, PartialToolUse>,
    usage: Usage,
}

#[derive(Default)]
struct PartialToolUse {
    name: String,
    input: String,
}

impl AnthropicStreamState {
    fn finish(self) -> Result<ModelResponse> {
        let mut actions = self
            .tool_calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| {
                let input = if call.input.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&call.input)
                        .context("Anthropic stream returned invalid tool input")?
                };
                action_from_tool_call(&call.name, input)
            })
            .collect::<Result<Vec<_>>>()?;
        let text = if self.text.is_empty() {
            (!actions.is_empty())
                .then(|| action_batch_text(&actions))
                .context("Anthropic stream contained neither text nor a tool call")?
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
    state: &mut AnthropicStreamState,
) -> Result<()> {
    if event.data == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(&event.data).context("invalid Anthropic SSE event")?;
    let kind = event
        .event
        .as_deref()
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or_default();
    match kind {
        "message_start" => {
            state.usage.input_tokens = at(&value, "/message/usage/input_tokens");
            state.usage.cache_read_tokens = at(&value, "/message/usage/cache_read_input_tokens");
            state.usage.cache_write_tokens =
                at(&value, "/message/usage/cache_creation_input_tokens");
        }
        "content_block_start" => {
            if let Some(block) = value.get("content_block") {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            state.text.push_str(text);
                            emit_delta(stream, ModelStreamEvent::TextDelta(text.to_owned()))?;
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                            emit_delta(stream, ModelStreamEvent::ReasoningDelta(text.to_owned()))?;
                        }
                    }
                    Some("tool_use") => {
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let call = state.tool_calls.entry(index).or_default();
                        call.name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if let Some(input) = block.get("input").filter(|input| {
                            input.as_object().is_some_and(|object| !object.is_empty())
                        }) {
                            call.input = serde_json::to_string(input)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = value.get("delta") {
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            state.text.push_str(text);
                            emit_delta(stream, ModelStreamEvent::TextDelta(text.to_owned()))?;
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            emit_delta(stream, ModelStreamEvent::ReasoningDelta(text.to_owned()))?;
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = delta.get("partial_json").and_then(Value::as_str) {
                            let index =
                                value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            state
                                .tool_calls
                                .entry(index)
                                .or_default()
                                .input
                                .push_str(json);
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            state.usage.output_tokens = at(&value, "/usage/output_tokens");
        }
        _ => {}
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
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
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
        .collect::<Result<Vec<_>>>()?;
    let mut text = value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.is_empty() && !actions.is_empty() {
        text = action_batch_text(&actions);
    }
    if text.is_empty() {
        anyhow::bail!("Anthropic response contained neither text nor a supported tool call");
    }
    let action = (!actions.is_empty()).then(|| actions.remove(0));
    Ok(ModelResponse {
        text,
        action,
        additional_actions: actions,
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

    #[test]
    fn preserves_multiple_anthropic_tool_uses() {
        let mut response = parse_response(json!({
            "content": [
                {
                    "type": "tool_use",
                    "id": "tool_1",
                    "name": "read_file",
                    "input": {"path": "Cargo.toml"}
                },
                {
                    "type": "tool_use",
                    "id": "tool_2",
                    "name": "glob",
                    "input": {"pattern": "**/*.rs"}
                }
            ],
            "usage": {}
        }))
        .unwrap();

        assert!(matches!(
            response.take_actions().as_slice(),
            [
                crate::protocol::Action::ReadFile { .. },
                crate::protocol::Action::Glob { .. }
            ]
        ));
    }

    #[test]
    fn assembles_text_and_tool_json_stream_events() {
        let stream = RecordingStream::default();
        let mut state = AnthropicStreamState::default();
        for (event, data) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":9,"cache_read_input_tokens":5}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","content_block":{"type":"tool_use","name":"finish","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"summary\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"\"done\"}"}}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","usage":{"output_tokens":2}}"#,
            ),
        ] {
            ingest_stream_event(
                SseEvent {
                    event: Some(event.into()),
                    data: data.into(),
                },
                &stream,
                &mut state,
            )
            .unwrap();
        }
        let response = state.finish().unwrap();

        assert_eq!(
            response.action,
            Some(crate::protocol::Action::Finish {
                summary: "done".into(),
            })
        );
        assert_eq!(response.usage.input_tokens, 9);
        assert_eq!(response.usage.output_tokens, 2);
    }

    #[test]
    fn emits_anthropic_text_and_thinking_deltas() {
        let stream = RecordingStream::default();
        let mut state = AnthropicStreamState::default();
        for data in [
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"plan "}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"done"}}"#,
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

        assert_eq!(state.finish().unwrap().text, "done");
        assert_eq!(
            *stream.0.lock().unwrap(),
            [
                ModelStreamEvent::ReasoningDelta("plan ".into()),
                ModelStreamEvent::TextDelta("done".into()),
            ]
        );
    }
}
