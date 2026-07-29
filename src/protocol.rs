use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
        Action::Finish { summary } if summary.trim().is_empty() => {
            bail!("finish summary cannot be empty")
        }
        _ => Ok(()),
    }
}

fn default_path() -> String {
    ".".into()
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
}
