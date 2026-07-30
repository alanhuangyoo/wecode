use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{
    CompletionRequest, ModelResponse, ModelStream, ModelStreamEvent, RawModel, StopReason,
    ToolProfile, Usage, action_batch_text, model_tool_call, request_tool_definitions,
};
use crate::config::{ModelConfig, PromptCacheMode, WireApi};
use crate::context::Role;
use crate::model::http::{RetryPolicy, SseEvent, send_json, send_sse};

pub struct OpenAiModel {
    config: ModelConfig,
    api_key: Option<String>,
    client: reqwest::Client,
    tool_profile: ToolProfile,
    extra_tools: Vec<Value>,
}

impl OpenAiModel {
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
        messages.extend(request.messages.iter().map(chat_message));
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": self.config.max_output_tokens,
        });
        if let Some(temperature) = self.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(reasoning_effort) = &self.config.reasoning_effort {
            body["reasoning_effort"] = json!(reasoning_effort);
        }
        if self.config.send_prompt_cache_key && self.config.prompt_cache != PromptCacheMode::Off {
            body["prompt_cache_key"] = json!(clamp_cache_key(&request.session_id));
            if self.config.prompt_cache == PromptCacheMode::Long {
                body["prompt_cache_retention"] = json!("24h");
            }
        }
        if self.config.native_tools {
            body["tools"] = Value::Array(
                request_tool_definitions(request, self.tool_profile, &self.extra_tools)
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
        let input: Vec<Value> = request.messages.iter().flat_map(responses_items).collect();
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
        if let Some(reasoning_effort) = &self.config.reasoning_effort {
            body["reasoning"] = json!({
                "effort": reasoning_effort,
                "summary": "auto",
            });
        }
        if self.config.prompt_cache != PromptCacheMode::Off {
            body["prompt_cache_key"] = json!(clamp_cache_key(&request.session_id));
            if self.config.prompt_cache == PromptCacheMode::Long {
                body["prompt_cache_retention"] = json!("24h");
            }
        }
        if self.config.native_tools {
            body["tools"] = Value::Array(
                request_tool_definitions(request, self.tool_profile, &self.extra_tools)
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

fn chat_message(message: &crate::context::Message) -> Value {
    if let Some(result) = &message.tool_result {
        return json!({
            "role": "tool",
            "tool_call_id": result.call_id,
            "content": message.content,
        });
    }
    if !message.tool_calls.is_empty() {
        return json!({
            "role": "assistant",
            "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
            "tool_calls": message.tool_calls.iter().map(|call| json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": serde_json::to_string(&call.arguments)
                        .expect("tool call arguments are serializable"),
                }
            })).collect::<Vec<_>>(),
        });
    }
    json!({
        "role": match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        "content": chat_message_content(message),
    })
}

fn responses_items(message: &crate::context::Message) -> Vec<Value> {
    if let Some(result) = &message.tool_result {
        return vec![json!({
            "type": "function_call_output",
            "call_id": result.call_id,
            "output": message.content,
        })];
    }
    if !message.tool_calls.is_empty() {
        let mut items = Vec::with_capacity(message.tool_calls.len() + 1);
        if !message.content.is_empty() {
            items.push(json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": message.content}],
            }));
        }
        items.extend(message.tool_calls.iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": serde_json::to_string(&call.arguments)
                    .expect("tool call arguments are serializable"),
            })
        }));
        return items;
    }
    vec![json!({
        "role": match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        "content": responses_message_content(message),
    })]
}

fn chat_message_content(message: &crate::context::Message) -> Value {
    if message.images.is_empty() {
        return json!(message.content);
    }
    let mut content = vec![json!({"type": "text", "text": message.content})];
    content.extend(message.images.iter().map(|image| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": image.data_url(),
                "detail": "auto",
            }
        })
    }));
    Value::Array(content)
}

fn responses_message_content(message: &crate::context::Message) -> Value {
    if message.images.is_empty() {
        return json!(message.content);
    }
    let text_type = match message.role {
        Role::User => "input_text",
        Role::Assistant => "output_text",
    };
    let mut content = vec![json!({"type": text_type, "text": message.content})];
    content.extend(message.images.iter().map(|image| {
        json!({
            "type": "input_image",
            "image_url": image.data_url(),
            "detail": "auto",
        })
    }));
    Value::Array(content)
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
    stop_reason: StopReason,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamState {
    fn finish(self, source: &str) -> Result<ModelResponse> {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| {
                let arguments = serde_json::from_str(&call.arguments)
                    .with_context(|| format!("{source} returned invalid tool arguments"))?;
                model_tool_call(
                    if call.id.is_empty() {
                        format!("wecode-call-{index}")
                    } else {
                        call.id
                    },
                    call.name,
                    arguments,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mut actions = tool_calls
            .iter()
            .map(|call| call.action.clone())
            .collect::<Vec<_>>();
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
            tool_calls,
            action,
            additional_actions: actions,
            usage: self.usage,
            cache_hit: false,
            stop_reason: self.stop_reason,
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
    if let Some(reason) = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        state.stop_reason = openai_stop_reason(reason);
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
            if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                call.id = id.to_owned();
            }
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
                if let Some(id) = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                {
                    call.id = id.to_owned();
                }
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
    let tool_calls = value
        .pointer("/choices/0/message/tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, call)| parse_openai_tool_call(call, index))
        .collect::<Result<Vec<_>>>()?;
    let mut actions = tool_calls
        .iter()
        .map(|call| call.action.clone())
        .collect::<Vec<_>>();
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
        tool_calls,
        action,
        additional_actions: actions,
        usage,
        cache_hit: false,
        stop_reason: value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .map(openai_stop_reason)
            .unwrap_or(StopReason::Unknown),
    })
}

fn parse_responses_response(value: Value) -> Result<ModelResponse> {
    let mut tool_calls = Vec::new();
    let mut text = value
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.is_empty()
        && let Some(output) = value.get("output").and_then(Value::as_array)
    {
        for (index, item) in output.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("function_call")
                && let Some(call) = parse_openai_tool_call(item, index)
            {
                tool_calls.push(call?);
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
    let mut actions = tool_calls
        .iter()
        .map(|call| call.action.clone())
        .collect::<Vec<_>>();
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
        tool_calls,
        action,
        additional_actions: actions,
        usage,
        cache_hit: false,
        stop_reason: responses_stop_reason(&value),
    })
}

fn openai_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" | "max_tokens" | "max_output_tokens" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        "refusal" => StopReason::Refusal,
        "error" => StopReason::Error,
        _ => StopReason::Unknown,
    }
}

