// Adapted from Pi's harness compaction pipeline.
// Copyright 2025 Mario Zechner and Pi contributors. Licensed under MIT.
// Modified for WeCode's message model and provider-neutral completion interface.

use anyhow::{Result, bail};

use crate::context::{
    CompactionReport, ContextUsage, Message, Role, context_usage, tool_exchange_start,
};
use crate::model::{CompletionRequest, Model, StopReason, Usage};

const SUMMARY_MARKER: &str = "[wecode-context-summary-v2]";

const SUMMARIZATION_SYSTEM_PROMPT: &str = "\
You are a context summarization assistant. Read the supplied conversation and produce only the \
structured checkpoint summary requested by the user message. Do not continue the conversation, \
answer its questions, or call tools.";

const SUMMARIZATION_PROMPT: &str = "\
Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this exact format:

## Goal
[What the user is trying to accomplish.]

## Constraints & Preferences
- [User requirements and preferences, or \"(none)\".]

## Progress
### Done
- [x] [Completed work]

### In Progress
- [ ] [Current work]

### Blocked
- [Current blockers, or \"(none)\".]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered continuation steps]

## Critical Context
- [Exact paths, symbols, commands, errors, data, or references needed to continue.]

Keep each section concise. Preserve exact file paths, function names, commands, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "\
Update the existing structured checkpoint summary with the new conversation messages.

Preserve still-relevant goals, constraints, completed work, decisions, exact paths, symbols, \
commands, and errors. Add new progress and move completed items out of In Progress. Remove only \
information that is demonstrably obsolete.

Use the same exact section format as the existing summary and keep it concise.";

#[derive(Clone, Debug)]
struct Preparation {
    before: ContextUsage,
    original_len: usize,
    first: Message,
    retained_tail: Vec<Message>,
    messages_to_summarize: Vec<Message>,
    previous_summary: Option<String>,
    focus: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CompactionOutcome {
    pub(crate) report: CompactionReport,
    pub(crate) usage: Usage,
}

pub(crate) async fn compact(
    model: &dyn Model,
    messages: &mut Vec<Message>,
    max_tokens: u64,
    keep_messages: usize,
    session_id: &str,
    focus: Option<&str>,
    force: bool,
) -> Result<Option<CompactionOutcome>> {
    let Some(preparation) = prepare(messages, max_tokens, keep_messages, focus, force) else {
        return Ok(None);
    };
    let prompt = summary_prompt(&preparation);
    let response = model
        .complete(
            CompletionRequest {
                system: SUMMARIZATION_SYSTEM_PROMPT.to_owned(),
                messages: vec![Message::user(prompt)],
                session_id: format!("{session_id}:compaction"),
                enabled_tools: Some(Vec::new()),
            },
            None,
        )
        .await?;
    if !response.tool_calls.is_empty() {
        bail!("compaction model returned tool calls instead of a summary");
    }
    if matches!(
        response.stop_reason,
        StopReason::MaxTokens | StopReason::Refusal | StopReason::ContentFilter | StopReason::Error
    ) {
        bail!(
            "compaction model did not complete normally: {:?}",
            response.stop_reason
        );
    }
    let summary = response.text.trim();
    if summary.is_empty() {
        bail!("compaction model returned an empty summary");
    }

    let mut compacted = Vec::with_capacity(preparation.retained_tail.len() + 2);
    compacted.push(preparation.first);
    compacted.push(Message::user(format!(
        "{SUMMARY_MARKER}\n\
         This checkpoint was generated from older conversation history. Treat it as context, not \
         as a new user request.\n\n{summary}"
    )));
    compacted.extend(preparation.retained_tail);
    let after = context_usage(&compacted);
    let removed_messages = preparation.original_len.saturating_sub(compacted.len());
    *messages = compacted;
    Ok(Some(CompactionOutcome {
        report: CompactionReport {
            before: preparation.before,
            after,
            removed_messages,
        },
        usage: response.usage,
    }))
}

fn prepare(
    messages: &[Message],
    max_tokens: u64,
    keep_messages: usize,
    focus: Option<&str>,
    force: bool,
) -> Option<Preparation> {
    let before = context_usage(messages);
    if !force && before.total_tokens <= max_tokens {
        return None;
    }
    let keep_messages = keep_messages.max(4);
    if messages.len() <= keep_messages + 1 {
        return None;
    }
    let mut keep_from = messages.len().saturating_sub(keep_messages);
    keep_from = tool_exchange_start(messages, keep_from);
    if keep_from <= 1 {
        return None;
    }

    let previous_summary = messages.get(1).and_then(previous_summary);
    let summarize_from = if previous_summary.is_some() { 2 } else { 1 };
    if keep_from <= summarize_from {
        return None;
    }
    Some(Preparation {
        before,
        original_len: messages.len(),
        first: messages[0].clone(),
        retained_tail: messages[keep_from..].to_vec(),
        messages_to_summarize: messages[summarize_from..keep_from].to_vec(),
        previous_summary,
        focus: focus
            .map(str::trim)
            .filter(|focus| !focus.is_empty())
            .map(str::to_owned),
    })
}

fn previous_summary(message: &Message) -> Option<String> {
    (message.role == Role::User
        && (message.content.starts_with(SUMMARY_MARKER)
            || message.content.starts_with("[wecode-context-summary-v1]")))
    .then(|| {
        message
            .content
            .split_once('\n')
            .map(|(_, summary)| summary.trim().to_owned())
            .unwrap_or_default()
    })
}

fn summary_prompt(preparation: &Preparation) -> String {
    let mut prompt = String::from("<conversation>\n");
    serialize_conversation(&preparation.messages_to_summarize, &mut prompt);
    prompt.push_str("</conversation>\n\n");
    if let Some(previous) = &preparation.previous_summary {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</previous-summary>\n\n");
        prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        prompt.push_str(SUMMARIZATION_PROMPT);
    }
    if let Some(focus) = &preparation.focus {
        prompt.push_str("\n\nAdditional focus requested by the user: ");
        prompt.push_str(focus);
    }
    prompt
}

fn serialize_conversation(messages: &[Message], output: &mut String) {
    for message in messages {
        let role = match message.role {
            Role::User if message.tool_result.is_some() => "tool-result",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        output.push('<');
        output.push_str(role);
        output.push_str(">\n");
        if !message.content.is_empty() {
            output.push_str(&message.content);
            output.push('\n');
        }
        for image in &message.images {
            output.push_str(&format!(
                "[attached image: name={}, media_type={}]\n",
                image.name, image.media_type
            ));
        }
        for call in &message.tool_calls {
            output.push_str(&format!(
                "[tool call: id={}, name={}, arguments={}]\n",
                call.id, call.name, call.arguments
            ));
        }
        if let Some(result) = &message.tool_result {
            output.push_str(&format!(
                "[tool result metadata: call_id={}, name={}, is_error={}]\n",
                result.call_id, result.name, result.is_error
            ));
        }
        output.push_str("</");
        output.push_str(role);
        output.push_str(">\n");
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;

    use super::{prepare, previous_summary, serialize_conversation};
    use crate::context::{Message, ToolCallMessage};
    use crate::model::{CompletionRequest, Model, ModelResponse, ModelStream, StopReason, Usage};

    struct SummaryModel;

    #[async_trait]
    impl Model for SummaryModel {
        async fn complete(
            &self,
            request: CompletionRequest,
            _stream: Option<&dyn ModelStream>,
        ) -> Result<ModelResponse> {
            assert!(request.enabled_tools.as_ref().is_some_and(Vec::is_empty));
            assert!(request.messages[0].content.contains("<conversation>"));
            Ok(ModelResponse {
                text: "## Goal\nContinue the task\n\n## Next Steps\n1. Finish it".into(),
                tool_calls: Vec::new(),
                action: None,
                additional_actions: Vec::new(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Usage::default()
                },
                cache_hit: false,
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    #[test]
    fn preparation_preserves_the_task_tail_and_tool_exchange() {
        let mut messages = vec![Message::user("task")];
        for index in 0..8 {
            messages.push(Message::assistant(format!("old {index}")));
        }
        messages.push(Message::assistant_tool_calls(
            "",
            vec![ToolCallMessage {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            }],
        ));
        messages.push(Message::tool_result(
            "call-1",
            "read_file",
            "contents",
            false,
        ));
        messages.push(Message::assistant("done"));

        let preparation = prepare(&messages, 1, 2, None, false).unwrap();
        assert_eq!(preparation.first.content, "task");
        let call = preparation
            .retained_tail
            .iter()
            .position(|message| !message.tool_calls.is_empty())
            .expect("tool call retained");
        assert!(preparation.retained_tail[call + 1].tool_result.is_some());
    }

    #[test]
    fn iterative_compaction_extracts_the_previous_summary() {
        let message = Message::user(
            "[wecode-context-summary-v2]\ncheckpoint metadata\n\n## Goal\nKeep working",
        );
        assert!(previous_summary(&message).unwrap().contains("## Goal"));
    }

    #[test]
    fn serialization_keeps_tool_and_image_metadata_without_image_payloads() {
        let mut message = Message::user("inspect");
        message.images.push(crate::context::ImageAttachment {
            media_type: "image/png".into(),
            data: "secret-binary-data".into(),
            name: "failure.png".into(),
        });
        let mut output = String::new();
        serialize_conversation(&[message], &mut output);
        assert!(output.contains("failure.png"));
        assert!(output.contains("image/png"));
        assert!(!output.contains("secret-binary-data"));
    }

    #[tokio::test]
    async fn model_summary_replaces_old_history_and_reports_usage() {
        let mut messages = vec![Message::user("task")];
        for index in 0..12 {
            messages.push(Message::assistant(format!("analysis {index}")));
            messages.push(Message::user(format!("result {index}")));
        }
        let original_len = messages.len();
        let outcome = super::compact(
            &SummaryModel,
            &mut messages,
            1,
            4,
            "session",
            Some("preserve the parser API"),
            false,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(messages[0].content, "task");
        assert!(
            messages[1]
                .content
                .starts_with("[wecode-context-summary-v2]")
        );
        assert!(messages[1].content.contains("Continue the task"));
        assert!(messages.len() < original_len);
        assert_eq!(outcome.usage.input_tokens, 100);
        assert!(outcome.report.removed_messages > 0);
    }
}
