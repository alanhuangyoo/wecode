use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Take};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use directories::BaseDirs;

use crate::config::{SkillsConfig, wecode_home_dir};
use crate::executor::ExecutionResult;

const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 1_024;
const MAX_DISCOVERY_DEPTH: usize = 8;
const MAX_WALKED_DIRECTORIES: usize = 10_000;
const DEFAULT_READ_LINES: usize = 500;
const MAX_READ_LINES: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_LISTED_FILES: usize = 100;
const MAX_LIST_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
    Explicit,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    pub base_directory: PathBuf,
    pub scope: SkillScope,
    pub disable_model_invocation: bool,
}

#[derive(Clone, Debug)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct SkillCatalog {
    inner: Arc<SkillCatalogInner>,
}

#[derive(Default)]
struct SkillCatalogInner {
    skills: BTreeMap<String, Skill>,
    diagnostics: Vec<SkillDiagnostic>,
    max_file_bytes: usize,
}

#[derive(Clone)]
struct DiscoveryRoot {
    path: PathBuf,
    scope: SkillScope,
    priority: usize,
}

impl SkillCatalog {
    pub fn discover(workspace: &Path, config: &SkillsConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self::default());
        }
        let roots = discovery_roots(workspace, config);
        Self::discover_roots(&roots, config)
    }

    fn discover_roots(roots: &[DiscoveryRoot], config: &SkillsConfig) -> Result<Self> {
        let mut selected: BTreeMap<String, (usize, Skill)> = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let mut seen_files = BTreeSet::new();
        for root in roots {
            let files = match discover_skill_files(&root.path) {
                Ok(files) => files,
                Err(error) => {
                    diagnostics.push(SkillDiagnostic {
                        path: root.path.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            for file in files {
                let canonical = match file.canonicalize() {
                    Ok(canonical) => canonical,
                    Err(error) => {
                        diagnostics.push(SkillDiagnostic {
                            path: file,
                            message: format!("failed to resolve skill: {error}"),
                        });
                        continue;
                    }
                };
                if !seen_files.insert(canonical.clone()) {
                    continue;
                }
                let parsed = parse_skill(&canonical, root.scope, config.max_file_bytes);
                let skill = match parsed {
                    Ok((skill, warnings)) => {
                        diagnostics.extend(warnings.into_iter().map(|message| SkillDiagnostic {
                            path: canonical.clone(),
                            message,
                        }));
                        skill
                    }
                    Err(error) => {
                        diagnostics.push(SkillDiagnostic {
                            path: canonical,
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if let Some((previous_priority, previous)) = selected.get(&skill.name) {
                    diagnostics.push(SkillDiagnostic {
                        path: skill.file.clone(),
                        message: if root.priority > *previous_priority {
                            format!(
                                "skill {:?} overrides {}",
                                skill.name,
                                previous.file.display()
                            )
                        } else {
                            format!(
                                "duplicate skill {:?} ignored; {} has equal or higher precedence",
                                skill.name,
                                previous.file.display()
                            )
                        },
                    });
                    if root.priority <= *previous_priority {
                        continue;
                    }
                }
                selected.insert(skill.name.clone(), (root.priority, skill));
            }
        }
        let omitted = selected.len().saturating_sub(config.max_skills);
        let mut ranked = selected.into_iter().collect::<Vec<_>>();
        ranked.sort_by(
            |(left_name, (left_priority, _)), (right_name, (right_priority, _))| {
                right_priority
                    .cmp(left_priority)
                    .then_with(|| left_name.cmp(right_name))
            },
        );
        ranked.truncate(config.max_skills);
        let skills = ranked
            .into_iter()
            .map(|(name, (_, skill))| (name, skill))
            .collect::<BTreeMap<_, _>>();
        if omitted > 0 {
            diagnostics.push(SkillDiagnostic {
                path: PathBuf::from("<catalog>"),
                message: format!(
                    "{omitted} skills omitted by the {}-skill catalog limit",
                    config.max_skills
                ),
            });
        }
        Ok(Self {
            inner: Arc::new(SkillCatalogInner {
                skills,
                diagnostics,
                max_file_bytes: config.max_file_bytes,
            }),
        })
    }

    pub fn skills(&self) -> Vec<Skill> {
        self.inner.skills.values().cloned().collect()
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.inner.diagnostics
    }

    pub fn len(&self) -> usize {
        self.inner.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.skills.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.inner.skills.contains_key(name)
    }

    pub fn system_prompt(&self) -> String {
        let visible = self
            .inner
            .skills
            .values()
            .filter(|skill| !skill.disable_model_invocation)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return String::new();
        }
        let mut output = String::from(
            "\n\nSkills provide specialized instructions and resources. When a task matches a skill description, call load_skill before acting. Only metadata is listed here; load the full instructions progressively. Resolve referenced paths relative to the reported skill base directory.\n<available_skills>\n",
        );
        for skill in visible {
            output.push_str("  <skill>\n    <name>");
            output.push_str(&escape_xml(&skill.name));
            output.push_str("</name>\n    <description>");
            output.push_str(&escape_xml(&skill.description));
            output.push_str("</description>\n  </skill>\n");
        }
        output.push_str("</available_skills>\n");
        output
    }

    pub fn explicit_request(&self, name: &str, arguments: &str) -> Result<String> {
        if !self.contains(name) {
            bail!("unknown skill {name:?}");
        }
        let mut request = format!(
            "The user explicitly invoked skill {name:?}. You must call load_skill with name {name:?} before taking any other action."
        );
        if !arguments.trim().is_empty() {
            request.push_str("\nUser arguments:\n");
            request.push_str(arguments.trim());
        }
        Ok(request)
    }

    pub async fn read(
        &self,
        name: &str,
        path: Option<&str>,
        offset: Option<usize>,
        limit: Option<usize>,
        max_output_bytes: usize,
    ) -> Result<ExecutionResult> {
        let catalog = self.clone();
        let name = name.to_owned();
        let path = path.map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || {
            catalog.read_blocking(&name, path.as_deref(), offset, limit, max_output_bytes)
        })
        .await
        .context("skill reader task stopped")?
    }

    fn read_blocking(
        &self,
        name: &str,
        path: Option<&str>,
        offset: Option<usize>,
        limit: Option<usize>,
        max_output_bytes: usize,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        let skill = self
            .inner
            .skills
            .get(name)
            .with_context(|| format!("unknown skill {name:?}"))?;
        let relative = path.unwrap_or("SKILL.md");
        let normalized = validate_relative_path(relative)?;
        let candidate = skill.base_directory.join(&normalized);
        let resolved = candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve skill resource {relative:?}"))?;
        if !resolved.starts_with(&skill.base_directory) {
            bail!("skill resource path escapes its base directory");
        }
        let metadata = std::fs::metadata(&resolved)
            .with_context(|| format!("failed to inspect skill resource {}", resolved.display()))?;
        if !metadata.is_file() {
            bail!("skill resource {relative:?} is not a file");
        }
        if metadata.len() > self.inner.max_file_bytes as u64 {
            bail!(
                "skill resource is {} bytes; the configured limit is {}",
                metadata.len(),
                self.inner.max_file_bytes
            );
        }
        let bytes = std::fs::read(&resolved)
            .with_context(|| format!("failed to read skill resource {}", resolved.display()))?;
        if bytes.contains(&0) {
            bail!("skill resource appears to be binary");
        }
        let content = std::str::from_utf8(&bytes).context("skill resource is not valid UTF-8")?;
        let lines = content.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start = offset.unwrap_or(1);
        if start == 0 || (total_lines > 0 && start > total_lines) {
            bail!("skill resource offset {start} is outside the file");
        }
        let requested = limit.unwrap_or(DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES);
        let budget = max_output_bytes.max(4_096);
        let mut output = String::new();
        if normalized == Path::new("SKILL.md") {
            output.push_str(&format!(
                "LOADED SKILL: {}\ndescription: {}\nscope: {}\nbase directory: {}\n",
                skill.name,
                skill.description,
                skill.scope.as_str(),
                skill.base_directory.display()
            ));
            let files = list_skill_files(&skill.base_directory);
            if !files.is_empty() {
                output.push_str("available files:\n");
                for file in files {
                    output.push_str("- ");
                    output.push_str(&file);
                    output.push('\n');
                }
            }
        } else {
            output.push_str(&format!(
                "SKILL RESOURCE: {} / {}\nbase directory: {}\n",
                skill.name,
                normalized.display(),
                skill.base_directory.display()
            ));
        }
        output.push_str(&format!(
            "lines: {}-{} of {total_lines}\n",
            start,
            start.saturating_sub(1)
        ));
        let mut shown = 0_usize;
        let mut omitted = 0_usize;
        for (index, line) in lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(requested)
        {
            let rendered = format!(
                "{:>6}\t{}\n",
                index + 1,
                truncate_chars(line, MAX_LINE_CHARS)
            );
            if output.len().saturating_add(rendered.len()) > budget {
                omitted = omitted.saturating_add(rendered.len());
                omitted = omitted.saturating_add(
                    lines[index + 1..]
                        .iter()
                        .map(|line| line.len().saturating_add(1))
                        .sum::<usize>(),
                );
                break;
            }
            output.push_str(&rendered);
            shown += 1;
        }
        let end = start.saturating_add(shown).saturating_sub(1);
        let more = end < total_lines;
        if more {
            let notice = format!(
                "[More skill content available. Continue with load_skill name={name:?} path={relative:?} offset={}]\n",
                end.saturating_add(1)
            );
            if output.len().saturating_add(notice.len()) <= budget {
                output.push_str(&notice);
            } else {
                omitted = omitted.saturating_add(notice.len());
            }
        }
        let old_range = format!("lines: {}-{} of", start, start.saturating_sub(1));
        output = output.replacen(&old_range, &format!("lines: {start}-{end} of"), 1);
        if output.len() > budget {
            let (bounded, additionally_omitted) = truncate_middle(&output, budget);
            output = bounded;
            omitted = omitted.saturating_add(additionally_omitted);
        }
        Ok(ExecutionResult {
            exit_code: Some(0),
            stdout: output,
            stderr: String::new(),
            duration_ms: started.elapsed().as_millis(),
            timed_out: false,
            truncated_bytes: omitted,
        })
    }
}

fn discovery_roots(workspace: &Path, config: &SkillsConfig) -> Vec<DiscoveryRoot> {
    let mut paths = Vec::new();
    let mut priority = 0_usize;
    let mut push = |path: PathBuf, scope| {
        priority = priority.saturating_add(1);
        paths.push(DiscoveryRoot {
            path,
            scope,
            priority,
        });
    };
    if config.discover_user {
        if config.compatibility_directories
            && let Some(base) = BaseDirs::new()
        {
            push(base.home_dir().join(".codex/skills"), SkillScope::User);
            push(base.home_dir().join(".claude/skills"), SkillScope::User);
            push(base.home_dir().join(".agents/skills"), SkillScope::User);
        }
        push(wecode_home_dir().join("skills"), SkillScope::User);
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
                push(directory.join(".codex/skills"), SkillScope::Project);
                push(directory.join(".claude/skills"), SkillScope::Project);
                push(directory.join(".agents/skills"), SkillScope::Project);
            }
            push(directory.join(".wecode/skills"), SkillScope::Project);
        }
    }
    for path in &config.paths {
        let expanded = expand_home(path);
        push(
            if expanded.is_absolute() {
                expanded
            } else {
                workspace.join(expanded)
            },
            SkillScope::Explicit,
        );
    }
    paths
}

fn discover_skill_files(root: &Path) -> Result<Vec<PathBuf>> {
    if root.is_file() {
        return Ok((root.file_name().is_some_and(|name| name == "SKILL.md"))
            .then(|| root.to_path_buf())
            .into_iter()
            .collect());
    }
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut walked = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        walked = walked.saturating_add(1);
        if walked > MAX_WALKED_DIRECTORIES {
            bail!(
                "skill discovery under {} exceeded {MAX_WALKED_DIRECTORIES} directories",
                root.display()
            );
        }
        let skill_file = directory.join("SKILL.md");
        if skill_file.is_file() {
            files.push(skill_file);
            continue;
        }
        if depth >= MAX_DISCOVERY_DEPTH {
            continue;
        }
        let mut entries = std::fs::read_dir(&directory)
            .with_context(|| format!("failed to scan skill directory {}", directory.display()))?
            .filter_map(|entry| entry.ok())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.')
                || matches!(name.to_str(), Some("node_modules" | "target"))
            {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn parse_skill(
    path: &Path,
    scope: SkillScope,
    max_file_bytes: usize,
) -> Result<(Skill, Vec<String>)> {
    let raw = read_bounded(path, max_file_bytes)?;
    let (frontmatter, _) = split_frontmatter(&raw)?;
    let fields = crate::frontmatter::parse_string_fields(frontmatter)?;
    let name = fields
        .get("name")
        .map(String::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    validate_name(&name)?;
    let description = fields
        .get("description")
        .map(String::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if description.is_empty() {
        bail!("skill description is required");
    }
    let mut warnings = Vec::new();
    let description = if description.len() > MAX_DESCRIPTION_BYTES {
        warnings.push(format!(
            "description exceeded {MAX_DESCRIPTION_BYTES} bytes and was truncated"
        ));
        truncate_bytes(&description, MAX_DESCRIPTION_BYTES)
    } else {
        description
    };
    let disable_model_invocation = fields
        .get("disable-model-invocation")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let base_directory = path
        .parent()
        .context("skill file has no parent directory")?
        .canonicalize()
        .context("failed to resolve skill base directory")?;
    Ok((
        Skill {
            name,
            description,
            file: path.to_path_buf(),
            base_directory,
            scope,
            disable_model_invocation,
        },
        warnings,
    ))
}

fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut offset = 0_usize;
    let mut lines = raw.split_inclusive('\n');
    let first = lines.next().context("skill file is empty")?;
    offset += first.len();
    if first.trim_end_matches(['\r', '\n']) != "---" {
        bail!("skill file must begin with YAML frontmatter");
    }
    for line in lines {
        let start = offset;
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((&raw[first.len()..start], &raw[offset..]));
        }
    }
    bail!("skill frontmatter is missing its closing delimiter")
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!(
            "skill name must be 1-64 lowercase ASCII letters, digits, or hyphens without leading, trailing, or consecutive hyphens"
        );
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() {
        bail!("skill resource path cannot be empty");
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("skill resource path must stay inside the skill directory");
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            Component::CurDir => None,
            _ => None,
        })
        .collect())
}

fn list_skill_files(base: &Path) -> Vec<String> {
    let mut output = Vec::new();
    let mut pending = vec![(base.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_LIST_DEPTH || output.len() >= MAX_LISTED_FILES {
            continue;
        }
        let mut entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(_) => continue,
        };
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            if output.len() >= MAX_LISTED_FILES {
                break;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if file_type.is_file()
                && let Ok(relative) = entry.path().strip_prefix(base)
            {
                output.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    output.sort();
    output
}

fn read_bounded(path: &Path, limit: usize) -> Result<String> {
    let file =
        File::open(path).with_context(|| format!("failed to read skill {}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(8 * 1_024));
    let mut reader: Take<File> = file.take(limit.saturating_add(1) as u64);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("skill file exceeds the configured {limit}-byte limit");
    }
    String::from_utf8(bytes).context("skill file is not valid UTF-8")
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

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn truncate_middle(value: &str, limit: usize) -> (String, usize) {
    if value.len() <= limit {
        return (value.to_owned(), 0);
    }
    let marker = "\n... skill output truncated ...\n";
    let available = limit.saturating_sub(marker.len());
    let mut head = available / 2;
    while head > 0 && !value.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = value.len().saturating_sub(available.saturating_sub(head));
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    (
        format!("{}{marker}{}", &value[..head], &value[tail_start..]),
        tail_start.saturating_sub(head),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SkillsConfig {
        SkillsConfig {
            discover_user: false,
            discover_project: false,
            compatibility_directories: false,
            max_file_bytes: 16 * 1_024,
            ..Default::default()
        }
    }

    #[test]
    fn discovers_frontmatter_and_formats_progressive_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        std::fs::create_dir_all(root.join("review/references")).unwrap();
        std::fs::write(
            root.join("review/SKILL.md"),
            "---\nname: code-review\ndescription: >\n  Review code for correctness.\n  Use for pull requests.\n---\n# Review\n",
        )
        .unwrap();
        std::fs::write(root.join("review/references/checklist.md"), "checklist").unwrap();
        let catalog = SkillCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root,
                scope: SkillScope::Project,
                priority: 1,
            }],
            &config(),
        )
        .unwrap();
        assert_eq!(catalog.len(), 1);
        let skill = &catalog.skills()[0];
        assert_eq!(skill.name, "code-review");
        assert_eq!(
            skill.description,
            "Review code for correctness. Use for pull requests."
        );
        let prompt = catalog.system_prompt();
        assert!(prompt.contains("<name>code-review</name>"));
        assert!(!prompt.contains("# Review"));
    }

    #[tokio::test]
    async fn loads_skill_and_bounded_relative_resources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        std::fs::create_dir_all(root.join("review/references")).unwrap();
        std::fs::write(
            root.join("review/SKILL.md"),
            "---\nname: review\ndescription: Review code.\n---\n# Instructions\n",
        )
        .unwrap();
        std::fs::write(
            root.join("review/references/checklist.md"),
            "first\nsecond\nthird\n",
        )
        .unwrap();
        let catalog = SkillCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root,
                scope: SkillScope::Project,
                priority: 1,
            }],
            &config(),
        )
        .unwrap();
        let loaded = catalog
            .read("review", None, None, None, 8 * 1_024)
            .await
            .unwrap();
        assert!(loaded.stdout.contains("LOADED SKILL: review"));
        assert!(loaded.stdout.contains("references/checklist.md"));
        let resource = catalog
            .read(
                "review",
                Some("references/checklist.md"),
                Some(2),
                Some(1),
                8 * 1_024,
            )
            .await
            .unwrap();
        assert!(resource.stdout.contains("     2\tsecond"));
        assert!(resource.stdout.contains("offset=3"));
        assert!(
            catalog
                .read("review", Some("../outside"), None, None, 8 * 1_024)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn skill_observations_respect_the_hard_output_budget() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills/big");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            format!(
                "---\nname: big\ndescription: Large skill.\n---\n{}",
                "instruction line\n".repeat(2_000)
            ),
        )
        .unwrap();
        for index in 0..100 {
            std::fs::write(
                root.join(format!("{index:03}-{}.md", "x".repeat(120))),
                "resource",
            )
            .unwrap();
        }
        let catalog = SkillCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root.parent().unwrap().to_path_buf(),
                scope: SkillScope::Project,
                priority: 1,
            }],
            &SkillsConfig {
                max_file_bytes: 64 * 1_024,
                ..config()
            },
        )
        .unwrap();
        let result = catalog
            .read("big", None, None, None, 4 * 1_024)
            .await
            .unwrap();
        assert!(result.stdout.len() <= 4 * 1_024);
        assert!(result.truncated_bytes > 0);
        assert!(result.stdout.contains("skill output truncated"));
    }

    #[test]
    fn higher_precedence_roots_override_collisions_deterministically() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user/review");
        let project = temp.path().join("project/review");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            user.join("SKILL.md"),
            "---\nname: review\ndescription: User review.\n---\n",
        )
        .unwrap();
        std::fs::write(
            project.join("SKILL.md"),
            "---\nname: review\ndescription: Project review.\n---\n",
        )
        .unwrap();
        let catalog = SkillCatalog::discover_roots(
            &[
                DiscoveryRoot {
                    path: user.parent().unwrap().to_path_buf(),
                    scope: SkillScope::User,
                    priority: 1,
                },
                DiscoveryRoot {
                    path: project.parent().unwrap().to_path_buf(),
                    scope: SkillScope::Project,
                    priority: 2,
                },
            ],
            &config(),
        )
        .unwrap();
        assert_eq!(catalog.skills()[0].description, "Project review.");
        assert_eq!(catalog.diagnostics().len(), 1);
    }

    #[test]
    fn same_physical_skill_discovered_through_two_scopes_is_not_a_duplicate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills/review");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            "---\nname: review\ndescription: Review code.\n---\n",
        )
        .unwrap();
        let parent = root.parent().unwrap().to_path_buf();
        let catalog = SkillCatalog::discover_roots(
            &[
                DiscoveryRoot {
                    path: parent.clone(),
                    scope: SkillScope::User,
                    priority: 1,
                },
                DiscoveryRoot {
                    path: parent,
                    scope: SkillScope::Project,
                    priority: 2,
                },
            ],
            &config(),
        )
        .unwrap();

        assert_eq!(catalog.len(), 1);
        assert!(catalog.diagnostics().is_empty());
    }

    #[test]
    fn hidden_and_invalid_skills_are_handled_safely() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        std::fs::create_dir_all(root.join("hidden")).unwrap();
        std::fs::create_dir_all(root.join("invalid")).unwrap();
        std::fs::write(
            root.join("hidden/SKILL.md"),
            "---\nname: hidden\ndescription: Explicit only.\ndisable-model-invocation: true\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("invalid/SKILL.md"),
            "---\nname: Bad_Name\ndescription: Invalid.\n---\n",
        )
        .unwrap();
        let catalog = SkillCatalog::discover_roots(
            &[DiscoveryRoot {
                path: root,
                scope: SkillScope::Project,
                priority: 1,
            }],
            &config(),
        )
        .unwrap();
        assert!(catalog.contains("hidden"));
        assert!(!catalog.system_prompt().contains("<name>hidden</name>"));
        assert_eq!(catalog.diagnostics().len(), 1);
    }
}
