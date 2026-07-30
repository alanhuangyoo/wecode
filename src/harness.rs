use crate::context::Message;
use crate::model::CompletionRequest;
use crate::prompt_context::PromptContext;

#[derive(Clone, Debug)]
pub struct AgentHarness {
    session_id: String,
    prompt_context: PromptContext,
}

impl AgentHarness {
    pub fn new(session_id: impl Into<String>, prompt_context: PromptContext) -> Self {
        Self {
            session_id: session_id.into(),
            prompt_context,
        }
    }

    pub fn create_turn_state(
        &self,
        messages: &[Message],
        active_tools: Option<&[String]>,
    ) -> TurnState {
        TurnState {
            session_id: self.session_id.clone(),
            prompt_context: self.prompt_context.clone(),
            messages: messages.to_vec(),
            active_tools: active_tools.map(<[String]>::to_vec),
        }
    }

    pub fn prompt_context(&self) -> &PromptContext {
        &self.prompt_context
    }
}

#[derive(Clone, Debug)]
pub struct TurnState {
    pub session_id: String,
    pub prompt_context: PromptContext,
    pub messages: Vec<Message>,
    pub active_tools: Option<Vec<String>>,
}

impl TurnState {
    pub fn completion_request(self) -> CompletionRequest {
        CompletionRequest {
            system: self.prompt_context.render(),
            messages: self.messages,
            session_id: self.session_id,
            enabled_tools: self.active_tools,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;
    use crate::model::ToolProfile;
    use crate::prompt_context::PromptContextOptions;

    #[test]
    fn turn_snapshots_keep_the_prompt_prefix_stable() {
        let temp = tempfile::tempdir().unwrap();
        let model = ModelConfig::default();
        let prompt = PromptContext::build(PromptContextOptions {
            profile: ToolProfile::Interactive,
            model: &model,
            workspace: temp.path(),
            instructions: None,
            skills_prompt: None,
            additional_prompt: None,
        })
        .unwrap();
        let harness = AgentHarness::new("session-1", prompt);
        let active_tools = vec!["read_file".into()];
        let first = harness
            .create_turn_state(&[Message::user("first")], Some(&active_tools))
            .completion_request();
        let second = harness
            .create_turn_state(
                &[Message::user("first"), Message::assistant("second")],
                Some(&active_tools),
            )
            .completion_request();

        assert_eq!(first.system, second.system);
        assert_eq!(first.enabled_tools, second.enabled_tools);
        assert_ne!(first.messages, second.messages);
    }
}
