use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, ModelStream, ModelStreamEvent, RawModel, ToolProfile, Usage,
    action_batch_text, action_from_tool_call, tool_definitions,
};
use crate::config::{ModelConfig, PromptCacheMode, WireApi};
use crate::context::Role;
use crate::model::http::{RetryPolicy, SseEvent, send_json, send_sse};

pub struct OpenAiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    tool_profile: ToolProfile,
}

impl OpenAiModel {
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
                tool_definitions(self.tool_profile)
                    .into_iter()
                    .map(|definition| json!({"type": "function", "function": definition}))
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(true);
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
                tool_definitions(self.tool_profile)
                    .into_iter()
                    .map(|mut definition| {
                        definition["type"] = json!("function");
                        definition
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(true);
        }
        body
    }
}

#[async_trait]
impl RawModel for OpenAiModel {
    async fn complete_raw(
        &self,
        request: &CompletionRequest,
        stream: Option<&dyn ModelStream>,
    ) -> Result<ModelResponse> {
        if self.config.streaming
            && let Some(stream) = stream
        {
            return match self.config.wire_api {
                WireApi::ChatCompletions => self.complete_chat_stream(request, stream).await,
                WireApi::Responses => self.complete_responses_stream(request, stream).await,
            };
        }
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
            RetryPolicy::from_config(&self.config),
        )
        .await?;
        match self.config.wire_api {
            WireApi::ChatCompletions => parse_chat_response(value),
            WireApi::Responses => parse_responses_response(value),
        }
    }
}

impl OpenAiModel {
    async fn complete_chat_stream(
        &self,
        request: &CompletionRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = self.chat_body(request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
        let mut state = OpenAiStreamState::default();
        send_sse(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &body,
            RetryPolicy::from_config(&self.config),
            |event| ingest_chat_event(event, stream, &mut state),
        )
        .await?;
        state.finish("OpenAI-compatible stream")
    }

    async fn complete_responses_stream(
        &self,
        request: &CompletionRequest,
        stream: &dyn ModelStream,
    ) -> Result<ModelResponse> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let mut body = self.responses_body(request);
        body["stream"] = json!(true);
        let mut state = OpenAiStreamState::default();
        let mut completed = None;
        send_sse(
            &self.client,
            reqwest::Method::POST,
            &url,
            self.headers()?,
            &body,
            RetryPolicy::from_config(&self.config),
            |event| ingest_responses_event(event, stream, &mut state, &mut completed),
        )
        .await?;
        if let Some(response) = completed {
            return parse_responses_response(response);
        }
        state.finish("OpenAI Responses stream")
    }
}

#[derive(Default)]
struct OpenAiStreamState {
    text: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    usage: Usage,
}

#[derive(Default)]
struct PartialToolCall {
    name: String,
    arguments: String,
}

impl OpenAiStreamState {
    fn finish(self, source: &str) -> Result<ModelResponse> {
        let mut actions = self
            .tool_calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| {
                let arguments = serde_json::from_str(&call.arguments)
                    .with_context(|| format!("{source} returned invalid tool arguments"))?;
                action_from_tool_call(&call.name, arguments)
            })
            .collect::<Result<Vec<_>>>()?;
        let text = if self.text.is_empty() {
            (!actions.is_empty())
                .then(|| action_batch_text(&actions))
                .with_context(|| format!("{source} contained neither text nor a tool call"))?
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

fn ingest_chat_event(
    event: SseEvent,
    stream: &dyn ModelStream,
    state: &mut OpenAiStreamState,
) -> Result<()> {
    if event.data == "[DONE]" {
        return Ok(());
    }
    let value: Value =
        serde_json::from_str(&event.data).context("invalid OpenAI-compatible SSE event")?;
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        state.usage = Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write_tokens: 0,
        };
    }
    let Some(delta) = value.pointer("/choices/0/delta") else {
        return Ok(());
    };
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        state.text.push_str(text);
        emit_nonempty(stream, ModelStreamEvent::TextDelta(text.to_owned()))?;
    }
    for key in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(text) = delta.get(key).and_then(Value::as_str) {
            emit_nonempty(stream, ModelStreamEvent::ReasoningDelta(text.to_owned()))?;
        }
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(function) = tool_call.get("function") else {
                continue;
            };
            let call = state.tool_calls.entry(index).or_default();
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                call.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
            }
        }
    }
    Ok(())
}

