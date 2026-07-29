use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanItem {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    #[serde(alias = "read")]
    ReadFile {
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(alias = "list_dir", alias = "ls")]
    ListFiles {
        #[serde(default = "default_path")]
        path: String,
        #[serde(default)]
        depth: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(alias = "find")]
    Glob {
        pattern: String,
        #[serde(default = "default_path")]
        path: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(alias = "search")]
    Grep {
        pattern: String,
        #[serde(default = "default_path")]
        path: String,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        literal: bool,
        #[serde(default)]
        ignore_case: bool,
        #[serde(default)]
        context: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Shell {
        command: String,
        #[serde(default)]
        description: String,
    },
    Patch {
        patch: String,
        #[serde(default)]
        description: String,
    },
    UpdatePlan {
        #[serde(default)]
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    RequestUserInput {
        questions: Vec<UserQuestion>,
    },
    McpCall {
        server: String,
        tool: String,
        #[serde(default)]
        arguments: serde_json::Value,
    },
    LoadSkill {
        name: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    StartProcess {
        command: String,
        #[serde(default)]
        description: String,
    },
    ProcessStatus {
        #[serde(default)]
        process_id: Option<u64>,
        #[serde(default)]
        cursor: Option<u64>,
    },
    WriteProcess {
        process_id: u64,
        input: String,
        #[serde(default = "default_true")]
        newline: bool,
    },
    StopProcess {
        process_id: u64,
    },
    Finish {
        summary: String,
    },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ReadFile { .. } => "read_file",
            Self::ListFiles { .. } => "list_files",
            Self::Glob { .. } => "glob",
            Self::Grep { .. } => "grep",
            Self::Shell { .. } => "shell",
            Self::Patch { .. } => "patch",
            Self::UpdatePlan { .. } => "update_plan",
            Self::RequestUserInput { .. } => "request_user_input",
            Self::McpCall { .. } => "mcp_call",
            Self::LoadSkill { .. } => "load_skill",
            Self::StartProcess { .. } => "start_process",
            Self::ProcessStatus { .. } => "process_status",
            Self::WriteProcess { .. } => "write_process",
            Self::StopProcess { .. } => "stop_process",
            Self::Finish { .. } => "finish",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::ReadFile { path, .. } | Self::ListFiles { path, .. } => path,
            Self::Glob { pattern, .. } | Self::Grep { pattern, .. } => pattern,
            Self::Shell {
                command,
                description,
            } => {
                if description.is_empty() {
                    command
                } else {
                    description
                }
            }
            Self::Patch { description, .. } => {
                if description.is_empty() {
                    "apply patch"
                } else {
                    description
                }
            }
            Self::UpdatePlan { plan, .. } => {
                if plan.len() == 1 {
                    "1 plan step"
                } else {
                    "plan updated"
                }
            }
            Self::RequestUserInput { questions } => {
                if questions.len() == 1 {
                    "1 question"
                } else {
                    "questions for user"
                }
            }
            Self::McpCall { tool, .. } => tool,
            Self::LoadSkill { name, path, .. } => path.as_deref().unwrap_or(name),
            Self::StartProcess {
                command,
                description,
            } => {
                if description.is_empty() {
                    command
                } else {
                    description
                }
            }
            Self::ProcessStatus { .. } => "inspect background processes",
            Self::WriteProcess { .. } => "write background process stdin",
            Self::StopProcess { .. } => "stop background process",
            Self::Finish { summary } => summary,
        }
    }
}

pub fn parse_action(response: &str) -> Result<Action> {
    let trimmed = response.trim();
    let candidate = strip_code_fence(trimmed)
        .or_else(|| object_slice(trimmed))
        .unwrap_or(trimmed);
    let action: Action = serde_json::from_str(candidate)
        .with_context(|| format!("model response was not a valid action: {candidate}"))?;
    validate_action(&action)?;
    Ok(action)
}

pub(crate) fn validate_action(action: &Action) -> Result<()> {
    match action {
        Action::ReadFile { path, .. } if path.trim().is_empty() => {
            bail!("read_file path cannot be empty")
        }
        Action::Glob { pattern, .. } if pattern.trim().is_empty() => {
            bail!("glob pattern cannot be empty")
        }
        Action::Grep { pattern, .. } if pattern.is_empty() => {
            bail!("grep pattern cannot be empty")
        }
        Action::Shell { command, .. } if command.trim().is_empty() => {
            bail!("shell command cannot be empty")
        }
        Action::Patch { patch, .. } if patch.trim().is_empty() => bail!("patch cannot be empty"),
        Action::UpdatePlan { plan, .. } => validate_plan(plan),
        Action::RequestUserInput { questions } => validate_questions(questions),
        Action::McpCall {
            server,
            tool,
            arguments,
        } => {
            validate_mcp_component(server, "server")?;
            validate_mcp_component(tool, "tool")?;
            if !arguments.is_object() {
                bail!("mcp arguments must be a JSON object");
            }
            Ok(())
        }
        Action::LoadSkill {
            name,
            path,
            offset,
            limit,
        } => {
            if name.trim().is_empty() {
                bail!("skill name cannot be empty");
            }
            if path.as_ref().is_some_and(|path| path.trim().is_empty()) {
                bail!("skill path cannot be empty");
            }
            if offset == &Some(0) || limit == &Some(0) {
                bail!("skill offset and limit must be greater than zero");
            }
            Ok(())
        }
        Action::StartProcess { command, .. } if command.trim().is_empty() => {
            bail!("start_process command cannot be empty")
        }
        Action::ProcessStatus {
            process_id: Some(0),
            ..
        }
        | Action::WriteProcess { process_id: 0, .. }
        | Action::StopProcess { process_id: 0 } => {
            bail!("background process ID must be greater than zero")
        }
        Action::WriteProcess { input, .. } if input.len() > 64 * 1_024 => {
            bail!("background process input cannot exceed 65536 bytes")
        }
        Action::Finish { summary } if summary.trim().is_empty() => {
            bail!("finish summary cannot be empty")
        }
        _ => Ok(()),
    }
}

fn validate_mcp_component(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || value.contains("__")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("mcp {kind} name is invalid");
    }
    Ok(())
}

