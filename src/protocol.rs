use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
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
    Finish {
        summary: String,
    },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Shell { .. } => "shell",
            Self::Patch { .. } => "patch",
            Self::Finish { .. } => "finish",
        }
    }

    pub fn description(&self) -> &str {
        match self {
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
        Action::Shell { command, .. } if command.trim().is_empty() => {
            bail!("shell command cannot be empty")
        }
        Action::Patch { patch, .. } if patch.trim().is_empty() => bail!("patch cannot be empty"),
        Action::Finish { summary } if summary.trim().is_empty() => {
            bail!("finish summary cannot be empty")
        }
        _ => Ok(()),
    }
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
}
