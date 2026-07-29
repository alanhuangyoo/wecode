use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::protocol::{Action, PlanItem, UserQuestion, validate_action};

pub const MAX_PARALLEL_TOOL_CALLS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolProfile {
    #[default]
    Coding,
    Interactive,
    ReadOnlySubagent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolConcurrency {
    ParallelRead,
    ParallelSpawn,
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

    pub fn for_profile(profile: ToolProfile) -> Self {
        let mut registry = Self::builtins();
        match profile {
            ToolProfile::Coding => {}
            ToolProfile::Interactive => {
                registry.tools.extend(
                    interactive_definitions()
                        .into_iter()
                        .map(|definition| ToolSpec { definition }),
                );
            }
            ToolProfile::ReadOnlySubagent => {
                registry.tools.retain(|tool| {
                    matches!(
                        tool.definition.get("name").and_then(Value::as_str),
                        Some("read_file" | "list_files" | "glob" | "grep" | "finish")
                    )
                });
            }
        }
        registry
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
            Action::Shell { .. }
            | Action::Patch { .. }
            | Action::UpdatePlan { .. }
            | Action::RequestUserInput { .. }
            | Action::McpCall { .. }
            | Action::LoadSkill { .. }
            | Action::StartProcess { .. }
            | Action::ProcessStatus { .. }
            | Action::WriteProcess { .. }
            | Action::StopProcess { .. }
            | Action::Lsp { .. }
            | Action::AgentStatus { .. }
            | Action::SendAgent { .. }
            | Action::WaitAgent { .. }
            | Action::StopAgent { .. }
            | Action::SpawnAgent {
                background: false, ..
            } => ToolConcurrency::Exclusive,
            Action::SpawnAgent {
                background: true, ..
            } => ToolConcurrency::ParallelSpawn,
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
        if actions.len() > 1 {
            let concurrency = Self::concurrency(&actions[0]);
            if !matches!(
                concurrency,
                ToolConcurrency::ParallelRead | ToolConcurrency::ParallelSpawn
            ) || !actions
                .iter()
                .all(|action| Self::concurrency(action) == concurrency)
            {
                bail!(
                    "only independent repository reads or background spawn_agent calls may be batched; other tools must run one at a time"
                );
            }
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
            "update_plan" => Action::UpdatePlan {
                explanation: arguments
                    .get("explanation")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                plan: serde_json::from_value::<Vec<PlanItem>>(
                    arguments.get("plan").cloned().unwrap_or(Value::Null),
                )
                .context("update_plan returned invalid plan items")?,
            },
            "request_user_input" | "question" => Action::RequestUserInput {
                questions: serde_json::from_value::<Vec<UserQuestion>>(
                    arguments.get("questions").cloned().unwrap_or(Value::Null),
                )
                .context("request_user_input returned invalid questions")?,
            },
            "load_skill" | "skill" => Action::LoadSkill {
                name: get_string("name"),
                path: arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                offset: get_usize("offset"),
                limit: get_usize("limit"),
            },
            "start_process" => Action::StartProcess {
                command: get_string("command"),
                description: get_string("description"),
            },
            "process_status" => Action::ProcessStatus {
                process_id: arguments.get("process_id").and_then(Value::as_u64),
                cursor: arguments.get("cursor").and_then(Value::as_u64),
            },
            "write_process" => Action::WriteProcess {
                process_id: arguments
                    .get("process_id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                input: get_string("input"),
                newline: arguments
                    .get("newline")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            },
            "stop_process" => Action::StopProcess {
                process_id: arguments
                    .get("process_id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            },
            "lsp" => Action::Lsp {
                operation: serde_json::from_value(
                    arguments.get("operation").cloned().unwrap_or(Value::Null),
                )
                .context("lsp operation is missing or invalid")?,
                path: get_string("path"),
                line: arguments
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
                character: arguments
                    .get("character")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
                query: arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            },
            "spawn_agent" => Action::SpawnAgent {
                description: get_string("description"),
                prompt: get_string("prompt"),
                agent_type: arguments
                    .get("agent_type")
                    .and_then(Value::as_str)
                    .unwrap_or("general-purpose")
                    .to_owned(),
                background: arguments
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                model: arguments
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            },
            "agent_status" => Action::AgentStatus {
                agent_id: arguments.get("agent_id").and_then(Value::as_u64),
            },
            "send_agent" => Action::SendAgent {
                agent_id: arguments
                    .get("agent_id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                message: get_string("message"),
            },
            "wait_agent" => Action::WaitAgent {
                agent_ids: arguments
                    .get("agent_ids")
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default(),
                timeout_seconds: arguments.get("timeout_seconds").and_then(Value::as_u64),
            },
            "stop_agent" => Action::StopAgent {
                agent_id: arguments
                    .get("agent_id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            },
            "finish" => Action::Finish {
                summary: get_string("summary"),
            },
            _ if name.starts_with("mcp__") => {
                let mut parts = name.splitn(3, "__");
                let prefix = parts.next();
                let server = parts.next().unwrap_or_default();
                let tool = parts.next().unwrap_or_default();
                if prefix != Some("mcp") || server.is_empty() || tool.is_empty() {
                    bail!("provider returned malformed MCP tool call {name:?}");
                }
                Action::McpCall {
                    server: server.to_owned(),
                    tool: tool.to_owned(),
                    arguments,
                }
            }
            _ => bail!("provider returned unknown tool call {name:?}"),
        };
        validate_action(&action)?;
        Ok(action)
    }
}

fn interactive_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "update_plan",
            "description": "Create or update the visible task plan. Use it for multi-step work and keep statuses current. At most one step may be in progress.",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {"type": "string", "description": "Optional short reason for this plan update."},
                    "plan": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {"type": "string", "description": "Concise task step."},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "request_user_input",
            "description": "Ask one to three concise questions only when a user choice materially changes the result. Wait for the answers before continuing.",
            "parameters": {
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "Stable lowercase snake_case identifier."},
                                "header": {"type": "string", "description": "Short UI heading."},
                                "question": {"type": "string", "description": "One clear question."},
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "Short choice label."},
                                            "description": {"type": "string", "description": "One-sentence tradeoff."}
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["id", "header", "question", "options"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "load_skill",
            "description": "Load specialized instructions or a relative text resource from a discovered skill. Load SKILL.md before following a skill, then use path to read referenced files progressively.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Exact skill name from available_skills or an explicit user invocation."},
                    "path": {"type": "string", "description": "Optional path relative to the skill base directory. Defaults to SKILL.md."},
                    "offset": {"type": "integer", "minimum": 1, "description": "Optional 1-indexed first line."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum lines to return."}
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "start_process",
            "description": "Start a long-running command such as a dev server, watcher, or extended test in the workspace. It runs without blocking the conversation and returns a process ID.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Foreground shell command. Do not append & or another background operator."},
                    "description": {"type": "string", "description": "Short human-readable purpose."}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "process_status",
            "description": "List background processes or read incremental output from one process. Reuse next_cursor on later calls to avoid repeated output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "process_id": {"type": "integer", "minimum": 1, "description": "Process to inspect. Omit to list all processes."},
                    "cursor": {"type": "integer", "minimum": 0, "description": "Output cursor returned by the prior status call."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "write_process",
            "description": "Write bounded input to a running background process stdin.",
            "parameters": {
                "type": "object",
                "properties": {
                    "process_id": {"type": "integer", "minimum": 1},
                    "input": {"type": "string", "description": "Text to write."},
                    "newline": {"type": "boolean", "description": "Append a newline. Defaults to true."}
                },
                "required": ["process_id", "input"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "stop_process",
            "description": "Stop a running background process and its child process tree.",
            "parameters": {
                "type": "object",
                "properties": {
                    "process_id": {"type": "integer", "minimum": 1}
                },
                "required": ["process_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "lsp",
            "description": "Query an installed Language Server for semantic code intelligence. Servers start lazily by file type. Positions are 1-based. Diagnostics also arrive automatically after files are synchronized.",
            "parameters": {
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": [
                            "go_to_definition",
                            "find_references",
                            "hover",
                            "document_symbols",
                            "workspace_symbols",
                            "go_to_implementation",
                            "prepare_call_hierarchy",
                            "incoming_calls",
                            "outgoing_calls",
                            "diagnostics"
                        ]
                    },
                    "path": {"type": "string", "description": "Workspace-relative source file used for routing and document synchronization."},
                    "line": {"type": "integer", "minimum": 1, "description": "1-based line for position operations."},
                    "character": {"type": "integer", "minimum": 1, "description": "1-based UTF-16 character offset for position operations."},
                    "query": {"type": "string", "description": "Symbol query for workspace_symbols."}
                },
                "required": ["operation", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "spawn_agent",
            "description": "Delegate a concrete, self-contained task to an isolated-context subagent. Foreground waits for the result; background returns immediately and sends an automatic completion notification. Multiple independent background spawns may be called together.",
            "parameters": {
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "Short 3-8 word task label."},
                    "prompt": {"type": "string", "description": "Complete task brief with relevant context, scope, expected output, and whether edits are allowed."},
                    "agent_type": {
                        "type": "string",
                        "description": "Role: general-purpose, explore, plan, review, or a configured custom role. Defaults to general-purpose."
                    },
                    "background": {"type": "boolean", "description": "Run independently in the background. Completion is delivered automatically."},
                    "model": {"type": "string", "description": "Optional model override on the current provider."}
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "agent_status",
            "description": "List subagents or inspect one subagent's bounded status and latest result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id": {"type": "integer", "minimum": 1, "description": "Agent to inspect. Omit to list all agents."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_agent",
            "description": "Send additional context to a running subagent at its next safe model boundary, or continue a completed subagent with its preserved conversation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id": {"type": "integer", "minimum": 1},
                    "message": {"type": "string", "description": "New context or follow-up task."}
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "wait_agent",
            "description": "Wait briefly for one or more background subagents when their results are required before useful work can continue. Completion notifications otherwise arrive automatically.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                        "items": {"type": "integer", "minimum": 1}
                    },
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 60}
                },
                "required": ["agent_ids"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "stop_agent",
            "description": "Cancel a queued or running subagent. Its completed conversation remains inspectable.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id": {"type": "integer", "minimum": 1}
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }
        }),
    ]
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

    #[test]
    fn interactive_profile_adds_session_tools_without_changing_benchmark_tools() {
        let coding = ToolRegistry::for_profile(ToolProfile::Coding).definitions();
        let interactive = ToolRegistry::for_profile(ToolProfile::Interactive).definitions();
        let read_only = ToolRegistry::for_profile(ToolProfile::ReadOnlySubagent).definitions();
        assert_eq!(coding.len(), 7);
        assert_eq!(interactive.len(), 20);
        assert_eq!(
            interactive
                .iter()
                .skip(coding.len())
                .filter_map(|tool| tool["name"].as_str())
                .collect::<Vec<_>>(),
            vec![
                "update_plan",
                "request_user_input",
                "load_skill",
                "start_process",
                "process_status",
                "write_process",
                "stop_process",
                "lsp",
                "spawn_agent",
                "agent_status",
                "send_agent",
                "wait_agent",
                "stop_agent",
            ]
        );
        assert_eq!(
            read_only
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect::<Vec<_>>(),
            vec!["read_file", "list_files", "glob", "grep", "finish"]
        );
    }

    #[test]
    fn only_background_subagent_spawns_may_be_batched() {
        let background = |description: &str| Action::SpawnAgent {
            description: description.into(),
            prompt: "Inspect the repository.".into(),
            agent_type: "explore".into(),
            background: true,
            model: None,
        };
        assert!(ToolRegistry::validate_batch(&[background("one"), background("two")]).is_ok());

        let mut foreground = background("foreground");
        if let Action::SpawnAgent {
            ref mut background, ..
        } = foreground
        {
            *background = false;
        }
        assert!(ToolRegistry::validate_batch(&[foreground, background("background")]).is_err());
        assert!(
            ToolRegistry::validate_batch(&[
                background("background"),
                Action::ReadFile {
                    path: "src/lib.rs".into(),
                    offset: None,
                    limit: None,
                },
            ])
            .is_err()
        );
    }

    #[test]
    fn subagent_calls_preserve_lifecycle_fields() {
        assert_eq!(
            ToolRegistry::parse_call(
                "spawn_agent",
                json!({
                    "description": "review parser",
                    "prompt": "Inspect parser edge cases.",
                    "agent_type": "review",
                    "background": true,
                    "model": "small-model"
                })
            )
            .unwrap(),
            Action::SpawnAgent {
                description: "review parser".into(),
                prompt: "Inspect parser edge cases.".into(),
                agent_type: "review".into(),
                background: true,
                model: Some("small-model".into()),
            }
        );
        assert_eq!(
            ToolRegistry::parse_call("agent_status", json!({"agent_id": 3})).unwrap(),
            Action::AgentStatus { agent_id: Some(3) }
        );
        assert_eq!(
            ToolRegistry::parse_call(
                "send_agent",
                json!({"agent_id": 3, "message": "Check Windows too."})
            )
            .unwrap(),
            Action::SendAgent {
                agent_id: 3,
                message: "Check Windows too.".into(),
            }
        );
        assert_eq!(
            ToolRegistry::parse_call(
                "wait_agent",
                json!({"agent_ids": [2, 3], "timeout_seconds": 12})
            )
            .unwrap(),
            Action::WaitAgent {
                agent_ids: vec![2, 3],
                timeout_seconds: Some(12),
            }
        );
        assert_eq!(
            ToolRegistry::parse_call("stop_agent", json!({"agent_id": 3})).unwrap(),
            Action::StopAgent { agent_id: 3 }
        );
    }

    #[test]
    fn process_calls_preserve_ids_cursors_and_input_mode() {
        assert_eq!(
            ToolRegistry::parse_call(
                "start_process",
                json!({"command": "cargo watch", "description": "watch tests"})
            )
            .unwrap(),
            Action::StartProcess {
                command: "cargo watch".into(),
                description: "watch tests".into(),
            }
        );
        assert_eq!(
            ToolRegistry::parse_call("process_status", json!({"process_id": 4, "cursor": 1024}))
                .unwrap(),
            Action::ProcessStatus {
                process_id: Some(4),
                cursor: Some(1024),
            }
        );
        assert_eq!(
            ToolRegistry::parse_call(
                "write_process",
                json!({"process_id": 4, "input": "q", "newline": false})
            )
            .unwrap(),
            Action::WriteProcess {
                process_id: 4,
                input: "q".into(),
                newline: false,
            }
        );
        assert!(ToolRegistry::parse_call("stop_process", json!({"process_id": 0})).is_err());
    }

    #[test]
    fn lsp_calls_use_typed_operations_and_one_based_positions() {
        assert_eq!(
            ToolRegistry::parse_call(
                "lsp",
                json!({
                    "operation": "go_to_definition",
                    "path": "src/main.rs",
                    "line": 10,
                    "character": 7
                })
            )
            .unwrap(),
            Action::Lsp {
                operation: crate::protocol::LspOperation::GoToDefinition,
                path: "src/main.rs".into(),
                line: Some(10),
                character: Some(7),
                query: None,
            }
        );
        assert!(
            ToolRegistry::parse_call("lsp", json!({"operation": "hover", "path": "src/main.rs"}))
                .is_err()
        );
    }

    #[test]
    fn namespaced_mcp_calls_preserve_server_tool_and_arguments() {
        let arguments = json!({"path": "README.md"});
        let action = ToolRegistry::parse_call("mcp__files__read_text", arguments.clone()).unwrap();
        assert_eq!(
            action,
            Action::McpCall {
                server: "files".into(),
                tool: "read_text".into(),
                arguments,
            }
        );
        assert!(ToolRegistry::parse_call("mcp__broken", json!({})).is_err());
    }

    #[test]
    fn skill_calls_preserve_progressive_resource_ranges() {
        assert_eq!(
            ToolRegistry::parse_call(
                "load_skill",
                json!({
                    "name": "code-review",
                    "path": "references/checklist.md",
                    "offset": 20,
                    "limit": 50
                })
            )
            .unwrap(),
            Action::LoadSkill {
                name: "code-review".into(),
                path: Some("references/checklist.md".into()),
                offset: Some(20),
                limit: Some(50),
            }
        );
    }
}
