use std::collections::BTreeMap;
use std::io::{Read, Take};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use regex::{Captures, Regex};

use crate::config::{CommandsConfig, wecode_home_dir};

const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandScope {
    User,
    Project,
    Explicit,
}

impl CommandScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PromptCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub template: String,
    pub file: PathBuf,
    pub scope: CommandScope,
}

#[derive(Clone, Debug)]
pub struct CommandDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct CommandCatalog {
    inner: Arc<CommandCatalogInner>,
}

#[derive(Default)]
struct CommandCatalogInner {
    commands: BTreeMap<String, PromptCommand>,
    diagnostics: Vec<CommandDiagnostic>,
}

#[derive(Clone)]
struct DiscoveryRoot {
    path: PathBuf,
    scope: CommandScope,
    priority: usize,
}

impl CommandCatalog {
    pub fn discover(workspace: &Path, config: &CommandsConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self::default());
        }
        Self::discover_roots(&discovery_roots(workspace, config), config)
    }

    fn discover_roots(roots: &[DiscoveryRoot], config: &CommandsConfig) -> Result<Self> {
        let mut selected: BTreeMap<String, (usize, PromptCommand)> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for root in roots {
            let files = match discover_command_files(&root.path) {
                Ok(files) => files,
                Err(error) => {
                    diagnostics.push(CommandDiagnostic {
                        path: root.path.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            for file in files {
                let command = match parse_command(&file, root.scope, config.max_file_bytes) {
                    Ok(command) => command,
                    Err(error) => {
                        diagnostics.push(CommandDiagnostic {
                            path: file,
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if is_reserved(&command.name) {
                    diagnostics.push(CommandDiagnostic {
                        path: command.file,
                        message: format!(
                            "command {:?} conflicts with a built-in slash command and was ignored",
                            command.name
                        ),
                    });
                    continue;
                }
                if let Some((previous_priority, previous)) = selected.get(&command.name) {
                    diagnostics.push(CommandDiagnostic {
                        path: command.file.clone(),
                        message: if root.priority > *previous_priority {
                            format!(
                                "command {:?} overrides {}",
                                command.name,
                                previous.file.display()
                            )
                        } else {
                            format!(
                                "duplicate command {:?} ignored; {} has equal or higher precedence",
                                command.name,
                                previous.file.display()
                            )
                        },
                    });
                    if root.priority <= *previous_priority {
                        continue;
                    }
                }
                selected.insert(command.name.clone(), (root.priority, command));
            }
        }
        let omitted = selected.len().saturating_sub(config.max_commands);
        let mut ranked = selected.into_iter().collect::<Vec<_>>();
        ranked.sort_by(
            |(left_name, (left_priority, _)), (right_name, (right_priority, _))| {
                right_priority
                    .cmp(left_priority)
                    .then_with(|| left_name.cmp(right_name))
            },
        );
        ranked.truncate(config.max_commands);
        let commands = ranked
            .into_iter()
            .map(|(name, (_, command))| (name, command))
            .collect();
        if omitted > 0 {
            diagnostics.push(CommandDiagnostic {
                path: PathBuf::from("<catalog>"),
                message: format!(
                    "{omitted} commands omitted by the {}-command catalog limit",
                    config.max_commands
                ),
            });
        }
        Ok(Self {
            inner: Arc::new(CommandCatalogInner {
                commands,
                diagnostics,
            }),
        })
    }

    pub fn commands(&self) -> Vec<PromptCommand> {
        self.inner.commands.values().cloned().collect()
    }

    pub fn diagnostics(&self) -> &[CommandDiagnostic] {
        &self.inner.diagnostics
    }

    pub fn len(&self) -> usize {
        self.inner.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.commands.is_empty()
    }

    pub fn expand(&self, name: &str, arguments: &str) -> Result<String> {
        let command = self
            .inner
            .commands
            .get(name)
            .with_context(|| format!("unknown command \"/{name}\""))?;
        let args = parse_arguments(arguments);
        substitute_arguments(&command.template, &args)
    }
}

fn discovery_roots(workspace: &Path, config: &CommandsConfig) -> Vec<DiscoveryRoot> {
    let mut roots = Vec::new();
    let mut priority = 0_usize;
    let mut push = |path: PathBuf, scope| {
        priority = priority.saturating_add(1);
        roots.push(DiscoveryRoot {
            path,
            scope,
            priority,
        });
    };
    if config.discover_user {
        if config.compatibility_directories
            && let Some(base) = BaseDirs::new()
        {
            push(
                base.home_dir().join(".config/opencode/command"),
                CommandScope::User,
            );
            push(
                base.home_dir().join(".config/opencode/commands"),
                CommandScope::User,
            );
            push(
                base.home_dir().join(".pi/agent/prompts"),
                CommandScope::User,
            );
            push(base.home_dir().join(".claude/commands"), CommandScope::User);
        }
        push(wecode_home_dir().join("commands"), CommandScope::User);
    }
    if config.discover_project {
        let root = repository_root(workspace);
        let mut directories = workspace
            .ancestors()
            .take_while(|directory| *directory != root)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        directories.push(root.to_path_buf());
        directories.reverse();
        for directory in directories {
            if config.compatibility_directories {
                push(directory.join(".opencode/command"), CommandScope::Project);
                push(directory.join(".opencode/commands"), CommandScope::Project);
                push(directory.join(".pi/prompts"), CommandScope::Project);
                push(directory.join(".claude/commands"), CommandScope::Project);
            }
            push(directory.join(".wecode/commands"), CommandScope::Project);
        }
    }
    for path in &config.paths {
        let path = expand_home(path);
        push(
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            },
            CommandScope::Explicit,
        );
    }
    roots
}

fn discover_command_files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok(
            (root.extension().is_some_and(|extension| extension == "md"))
                .then(|| root.to_path_buf())
                .into_iter()
                .collect(),
        );
    }
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = std::fs::read_dir(root)
        .with_context(|| format!("failed to scan command directory {}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            (file_type.is_file()
                && !file_type.is_symlink()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "md"))
            .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn parse_command(path: &Path, scope: CommandScope, max_file_bytes: usize) -> Result<PromptCommand> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve command {}", path.display()))?;
    let raw = read_bounded(&canonical, max_file_bytes)?;
    let (frontmatter, template) = split_frontmatter(&raw)?;
    let fields = parse_frontmatter_fields(frontmatter);
    let name = canonical
        .file_stem()
        .and_then(|name| name.to_str())
        .context("command filename must be valid UTF-8")?
        .to_owned();
    validate_name(&name)?;
    let template = template.trim().to_owned();
    if template.is_empty() {
        bail!("command template cannot be empty");
    }
    let description = fields
        .get("description")
        .cloned()
        .or_else(|| {
            template
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.trim_start_matches('#').trim().to_owned())
        })
        .unwrap_or_default();
    let description = truncate_bytes(&description, MAX_DESCRIPTION_BYTES);
    let argument_hint = fields
        .get("argument-hint")
        .map(|hint| truncate_bytes(hint, MAX_DESCRIPTION_BYTES))
        .filter(|hint| !hint.is_empty());
    Ok(PromptCommand {
        name,
        description,
        argument_hint,
        template,
        file: canonical,
        scope,
    })
}

fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(first_newline) = raw.find('\n') else {
        return Ok(("", raw));
    };
    if raw[..first_newline].trim_end_matches('\r') != "---" {
        return Ok(("", raw));
    }
    let mut offset = first_newline + 1;
    for line in raw[offset..].split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((&raw[first_newline + 1..start], &raw[offset..]));
        }
    }
    bail!("command frontmatter is missing its closing delimiter")
}

