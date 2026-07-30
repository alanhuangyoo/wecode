use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
    pub name: String,
}

impl ImageAttachment {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCallMessage {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolResultMessage {
    pub call_id: String,
    pub name: String,
    pub is_error: bool,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_result: None,
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images,
            tool_calls: Vec::new(),
            tool_result: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_result: None,
        }
    }

    pub fn assistant_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCallMessage>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_calls,
            tool_result: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_result: Some(ToolResultMessage {
                call_id: call_id.into(),
                name: name.into(),
                is_error,
            }),
        }
    }

    pub fn is_plain(&self) -> bool {
        self.tool_calls.is_empty() && self.tool_result.is_none()
    }
}

pub fn repair_dangling_tool_calls(messages: &mut Vec<Message>) -> usize {
    let mut repaired = 0;
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role != Role::Assistant || messages[index].tool_calls.is_empty() {
            index += 1;
            continue;
        }

        let calls = messages[index].tool_calls.clone();
        let mut result_end = index + 1;
        let mut answered = std::collections::HashSet::new();
        while result_end < messages.len() {
            let Some(result) = messages[result_end].tool_result.as_ref() else {
                break;
            };
            answered.insert(result.call_id.clone());
            result_end += 1;
        }

        let missing = calls
            .into_iter()
            .filter(|call| !answered.contains(&call.id))
            .map(|call| {
                Message::tool_result(
                    call.id,
                    call.name,
                    "Tool execution was interrupted before a result was recorded. Re-evaluate the \
                     current state before deciding whether to retry.",
                    true,
                )
            })
            .collect::<Vec<_>>();
        let missing_count = missing.len();
        repaired += missing_count;
        if !missing.is_empty() {
            messages.splice(result_end..result_end, missing);
        }
        index = result_end + missing_count;
    }
    repaired
}

#[derive(Clone, Debug)]
pub struct ContextWindow {
    max_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub total_tokens: u64,
    pub text_tokens: u64,
    pub image_tokens: u64,
    pub messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub images: usize,
}

impl ContextUsage {
    pub fn percent_of(self, max_tokens: u64) -> u64 {
        if max_tokens == 0 {
            return 0;
        }
        self.total_tokens
            .saturating_mul(100)
            .saturating_add(max_tokens / 2)
            .saturating_div(max_tokens)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionReport {
    pub before: ContextUsage,
    pub after: ContextUsage,
    pub removed_messages: usize,
}

impl ContextWindow {
    pub fn new(max_tokens: u64, _keep_messages: usize) -> Self {
        Self { max_tokens }
    }

    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    pub fn usage(&self, messages: &[Message]) -> ContextUsage {
        context_usage(messages)
    }
}

pub fn estimate_text_tokens(value: &str) -> u64 {
    xai_token_estimation::estimate_tokens(value)
}

pub fn context_usage(messages: &[Message]) -> ContextUsage {
    let mut usage = ContextUsage {
        messages: messages.len(),
        ..Default::default()
    };
    for message in messages {
        match message.role {
            Role::User => usage.user_messages = usage.user_messages.saturating_add(1),
            Role::Assistant => {
                usage.assistant_messages = usage.assistant_messages.saturating_add(1)
            }
        }
        usage.text_tokens = usage
            .text_tokens
            .saturating_add(estimate_text_tokens(&message.content));
        for call in &message.tool_calls {
            usage.text_tokens = usage
                .text_tokens
                .saturating_add(estimate_text_tokens(&call.name))
                .saturating_add(estimate_text_tokens(&call.arguments.to_string()));
        }
        usage.images = usage.images.saturating_add(message.images.len());
        usage.image_tokens = usage.image_tokens.saturating_add(
            message
                .images
                .iter()
                .map(estimate_image_tokens)
                .fold(0_u64, u64::saturating_add),
        );
    }
    usage.total_tokens = usage.text_tokens.saturating_add(usage.image_tokens);
    usage
}

fn estimate_image_tokens(image: &ImageAttachment) -> u64 {
    u64::try_from(image.data.len().saturating_div(4))
        .unwrap_or(u64::MAX)
        .clamp(256, 4_096)
}

pub(crate) fn tool_exchange_start(messages: &[Message], mut keep_from: usize) -> usize {
    while let Some(result) = messages
        .get(keep_from)
        .and_then(|message| message.tool_result.as_ref())
    {
        let Some(call_index) = messages[..keep_from].iter().rposition(|message| {
            message
                .tool_calls
                .iter()
                .any(|call| call.id == result.call_id)
        }) else {
            break;
        };
        keep_from = call_index;
    }
    keep_from
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_usage_separates_text_and_images() {
        let messages = vec![
            Message::user("hello"),
            Message::user_with_images(
                "inspect",
                vec![ImageAttachment {
                    media_type: "image/png".into(),
                    data: "a".repeat(8_000),
                    name: "screen.png".into(),
                }],
            ),
            Message::assistant("done"),
        ];
        let usage = ContextWindow::new(10_000, 4).usage(&messages);

        assert_eq!(usage.messages, 3);
        assert_eq!(usage.user_messages, 2);
        assert_eq!(usage.assistant_messages, 1);
        assert_eq!(usage.images, 1);
        assert_eq!(usage.image_tokens, 2_000);
        assert_eq!(usage.total_tokens, usage.text_tokens + usage.image_tokens);
        assert_eq!(usage.percent_of(usage.total_tokens.saturating_mul(2)), 50);
    }

    #[test]
    fn old_message_json_deserializes_and_empty_images_do_not_change_shape() {
        let message: Message =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert!(message.images.is_empty());
        assert_eq!(
            serde_json::to_value(message).unwrap(),
            serde_json::json!({"role": "user", "content": "hello"})
        );
    }

    #[test]
    fn repairs_dangling_tool_calls_before_later_user_messages() {
        let calls = vec![
            ToolCallMessage {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "README.md"}),
            },
            ToolCallMessage {
                id: "call-2".into(),
                name: "grep".into(),
                arguments: serde_json::json!({"pattern": "TODO"}),
            },
        ];
        let mut messages = vec![
            Message::assistant_tool_calls("", calls),
            Message::tool_result("call-1", "read_file", "contents", false),
            Message::user("new request"),
        ];

        assert_eq!(repair_dangling_tool_calls(&mut messages), 1);
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[2]
                .tool_result
                .as_ref()
                .map(|result| result.call_id.as_str()),
            Some("call-2")
        );
        assert_eq!(messages[3].content, "new request");
    }

    #[test]
    fn leaves_complete_tool_exchanges_unchanged() {
        let mut messages = vec![
            Message::assistant_tool_calls(
                "",
                vec![ToolCallMessage {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "pwd"}),
                }],
            ),
            Message::tool_result("call-1", "shell", "workspace", false),
        ];

        let original = messages.clone();
        assert_eq!(repair_dangling_tool_calls(&mut messages), 0);
        assert_eq!(messages, original);
    }
}
