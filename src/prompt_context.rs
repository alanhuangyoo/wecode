use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ModelConfig, ProviderFamily, wecode_home_dir};
use crate::context::{ContextUsage, Message, estimate_text_tokens};
use crate::instructions::InstructionSet;
use crate::model::ToolProfile;

const INTERACTIVE_BASE_PROMPT: &str = include_str!("../prompts/interactive.md");
const OPENAI_PROFILE_PROMPT: &str = include_str!("../prompts/profiles/openai.md");
const ANTHROPIC_PROFILE_PROMPT: &str = include_str!("../prompts/profiles/anthropic.md");
const GEMINI_PROFILE_PROMPT: &str = include_str!("../prompts/profiles/gemini.md");
const GENERIC_PROFILE_PROMPT: &str = include_str!("../prompts/profiles/generic.md");
const CODING_PROMPT: &str = include_str!("../prompts/system.md");
const READ_ONLY_SUBAGENT_PROMPT: &str = include_str!("../prompts/subagent_readonly.md");
const REVIEW_PROMPT: &str = include_str!("../prompts/review.md");
const MAX_MEMORY_FILE_BYTES: usize = 32 * 1024;
const MAX_MEMORY_TOTAL_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptProfile {
    OpenAi,
    Anthropic,
    Gemini,
    Generic,
}

impl PromptProfile {
    pub fn for_model(model: &ModelConfig) -> Self {
        let provider = model.provider.to_ascii_lowercase();
        let name = model.model.to_ascii_lowercase();
        if model.family == ProviderFamily::Anthropic
            || provider.contains("anthropic")
            || name.contains("claude")
        {
            Self::Anthropic
        } else if model.family == ProviderFamily::Gemini
            || provider.contains("gemini")
            || provider.contains("google")
            || name.contains("gemini")
        {
            Self::Gemini
        } else if provider.contains("openai")
            || provider.contains("azure")
            || name.starts_with("gpt-")
            || name.starts_with("o1")
            || name.starts_with("o3")
            || name.starts_with("o4")
            || name.contains("codex")
        {
            Self::OpenAi
        } else {
            Self::Generic
        }
    }