fn ingest_responses_event(
    event: SseEvent,
    stream: &dyn ModelStream,
    state: &mut OpenAiStreamState,
    completed: &mut Option<Value>,
) -> Result<()> {
    if event.data == "[DONE]" {
        return Ok(());
    }
    let value: Value =
        serde_json::from_str(&event.data).context("invalid OpenAI Responses event")?;
    let kind = event
        .event
        .as_deref()
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                state.text.push_str(delta);
                emit_nonempty(stream, ModelStreamEvent::TextDelta(delta.to_owned()))?;
            }
        }
        "response.reasoning_text.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_summary_part.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                emit_nonempty(stream, ModelStreamEvent::ReasoningDelta(delta.to_owned()))?;
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                state
                    .tool_calls
                    .entry(index)
                    .or_default()
                    .arguments
                    .push_str(delta);
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = value.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let call = state.tool_calls.entry(index).or_default();
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    call.name = name.to_owned();
                }
                if kind.ends_with(".done")
                    && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                {
                    call.arguments = arguments.to_owned();
                }
            }
        }
        "response.completed" => {
            *completed = value.get("response").cloned();
        }
        _ => {}
    }
    Ok(())
}

fn emit_nonempty(stream: &dyn ModelStream, event: ModelStreamEvent) -> Result<()> {
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

fn parse_chat_response(value: Value) -> Result<ModelResponse> {
    let mut actions = value
        .pointer("/choices/0/message/tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| call.get("function"))
        .filter_map(parse_openai_tool_call)
        .collect::<Result<Vec<_>>>()?;
    let mut text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty() && !actions.is_empty() {
        text = action_batch_text(&actions);
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
    let action = (!actions.is_empty()).then(|| actions.remove(0));
    Ok(ModelResponse {
        text,
        action,
        additional_actions: actions,
        usage,
        cache_hit: false,
    })
}

fn parse_responses_response(value: Value) -> Result<ModelResponse> {
    let mut actions = Vec::new();
    let mut text = value
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty()
        && let Some(output) = value.get("output").and_then(Value::as_array)
    {
        for item in output {
            if item.get("type").and_then(Value::as_str) == Some("function_call")
                && let Some(action) = parse_openai_tool_call(item)
            {
                actions.push(action?);
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
    if text.is_empty() && !actions.is_empty() {
        text = action_batch_text(&actions);
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
    let action = (!actions.is_empty()).then(|| actions.remove(0));
    Ok(ModelResponse {
        text,
        action,
        additional_actions: actions,
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

    #[test]
    fn preserves_multiple_native_tool_calls_in_provider_order() {
        let mut response = parse_chat_response(json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [
                    {
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/lib.rs\"}"
                        }
                    },
                    {
                        "type": "function",
                        "function": {
                            "name": "grep",
                            "arguments": "{\"pattern\":\"pub mod\",\"path\":\"src\"}"
                        }
                    }
                ]
            }}],
            "usage": {}
        }))
        .unwrap();

        let actions = response.take_actions();
        assert!(matches!(
            actions.as_slice(),
            [
                crate::protocol::Action::ReadFile { .. },
                crate::protocol::Action::Grep { .. }
            ]
        ));
        assert!(response.text.starts_with("[{\"action\":\"read_file\""));
    }

    #[test]
    fn assembles_chat_stream_tool_fragments_and_usage() {
        let stream = RecordingStream::default();
        let mut state = OpenAiStreamState::default();
        for data in [
            r#"{"choices":[{"delta":{"reasoning_content":"checking "}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"finish","arguments":"{\"summary\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"done\"}"}}]}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":8}}}"#,
        ] {
            ingest_chat_event(
                SseEvent {
                    event: None,
                    data: data.into(),
                },
                &stream,
                &mut state,
            )
            .unwrap();
        }
        let response = state.finish("test").unwrap();

        assert_eq!(
            response.action,
            Some(crate::protocol::Action::Finish {
                summary: "done".into(),
            })
        );
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.cache_read_tokens, 8);
        assert_eq!(
            *stream.0.lock().unwrap(),
            [ModelStreamEvent::ReasoningDelta("checking ".into())]
        );
    }

    #[test]
    fn consumes_responses_deltas_and_completed_response() {
        let stream = RecordingStream::default();
        let mut state = OpenAiStreamState::default();
        let mut completed = None;
        for (event, data) in [
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"do"}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"ne"}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"output_text":"done","usage":{"input_tokens":4,"output_tokens":1}}}"#,
            ),
        ] {
            ingest_responses_event(
                SseEvent {
                    event: Some(event.into()),
                    data: data.into(),
                },
                &stream,
                &mut state,
                &mut completed,
            )
            .unwrap();
        }

        let response = parse_responses_response(completed.unwrap()).unwrap();
        assert_eq!(response.text, "done");
        assert_eq!(response.usage.output_tokens, 1);
        assert_eq!(
            *stream.0.lock().unwrap(),
            [
                ModelStreamEvent::TextDelta("do".into()),
                ModelStreamEvent::TextDelta("ne".into()),
            ]
        );
    }
}