fn responses_stop_reason(value: &Value) -> StopReason {
    match value.get("status").and_then(Value::as_str) {
        Some("failed" | "cancelled") => StopReason::Error,
        Some("incomplete") => value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .map(openai_stop_reason)
            .filter(|reason| *reason != StopReason::Unknown)
            .unwrap_or(StopReason::Error),
        _ if value
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call")) =>
        {
            StopReason::ToolUse
        }
        _ => StopReason::EndTurn,
    }
}

fn parse_openai_tool_call(call: &Value, index: usize) -> Option<Result<super::ModelToolCall>> {
    let function = call.get("function").unwrap_or(call);
    let name = function.get("name")?.as_str()?;
    let arguments = function.get("arguments")?;
    let arguments = match arguments {
        Value::String(value) => match serde_json::from_str(value) {
            Ok(value) => value,
            Err(error) => return Some(Err(error.into())),
        },
        value => value.clone(),
    };
    let id = call
        .get("call_id")
        .or_else(|| call.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("wecode-call-{index}"));
    Some(model_tool_call(id, name, arguments))
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
    fn maps_reasoning_effort_to_each_openai_wire_protocol() {
        let config = ModelConfig {
            reasoning_effort: Some("high".into()),
            native_tools: false,
            ..Default::default()
        };
        let model = OpenAiModel::new(
            config,
            None,
            reqwest::Client::new(),
            ToolProfile::Interactive,
            Vec::new(),
        );
        let request = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("inspect")],
            session_id: "session".into(),
            enabled_tools: None,
        };

        assert_eq!(model.chat_body(&request)["reasoning_effort"], "high");
        assert_eq!(
            model.responses_body(&request)["reasoning"]["effort"],
            "high"
        );
        assert_eq!(
            model.responses_body(&request)["reasoning"]["summary"],
            "auto"
        );
    }

    #[test]
    fn serializes_images_for_chat_and_responses_without_changing_text_only_shape() {
        let text = Message::user("inspect");
        assert_eq!(chat_message_content(&text), json!("inspect"));
        assert_eq!(responses_message_content(&text), json!("inspect"));

        let image = ImageAttachment {
            media_type: "image/png".into(),
            data: "YWJj".into(),
            name: "screen.png".into(),
        };
        let message = Message::user_with_images("inspect", vec![image]);
        assert_eq!(
            chat_message_content(&message),
            json!([
                {"type": "text", "text": "inspect"},
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,YWJj",
                        "detail": "auto"
                    }
                }
            ])
        );
        assert_eq!(
            responses_message_content(&message),
            json!([
                {"type": "input_text", "text": "inspect"},
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,YWJj",
                    "detail": "auto"
                }
            ])
        );
    }

    #[test]
    fn serializes_native_tool_results_with_the_original_openai_call_id() {
        let call = ToolCallMessage {
            id: "call_abc".into(),
            name: "read_file".into(),
            arguments: json!({"path": "src/lib.rs"}),
        };
        let assistant = Message::assistant_tool_calls("", vec![call]);
        let result = Message::tool_result("call_abc", "read_file", "file contents", false);

        assert_eq!(chat_message(&assistant)["tool_calls"][0]["id"], "call_abc");
        assert_eq!(chat_message(&result)["tool_call_id"], "call_abc");
        assert_eq!(responses_items(&assistant)[0]["call_id"], "call_abc");
        assert_eq!(responses_items(&result)[0]["call_id"], "call_abc");
        assert_eq!(responses_items(&result)[0]["type"], "function_call_output");
    }

    #[test]
    fn parses_both_openai_response_shapes() {
        let chat = parse_chat_response(json!({
            "choices": [{
                "message": {"content": "{\"action\":\"finish\",\"summary\":\"ok\"}"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 8}}
        }))
        .unwrap();
        assert_eq!(chat.usage.cache_read_tokens, 8);
        assert_eq!(chat.stop_reason, StopReason::EndTurn);

        let responses = parse_responses_response(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"content": [{"type": "output_text", "text": "done"}]}],
            "usage": {"input_tokens": 4, "output_tokens": 1}
        }))
        .unwrap();
        assert_eq!(responses.text, "done");
        assert_eq!(responses.stop_reason, StopReason::MaxTokens);
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