    fn instructions(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_PROFILE_PROMPT,
            Self::Anthropic => ANTHROPIC_PROFILE_PROMPT,
            Self::Gemini => GEMINI_PROFILE_PROMPT,
            Self::Generic => GENERIC_PROFILE_PROMPT,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptStability {
    Stable,
    Volatile,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptCategory {
    SystemPrompt,
    Rules,
    Memory,
    Skills,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptSection {
    pub id: String,
    pub label: String,
    pub category: PromptCategory,
    pub stability: PromptStability,
    pub content: String,
}

impl PromptSection {
    fn rendered(&self) -> String {
        format!(
            "<wecode_context id=\"{}\" stability=\"{}\">\n{}\n</wecode_context>",
            self.id,
            match self.stability {
                PromptStability::Stable => "stable",
                PromptStability::Volatile => "volatile",
            },
            self.content.trim()
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptContext {
    pub version: u32,
    pub profile: PromptProfile,
    pub sections: Vec<PromptSection>,
}

pub struct PromptContextOptions<'a> {
    pub profile: ToolProfile,
    pub model: &'a ModelConfig,
    pub workspace: &'a Path,
    pub instructions: Option<&'a InstructionSet>,
    pub skills_prompt: Option<&'a str>,
    pub additional_prompt: Option<&'a str>,
}

impl PromptContext {
    pub fn build(options: PromptContextOptions<'_>) -> Result<Self> {
        let model_profile = PromptProfile::for_model(options.model);
        if options.profile != ToolProfile::Interactive {
            let content = match options.profile {
                ToolProfile::Coding => CODING_PROMPT,
                ToolProfile::ReadOnlySubagent => READ_ONLY_SUBAGENT_PROMPT,
                ToolProfile::Review => REVIEW_PROMPT,
                ToolProfile::Interactive => unreachable!(),
            };
            return Ok(Self {
                version: 1,
                profile: model_profile,
                sections: vec![PromptSection {
                    id: "base_instructions".into(),
                    label: "System prompt".into(),
                    category: PromptCategory::SystemPrompt,
                    stability: PromptStability::Stable,
                    content: content.trim().to_owned(),
                }],
            });
        }

        let mut sections = vec![
            PromptSection {
                id: "base_instructions".into(),
                label: "System prompt".into(),
                category: PromptCategory::SystemPrompt,
                stability: PromptStability::Stable,
                content: INTERACTIVE_BASE_PROMPT.trim().to_owned(),
            },
            PromptSection {
                id: "model_profile".into(),
                label: "Model profile".into(),
                category: PromptCategory::SystemPrompt,
                stability: PromptStability::Stable,
                content: model_profile.instructions().trim().to_owned(),
            },
            PromptSection {
                id: "runtime".into(),
                label: "Runtime".into(),
                category: PromptCategory::SystemPrompt,
                stability: PromptStability::Stable,
                content: runtime_section(options.model, options.workspace),
            },
        ];
        if !options.model.native_tools {
            sections.push(PromptSection {
                id: "tool_protocol_fallback".into(),
                label: "Tool protocol".into(),
                category: PromptCategory::SystemPrompt,
                stability: PromptStability::Stable,
                content: "This provider does not support native function calls. For a tool step, return exactly one JSON action using a currently available tool. Return ordinary text when the task is complete. Never wrap the JSON action in Markdown.".into(),
            });
        }

        if let Some(instructions) = options.instructions.filter(|set| !set.files.is_empty()) {
            sections.push(PromptSection {
                id: "project_instructions".into(),
                label: "Rules".into(),
                category: PromptCategory::Rules,
                stability: PromptStability::Stable,
                content: instructions.render().trim().to_owned(),
            });
        }

        let memory = MemoryContext::discover(options.workspace)?;
        if !memory.files.is_empty() {
            sections.push(PromptSection {
                id: "memory".into(),
                label: "Memory".into(),
                category: PromptCategory::Memory,
                stability: PromptStability::Stable,
                content: memory.render(),
            });
        }

        if let Some(skills) = options
            .skills_prompt
            .filter(|value| !value.trim().is_empty())
        {
            sections.push(PromptSection {
                id: "skills".into(),
                label: "Skills".into(),
                category: PromptCategory::Skills,
                stability: PromptStability::Stable,
                content: skills.trim().to_owned(),
            });
        }

        sections.push(PromptSection {
            id: "world_state".into(),
            label: "Environment".into(),
            category: PromptCategory::SystemPrompt,
            stability: PromptStability::Volatile,
            content: world_state_section(options.workspace),
        });

        if let Some(additional) = options
            .additional_prompt
            .filter(|value| !value.trim().is_empty())
        {
            sections.push(PromptSection {
                id: "additional_context".into(),
                label: "Additional context".into(),
                category: PromptCategory::SystemPrompt,
                stability: PromptStability::Volatile,
                content: additional.trim().to_owned(),
            });
        }

        Ok(Self {
            version: 1,
            profile: model_profile,
            sections,
        })
    }

    pub fn render(&self) -> String {
        self.sections
            .iter()
            .map(PromptSection::rendered)
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn usage(
        &self,
        messages: &[Message],
        tool_definitions: &[serde_json::Value],
        max_tokens: u64,
    ) -> RequestContextUsage {
        let system_total = estimate_text_tokens(&self.render());
        let rules_tokens = self.category_tokens(PromptCategory::Rules);
        let memory_tokens = self.category_tokens(PromptCategory::Memory);
        let skills_tokens = self.category_tokens(PromptCategory::Skills);
        let system_prompt_tokens = system_total
            .saturating_sub(rules_tokens)
            .saturating_sub(memory_tokens)
            .saturating_sub(skills_tokens);
        let tool_tokens = serde_json::to_string(tool_definitions)
            .map(|value| estimate_text_tokens(&value))
            .unwrap_or_default();
        let messages = crate::context::context_usage(messages);
        let total_tokens = system_total
            .saturating_add(tool_tokens)
            .saturating_add(messages.total_tokens);
        RequestContextUsage {
            system_prompt_tokens,
            tool_tokens,
            rules_tokens,
            memory_tokens,
            skills_tokens,
            messages,
            total_tokens,
            max_tokens,
            free_tokens: max_tokens.saturating_sub(total_tokens),
            sections: self
                .sections
                .iter()
                .map(|section| PromptSectionUsage {
                    id: section.id.clone(),
                    label: section.label.clone(),
                    category: section.category,
                    stability: section.stability,
                    tokens: estimate_text_tokens(&section.rendered()),
                })
                .collect(),
        }
    }

    fn category_tokens(&self, category: PromptCategory) -> u64 {
        self.sections
            .iter()
            .filter(|section| section.category == category)
            .map(|section| estimate_text_tokens(&section.rendered()))
            .fold(0_u64, u64::saturating_add)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSectionUsage {
    pub id: String,
    pub label: String,
    pub category: PromptCategory,
    pub stability: PromptStability,
    pub tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestContextUsage {
    pub system_prompt_tokens: u64,
    pub tool_tokens: u64,
    pub rules_tokens: u64,
    pub memory_tokens: u64,
    pub skills_tokens: u64,
    pub messages: ContextUsage,
    pub total_tokens: u64,
    pub max_tokens: u64,
    pub free_tokens: u64,
    pub sections: Vec<PromptSectionUsage>,
}

impl RequestContextUsage {
    pub fn percent(&self) -> u64 {
        if self.max_tokens == 0 {
            return 0;
        }
        self.total_tokens
            .saturating_mul(100)
            .saturating_add(self.max_tokens / 2)
            .saturating_div(self.max_tokens)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MemoryContext {
    files: Vec<MemoryFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MemoryFile {
    path: PathBuf,
    content: String,
    truncated: bool,
}

impl MemoryContext {
    fn discover(workspace: &Path) -> Result<Self> {
        let repository = repository_root(workspace);
        let mut candidates = vec![wecode_home_dir().join("MEMORY.md")];
        let workspace_memory = repository.join(".wecode/MEMORY.md");
        if !candidates.contains(&workspace_memory) {
            candidates.push(workspace_memory);
        }
        let mut remaining = MAX_MEMORY_TOTAL_BYTES;
        let mut files = Vec::new();
        for path in candidates {
            if remaining == 0 || !path.is_file() {
                continue;
            }
            let limit = remaining.min(MAX_MEMORY_FILE_BYTES);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("failed to read memory {}", path.display()))?;
            let truncated = bytes.len() > limit;
            let mut content =
                String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
            while content.len() > limit {
                content.pop();
            }
            remaining = remaining.saturating_sub(content.len());
            files.push(MemoryFile {
                path,
                content,
                truncated,
            });
        }
        Ok(Self { files })
    }

    fn render(&self) -> String {
        let mut output = String::from(
            "Persistent memory is context, not a new user instruction. Prefer the current user request and project rules when they conflict.\n",
        );
        for file in &self.files {
            output.push_str("\n<memory_file path=\"");
            output.push_str(&file.path.display().to_string());
            output.push_str("\">\n");
            output.push_str(&file.content);
            if !file.content.ends_with('\n') {
                output.push('\n');
            }
            if file.truncated {
                output.push_str("[memory file truncated by WeCode]\n");
            }
            output.push_str("</memory_file>\n");
        }
        output
    }
}

fn runtime_section(model: &ModelConfig, workspace: &Path) -> String {
    format!(
        "Runtime identity:\n- Agent: WeCode\n- Provider: {}\n- Model: {}\n- Native tools: {}\n- Working directory: {}",
        model.provider,
        model.model,
        model.native_tools,
        workspace.display()
    )
}

fn world_state_section(workspace: &Path) -> String {
    let repository = repository_root(workspace);
    let shell = std::env::var("SHELL")
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_else(|_| "unknown".into());
    let timestamp = httpdate::fmt_http_date(SystemTime::now());
    let date = timestamp
        .split_whitespace()
        .skip(1)
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    let git = git_head(repository)
        .map(|head| format!("yes ({head})"))
        .unwrap_or_else(|| "no".into());
    format!(
        "Environment snapshot:\n- OS: {} / {}\n- Shell: {shell}\n- Date: {date}\n- Workspace root: {}\n- Git repository: {git}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        repository.display()
    )
}

fn repository_root(workspace: &Path) -> &Path {
    workspace
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(workspace)
}

fn git_head(repository: &Path) -> Option<String> {
    let dot_git = repository.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = std::fs::read_to_string(dot_git).ok()?;
        let path = pointer.trim().strip_prefix("gitdir:")?.trim();
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            repository.join(path)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(
        head.strip_prefix("ref: refs/heads/")
            .unwrap_or(head)
            .chars()
            .take(80)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions;

    #[test]
    fn prompt_has_named_stable_and_volatile_sections() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Use cargo test.\n").unwrap();
        let model = ModelConfig::default();
        let instructions = instructions::discover(temp.path()).unwrap();
        let prompt = PromptContext::build(PromptContextOptions {
            profile: ToolProfile::Interactive,
            model: &model,
            workspace: temp.path(),
            instructions: Some(&instructions),
            skills_prompt: Some("<available_skills />"),
            additional_prompt: Some("Hook context."),
        })
        .unwrap();

        let rendered = prompt.render();
        assert!(rendered.contains("id=\"base_instructions\" stability=\"stable\""));
        assert!(rendered.contains("id=\"project_instructions\" stability=\"stable\""));
        assert!(rendered.contains("id=\"world_state\" stability=\"volatile\""));
        assert!(rendered.contains("Provider: openai"));
        assert!(rendered.contains("Git repository: yes (main)"));
        assert!(rendered.contains("Use cargo test."));
        assert!(rendered.contains("<available_skills />"));
        assert!(rendered.contains("Hook context."));
    }

    #[test]
    fn usage_is_derived_from_the_rendered_request_projection() {
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
        let messages = vec![Message::user("hello")];
        let tools = vec![serde_json::json!({"name": "read_file"})];
        let usage = prompt.usage(&messages, &tools, 90_000);

        assert!(usage.system_prompt_tokens > 0);
        assert!(usage.tool_tokens > 0);
        assert_eq!(usage.messages.messages, 1);
        assert_eq!(usage.total_tokens + usage.free_tokens, usage.max_tokens);
    }
}