fn validate_plan(plan: &[PlanItem]) -> Result<()> {
    if plan.is_empty() || plan.len() > 20 {
        bail!("plan must contain between 1 and 20 steps");
    }
    if plan
        .iter()
        .filter(|item| item.status == PlanStatus::InProgress)
        .count()
        > 1
    {
        bail!("at most one plan step may be in progress");
    }
    for item in plan {
        let length = item.step.chars().count();
        if item.step.trim().is_empty() || length > 200 {
            bail!("plan step text must contain between 1 and 200 characters");
        }
    }
    Ok(())
}

fn validate_questions(questions: &[UserQuestion]) -> Result<()> {
    if questions.is_empty() || questions.len() > 3 {
        bail!("request_user_input must contain between 1 and 3 questions");
    }
    for (index, question) in questions.iter().enumerate() {
        if !valid_question_id(&question.id) {
            bail!("question IDs must use lowercase snake_case");
        }
        if questions[..index]
            .iter()
            .any(|previous| previous.id == question.id)
        {
            bail!("question IDs must be unique");
        }
        if question.header.trim().is_empty() || question.header.chars().count() > 20 {
            bail!("question headers must contain between 1 and 20 characters");
        }
        if question.question.trim().is_empty() || question.question.chars().count() > 500 {
            bail!("question text must contain between 1 and 500 characters");
        }
        if question.options.len() < 2 || question.options.len() > 4 {
            bail!("each question must contain between 2 and 4 options");
        }
        for option in &question.options {
            if option.label.trim().is_empty() || option.label.chars().count() > 80 {
                bail!("option labels must contain between 1 and 80 characters");
            }
            if option.description.trim().is_empty() || option.description.chars().count() > 240 {
                bail!("option descriptions must contain between 1 and 240 characters");
            }
        }
    }
    Ok(())
}

fn valid_question_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}

fn default_path() -> String {
    ".".into()
}

fn default_true() -> bool {
    true
}

fn strip_code_fence(input: &str) -> Option<&str> {
    let rest = input
        .strip_prefix("```json")?
        .trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim)
}

fn object_slice(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let end = input.rfind('}')?;
    (end >= start).then(|| &input[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_action() {
        let action = parse_action(
            "I will inspect it.\n```json\n{\"action\":\"shell\",\"command\":\"rg foo\",\"description\":\"search\"}\n```",
        )
        .unwrap();
        assert_eq!(
            action,
            Action::Shell {
                command: "rg foo".into(),
                description: "search".into()
            }
        );
    }

    #[test]
    fn parses_file_tool_aliases_and_defaults() {
        assert_eq!(
            parse_action(r#"{"action":"read","path":"src/lib.rs","offset":10}"#).unwrap(),
            Action::ReadFile {
                path: "src/lib.rs".into(),
                offset: Some(10),
                limit: None,
            }
        );
        assert_eq!(
            parse_action(r#"{"action":"search","pattern":"TODO"}"#).unwrap(),
            Action::Grep {
                pattern: "TODO".into(),
                path: ".".into(),
                glob: None,
                literal: false,
                ignore_case: false,
                context: None,
                limit: None,
            }
        );
    }

    #[test]
    fn validates_interactive_plan_and_question_actions() {
        assert_eq!(
            parse_action(
                r#"{"action":"update_plan","plan":[{"step":"inspect","status":"in_progress"},{"step":"test","status":"pending"}]}"#
            )
            .unwrap(),
            Action::UpdatePlan {
                explanation: None,
                plan: vec![
                    PlanItem {
                        step: "inspect".into(),
                        status: PlanStatus::InProgress,
                    },
                    PlanItem {
                        step: "test".into(),
                        status: PlanStatus::Pending,
                    },
                ],
            }
        );
        assert!(
            parse_action(
                r#"{"action":"update_plan","plan":[{"step":"a","status":"in_progress"},{"step":"b","status":"in_progress"}]}"#
            )
            .is_err()
        );
        assert!(
            parse_action(
                r#"{"action":"request_user_input","questions":[{"id":"bad-id","header":"Choice","question":"Choose?","options":[{"label":"A","description":"First."},{"label":"B","description":"Second."}]}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn parses_and_validates_background_process_actions() {
        assert_eq!(
            parse_action(
                r#"{"action":"start_process","command":"cargo watch","description":"watch tests"}"#
            )
            .unwrap(),
            Action::StartProcess {
                command: "cargo watch".into(),
                description: "watch tests".into(),
            }
        );
        assert_eq!(
            parse_action(r#"{"action":"process_status","process_id":2,"cursor":99}"#).unwrap(),
            Action::ProcessStatus {
                process_id: Some(2),
                cursor: Some(99),
            }
        );
        assert!(
            parse_action(r#"{"action":"write_process","process_id":0,"input":"hello"}"#).is_err()
        );
        assert!(parse_action(r#"{"action":"stop_process","process_id":0}"#).is_err());
    }
}