fn parse_frontmatter_fields(frontmatter: &str) -> BTreeMap<String, String> {
    frontmatter
        .lines()
        .filter(|line| !line.starts_with([' ', '\t']))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| {
            (
                key.trim().to_ascii_lowercase(),
                unquote_scalar(value.trim()),
            )
        })
        .collect()
}

fn unquote_scalar(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value)
            .unwrap_or_else(|_| value[1..value.len() - 1].to_owned());
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value
        .split_once(" #")
        .map(|(value, _)| value)
        .unwrap_or(value)
        .trim()
        .to_owned()
}

fn parse_arguments(value: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in value.chars() {
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                current.push(character);
            }
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

fn substitute_arguments(template: &str, arguments: &[String]) -> Result<String> {
    let pattern = Regex::new(
        r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)",
    )
    .context("invalid built-in command argument pattern")?;
    let all = arguments.join(" ");
    Ok(pattern
        .replace_all(template, |captures: &Captures<'_>| {
            if let Some(target) = captures.get(1) {
                let value = if matches!(target.as_str(), "@" | "ARGUMENTS") {
                    all.as_str()
                } else {
                    target
                        .as_str()
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| arguments.get(index.saturating_sub(1)))
                        .map(String::as_str)
                        .unwrap_or("")
                };
                return if value.is_empty() {
                    captures.get(2).map_or("", |value| value.as_str())
                } else {
                    value
                }
                .to_owned();
            }
            if let Some(start) = captures.get(3) {
                let start = start
                    .as_str()
                    .parse::<usize>()
                    .unwrap_or(1)
                    .saturating_sub(1);
                let values = &arguments[start.min(arguments.len())..];
                let values = captures
                    .get(4)
                    .and_then(|length| length.as_str().parse::<usize>().ok())
                    .map_or(values, |length| &values[..length.min(values.len())]);
                return values.join(" ");
            }
            match captures.get(5).map(|value| value.as_str()) {
                Some("@" | "ARGUMENTS") => all.clone(),
                Some(index) => index
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| arguments.get(index.saturating_sub(1)))
                    .cloned()
                    .unwrap_or_default(),
                None => String::new(),
            }
        })
        .into_owned())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.starts_with(['-', '_'])
        || name.ends_with(['-', '_'])
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
    {
        bail!(
            "command filename must be 1-64 lowercase ASCII letters, digits, hyphens, or underscores"
        );
    }
    Ok(())
}

fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "allow"
            | "always"
            | "approve"
            | "approve-session"
            | "branch"
            | "cancel"
            | "checkpoint"
            | "checkpoints"
            | "clear"
            | "clear-queue"
            | "config"
            | "deny"
            | "exit"
            | "follow-up"
            | "followup"
            | "fork"
            | "help"
            | "history"
            | "instructions"
            | "later"
            | "mark"
            | "marks"
            | "mcp"
            | "model"
            | "name"
            | "new"
            | "plan"
            | "queue"
            | "quit"
            | "reject"
            | "rename"
            | "resume"
            | "rewind"
            | "rollback"
            | "rules"
            | "sessions"
            | "skills"
            | "status"
            | "steer"
            | "stop"
            | "todo"
            | "todos"
    )
}

fn read_bounded(path: &Path, limit: usize) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(8 * 1_024));
    let mut reader: Take<std::fs::File> = file.take(limit.saturating_add(1) as u64);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("command file exceeds the configured {limit}-byte limit");
    }
    String::from_utf8(bytes).context("command file is not valid UTF-8")
}

fn repository_root(workspace: &Path) -> &Path {
    workspace
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(workspace)
}

fn expand_home(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return BaseDirs::new()
            .map(|base| base.home_dir().to_path_buf())
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(suffix) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        && let Some(base) = BaseDirs::new()
    {
        return base.home_dir().join(suffix);
    }
    path.to_path_buf()
}

fn truncate_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit.saturating_sub(1);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CommandsConfig {
        CommandsConfig {
            discover_user: false,
            discover_project: false,
            compatibility_directories: false,
            max_file_bytes: 16 * 1_024,
            ..Default::default()
        }
    }

    #[test]
    fn discovers_and_expands_prompt_commands() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("commands");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("review.md"),
            "---\ndescription: Review a target\nargument-hint: \"<target> [focus]\"\n---\nReview $1. Focus on ${@:2}. All: $ARGUMENTS. Mode: ${3:-carefully}.",
        )
        .unwrap();
        let catalog = CommandCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root,
                scope: CommandScope::Project,
                priority: 1,
            }],
            &config(),
        )
        .unwrap();
        assert_eq!(catalog.len(), 1);
        let command = &catalog.commands()[0];
        assert_eq!(command.description, "Review a target");
        assert_eq!(command.argument_hint.as_deref(), Some("<target> [focus]"));
        assert_eq!(
            catalog
                .expand("review", "src \"error paths\" strict")
                .unwrap(),
            "Review src. Focus on error paths strict. All: src error paths strict. Mode: strict."
        );
    }

    #[test]
    fn project_and_explicit_commands_override_user_commands() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join("review.md"), "user").unwrap();
        std::fs::write(project.join("review.md"), "project").unwrap();
        let catalog = CommandCatalog::discover_roots(
            &[
                DiscoveryRoot {
                    path: user,
                    scope: CommandScope::User,
                    priority: 1,
                },
                DiscoveryRoot {
                    path: project,
                    scope: CommandScope::Project,
                    priority: 2,
                },
            ],
            &config(),
        )
        .unwrap();
        assert_eq!(catalog.expand("review", "").unwrap(), "project");
        assert_eq!(catalog.commands()[0].scope, CommandScope::Project);
    }

    #[test]
    fn invalid_reserved_and_oversized_commands_are_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("commands");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("help.md"), "shadow built-in").unwrap();
        std::fs::write(root.join("Bad Name.md"), "invalid").unwrap();
        std::fs::write(root.join("large.md"), "x".repeat(20_000)).unwrap();
        let catalog = CommandCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root,
                scope: CommandScope::Project,
                priority: 1,
            }],
            &config(),
        )
        .unwrap();
        assert!(catalog.is_empty());
        assert_eq!(catalog.diagnostics().len(), 3);
    }

    #[test]
    fn defaults_and_slices_match_prompt_template_conventions() {
        let arguments = parse_arguments("one \"two words\" three four");
        assert_eq!(arguments, vec!["one", "two words", "three", "four"]);
        assert_eq!(
            substitute_arguments(
                "$1|$2|$9|$@|${4:-fallback}|${9:-fallback}|${@:2}|${@:2:2}",
                &arguments
            )
            .unwrap(),
            "one|two words||one two words three four|four|fallback|two words three four|two words three"
        );
    }
}
