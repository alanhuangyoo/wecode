use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContextWindow {
    max_tokens: u64,
    keep_messages: usize,
}

impl ContextWindow {
    pub fn new(max_tokens: u64, keep_messages: usize) -> Self {
        Self {
            max_tokens,
            keep_messages: keep_messages.max(4),
        }
    }

    pub fn compact(&self, messages: &mut Vec<Message>) -> usize {
        let total: u64 = messages
            .iter()
            .map(|message| xai_token_estimation::estimate_tokens(&message.content))
            .sum();
        if total <= self.max_tokens || messages.len() <= self.keep_messages + 1 {
            return 0;
        }

        let keep_from = messages.len().saturating_sub(self.keep_messages);
        if keep_from <= 1 {
            return 0;
        }
        let removed = keep_from - 1;
        let mut summary = String::from(
            "Earlier trajectory was compacted locally. Key action and observation excerpts:\n",
        );
        for message in &messages[1..keep_from] {
            let role = match message.role {
                Role::User => "observation",
                Role::Assistant => "assistant",
            };
            summary.push_str("- ");
            summary.push_str(role);
            summary.push_str(": ");
            summary.push_str(&single_line_excerpt(&message.content, 320));
            summary.push('\n');
            if summary.len() >= 10_000 {
                summary.push_str("- additional earlier messages omitted\n");
                break;
            }
        }

        let mut compacted = Vec::with_capacity(self.keep_messages + 2);
        compacted.push(messages[0].clone());
        compacted.push(Message::user(summary));
        compacted.extend_from_slice(&messages[keep_from..]);
        *messages = compacted;
        removed
    }
}

fn single_line_excerpt(input: &str, max_chars: usize) -> String {
    let mut value = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() > max_chars {
        value = value.chars().take(max_chars).collect();
        value.push_str("...");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_preserves_task_and_tail() {
        let mut messages = vec![Message::user("task")];
        for index in 0..20 {
            messages.push(Message::assistant(format!(
                "action {index} {}",
                "x".repeat(100)
            )));
            messages.push(Message::user(format!("result {index} {}", "y".repeat(100))));
        }
        let removed = ContextWindow::new(250, 6).compact(&mut messages);
        assert!(removed > 0);
        assert_eq!(messages[0].content, "task");
        assert!(messages[1].content.contains("compacted locally"));
        assert!(messages.last().unwrap().content.contains("result 19"));
    }
}
