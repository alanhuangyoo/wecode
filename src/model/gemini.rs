use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, ModelStream, ModelStreamEvent, RawModel, StopReason,
    ToolProfile, Usage, action_batch_text, merge_adjacent_messages, model_tool_call,
    request_tool_definitions,
};
use crate::config::ModelConfig;
use crate::context::Role;
use crate::model::http::{RetryPolicy, SseEvent, send_json, send_sse};

pub struct GeminiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    tool_profile: ToolProfile,
    extra_tools: Vec<Value>,
}

impl GeminiModel {
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

    fn body(&self, request: &CompletionRequest) -> Value {
        let contents: Vec<Value> = merge_adjacent_messages(&request.messages)
            .iter()
            .map(gemini_content)
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
                "functionDeclarations": request_tool_definitions(request, self.tool_profile, &self.extra_tools)
            }]);
            body["toolConfig"] = json!({
                "functionCallingConfig": {"mode": "AUTO"}
            });
        }
        body
    }
}

fn gemini_content(message: &crate::context::Message) -> Value {
    if let Some(result) = &message.tool_result {
        return json!({
            "role": "user",
            "parts": [{
                "functionResponse": {
                    "name": result.name,
                    "response": {
                        "output": message.content,
                        "is_error": result.is_error,
                    }
                }
            }]
        });
    }
    let mut parts = Vec::new();
    if !message.content.is_empty() || message.tool_calls.is_empty() {
        parts.push(json!({"text": message.content}));
    }
    parts.extend(message.tool_calls.iter().map(|call| {
        json!({
            "functionCall": {
                "name": call.name,
                "args": call.arguments,
            }
        })
    }));
    parts.extend(message.images.iter().map(|image| {
        json!({
            "inlineData": {
                "mimeType": image.media_type,
                "data": image.data,
            }
        })
    }));
    json!({
        "role": match message.role {
            Role::User => "user",
            Role::Assistant => "model",
        },
        "parts": parts,
    })
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
    stop_reason: StopReason,
}

impl GeminiStreamState {
    fn finish(self) -> Result<ModelResponse> {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                model_tool_call(format!("wecode-call-{index}"), name, arguments)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut actions = tool_calls
            .iter()
            .map(|call| call.action.clone())
            .collect::<Vec<_>>();
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
            tool_calls,
            action,
            additional_actions: actions,
            usage: self.usage,
            cache_hit: false,
            stop_reason: self.stop_reason,
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
    if let Some(reason) = value
        .pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
    {
        state.stop_reason = gemini_stop_reason(reason);
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
    let tool_calls = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("functionCall"))
        .enumerate()
        .map(|(index, call)| {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .context("Gemini functionCall did not contain a name")?;
            model_tool_call(
                format!("wecode-call-{index}"),
                name,
                call.get("args").cloned().unwrap_or_else(|| json!({})),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut actions = tool_calls
        .iter()
        .map(|call| call.action.clone())
        .collect::<Vec<_>>();
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
        tool_calls,
        action,
        additional_actions: actions,
        usage: Usage {
            input_tokens: at(&value, "/usageMetadata/promptTokenCount"),
            output_tokens: at(&value, "/usageMetadata/candidatesTokenCount"),
            cache_read_tokens: at(&value, "/usageMetadata/cachedContentTokenCount"),
            cache_write_tokens: 0,
        },
        cache_hit: false,
        stop_reason: value
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .map(gemini_stop_reason)
            .unwrap_or(StopReason::Unknown),
    })
}

fn gemini_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => StopReason::ContentFilter,
        "RECITATION" => StopReason::Refusal,
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "OTHER" => StopReason::Error,
        _ => StopReason::Unknown,
    }
}

fn at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::context::{ImageAttachment, Message, ToolCallMessage};

    #[derive(Default)]
    struct RecordingStream(Mutex<Vec<ModelStreamEvent>>);

    impl ModelStream for RecordingStream {
        fn emit(&self, event: ModelStreamEvent) -> Result<()> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[test]
    fn serializes_gemini_inline_images_and_preserves_text_only_parts() {
        let config = ModelConfig {
            native_tools: false,
            ..Default::default()
        };
        let model = GeminiModel::new(
            config,
            None,
            reqwest::Client::new(),
            ToolProfile::Coding,
            Vec::new(),
        );
        let plain = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("inspect")],
            session_id: "session".into(),
            enabled_tools: None,
        };
        assert_eq!(
            model.body(&plain)["contents"][0]["parts"],
            json!([{"text": "inspect"}])
        );

        let image = ImageAttachment {
            media_type: "image/webp".into(),
            data: "YWJj".into(),
            name: "screen.webp".into(),
        };
        let request = CompletionRequest {
            messages: vec![Message::user_with_images("inspect", vec![image])],
            ..plain
        };
        assert_eq!(
            model.body(&request)["contents"][0]["parts"],
            json!([
                {"text": "inspect"},
                {"inlineData": {"mimeType": "image/webp", "data": "YWJj"}}
            ])
        );
    }

    #[test]
    fn serializes_gemini_function_response_with_the_original_name() {
        let assistant = Message::assistant_tool_calls(
            "",
            vec![ToolCallMessage {
                id: "wecode-call-0".into(),
                name: "grep".into(),
                arguments: json!({"pattern": "TODO", "path": "."}),
            }],
        );
        let result = Message::tool_result("wecode-call-0", "grep", "no matches", false);

        assert_eq!(
            gemini_content(&assistant)["parts"][0]["functionCall"]["name"],
            "grep"
        );
        assert_eq!(
            gemini_content(&result)["parts"][0]["functionResponse"]["name"],
            "grep"
        );
    }

    #[test]
    fn parses_native_function_call() {
        let response = parse_response(json!({
            "candidates": [{
                "finishReason": "STOP",
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
        assert_eq!(response.stop_reason, StopReason::EndTurn);
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
