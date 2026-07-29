use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::protocol::{Action, validate_action};

pub const MAX_PARALLEL_TOOL_CALLS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolConcurrency {
    ParallelRead,
    Exclusive,
    Terminal,
}

#[derive(Clone, Debug)]
struct ToolSpec {
    definition: Value,
}

#[derive(Clone, Debug)]
pub struct ToolRegistry {
    tools: Vec<ToolSpec>,
}

impl ToolRegistry {
    pub fn builtins() -> Self {
        Self {
            tools: builtin_definitions()
                .into_iter()
                .map(|definition| ToolSpec { definition })
                .collect(),
        }
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub fn concurrency(action: &Action) -> ToolConcurrency {
        match action {
            Action::ReadFile { .. }
            | Action::ListFiles { .. }
            | Action::Glob { .. }
            | Action::Grep { .. } => ToolConcurrency::ParallelRead,
            Action::Shell { .. } | Action::Patch { .. } => ToolConcurrency::Exclusive,
            Action::Finish { .. } => ToolConcurrency::Terminal,
        }
    }

    pub fn validate_batch(actions: &[Action]) -> Result<()> {
        if actions.len() > MAX_PARALLEL_TOOL_CALLS {
            bail!(
                "received {} calls; at most {MAX_PARALLEL_TOOL_CALLS} are allowed per model turn",
                actions.len()
            );
        }
        if actions.len() > 1
            && !actions
                .iter()
                .all(|action| Self::concurrency(action) == ToolConcurrency::ParallelRead)
        {
            bail!(
                "only read_file, list_files, glob, and grep may be called together; shell, apply_patch, and finish must run one at a time"
            );
        }
        Ok(())
    }

    pub fn parse_call(name: &str, arguments: Value) -> Result<Action> {
        let get_string = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let get_usize = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
        };
        let path_or_root = || {
            let value = get_string("path");
            if value.is_empty() { ".".into() } else { value }
        };
        let action = match name {
            "read_file" | "read" => Action::ReadFile {
                path: get_string("path"),
                offset: get_usize("offset"),
                limit: get_usize("limit"),
            },
            "list_files" | "list_dir" | "ls" => Action::ListFiles {
                path: path_or_root(),
                depth: get_usize("depth"),
                limit: get_usize("limit"),
            },
            "glob" | "find" => Action::Glob {
                pattern: get_string("pattern"),
                path: path_or_root(),
                limit: get_usize("limit"),
            },
            "grep" | "search" => Action::Grep {
                pattern: get_string("pattern"),
                path: path_or_root(),
                glob: arguments
                    .get("glob")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                literal: arguments
                    .get("literal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                ignore_case: arguments
                    .get("ignore_case")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                context: get_usize("context"),
                limit: get_usize("limit"),
            },
            "shell" => Action::Shell {
                command: get_string("command"),
                description: get_string("description"),
            },
            "apply_patch" | "patch" => Action::Patch {
                patch: get_string("patch"),
                description: get_string("description"),
            },
            "finish" => Action::Finish {
                summary: get_string("summary"),
            },
            _ => bail!("provider returned unknown tool call {name:?}"),
        };
        validate_action(&action)?;
        Ok(action)
    }
}

fn builtin_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_file",
            "description": "Read a UTF-8 text file in the workspace with stable line numbers. Use offset and limit to continue through large files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-indexed first line."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum lines to return."}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_files",
            "description": "List a workspace directory deterministically. Respects .gitignore and excludes .git metadata.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path. Defaults to the workspace root."},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 8, "description": "Recursive depth. Defaults to 2."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum entries."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "glob",
            "description": "Find workspace files by glob without spawning a shell. Respects .gitignore and returns deterministic paths.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob such as **/*.rs or *.toml."},
                    "path": {"type": "string", "description": "Workspace-relative search root. Defaults to ."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum matches."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "grep",
            "description": "Search text files in the workspace. Supports regex or literal matching, optional context, glob filtering, .gitignore, and deterministic bounded output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression or literal text."},
                    "path": {"type": "string", "description": "Workspace-relative file or directory. Defaults to ."},
                    "glob": {"type": "string", "description": "Optional file filter such as **/*.rs."},
                    "literal": {"type": "boolean", "description": "Treat pattern as literal text. Defaults to false."},
                    "ignore_case": {"type": "boolean", "description": "Case-insensitive search. Defaults to false."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 5, "description": "Context lines before and after matches."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum matching lines."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "shell",
            "description": "Run one non-interactive shell command in the repository workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute."},
                    "description": {"type": "string", "description": "A short description of the intent."}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apply_patch",
            "description": "Apply a Codex-format patch beginning with *** Begin Patch and ending with *** End Patch.",
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The complete Codex apply_patch payload."},
                    "description": {"type": "string", "description": "A short description of the edit."}
                },
                "required": ["patch"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "finish",
            "description": "Finish only after the task is complete and relevant verification has run.",
            "parameters": {
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "What changed and how it was verified."}
                },
                "required": ["summary"],
                "additionalProperties": false
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_unique_builtins_and_scheduling_rules() {
        let registry = ToolRegistry::builtins();
        let definitions = registry.definitions();
        let mut names = definitions
            .iter()
            .filter_map(|definition| definition.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();

        assert_eq!(definitions.len(), 7);
        assert_eq!(names.len(), definitions.len());
        assert!(
            ToolRegistry::validate_batch(&[
                Action::ReadFile {
                    path: "a.rs".into(),
                    offset: None,
                    limit: None,
                },
                Action::Grep {
                    pattern: "main".into(),
                    path: ".".into(),
                    glob: None,
                    literal: false,
                    ignore_case: false,
                    context: None,
                    limit: None,
                },
            ])
            .is_ok()
        );
        assert!(
            ToolRegistry::validate_batch(&[
                Action::Shell {
                    command: "cargo test".into(),
                    description: String::new(),
                },
                Action::ReadFile {
                    path: "a.rs".into(),
                    offset: None,
                    limit: None,
                },
            ])
            .is_err()
        );
    }
}
