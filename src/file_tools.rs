use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use ignore::{DirEntry, WalkBuilder};
use regex::{Regex, RegexBuilder};

use crate::executor::ExecutionResult;

const DEFAULT_READ_LINES: usize = 400;
const MAX_READ_LINES: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_SEARCH_LINE_CHARS: usize = 600;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_LIST_LIMIT: usize = 200;
const MAX_LIST_LIMIT: usize = 1_000;
const DEFAULT_SEARCH_LIMIT: usize = 100;
const MAX_SEARCH_LIMIT: usize = 500;
const MAX_WALKED_FILES: usize = 50_000;

#[derive(Clone, Debug)]
pub struct FileTools {
    workspace: PathBuf,
    max_output_bytes: usize,
}

impl FileTools {
    pub fn new(workspace: PathBuf, max_output_bytes: usize) -> Self {
        Self {
            workspace,
            max_output_bytes: max_output_bytes.max(4_096),
        }
    }

    pub async fn read_file(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let this = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || this.read_file_blocking(&path, offset, limit))
            .await
            .context("read_file task stopped")?
    }

    pub async fn list_files(
        &self,
        path: &str,
        depth: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let this = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || this.list_files_blocking(&path, depth, limit))
            .await
            .context("list_files task stopped")?
    }

    pub async fn glob(
        &self,
        pattern: &str,
        path: &str,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let this = self.clone();
        let pattern = pattern.to_owned();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || this.glob_blocking(&pattern, &path, limit))
            .await
            .context("glob task stopped")?
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn grep(
        &self,
        pattern: &str,
        path: &str,
        glob: Option<&str>,
        literal: bool,
        ignore_case: bool,
        context: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let this = self.clone();
        let pattern = pattern.to_owned();
        let path = path.to_owned();
        let glob = glob.map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || {
            this.grep_blocking(
                &pattern,
                &path,
                glob.as_deref(),
                literal,
                ignore_case,
                context,
                limit,
            )
        })
        .await
        .context("grep task stopped")?
    }

    fn read_file_blocking(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        let resolved = self.resolve_existing(path)?;
        let metadata = fs::metadata(&resolved).with_context(|| {
            format!(
                "failed to inspect {}",
                display_path(&resolved, &self.workspace)
            )
        })?;
        if !metadata.is_file() {
            bail!("{path:?} is not a file; use list_files for directories");
        }
        if metadata.len() > MAX_FILE_BYTES {
            bail!(
                "{} is {:.1} MiB; read_file is limited to {} MiB. Use grep or a scoped shell command",
                display_path(&resolved, &self.workspace),
                metadata.len() as f64 / 1_048_576.0,
                MAX_FILE_BYTES / 1_048_576
            );
        }
        let bytes = fs::read(&resolved).with_context(|| {
            format!(
                "failed to read {}",
                display_path(&resolved, &self.workspace)
            )
        })?;
        if is_binary(&bytes) {
            bail!(
                "{} appears to be binary; read_file currently supports UTF-8 text",
                display_path(&resolved, &self.workspace)
            );
        }
        let content = std::str::from_utf8(&bytes).with_context(|| {
            format!(
                "{} is not valid UTF-8",
                display_path(&resolved, &self.workspace)
            )
        })?;
        let lines = content.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start = offset.unwrap_or(1);
        if start == 0 {
            bail!("offset is 1-indexed and must be at least 1");
        }
        if total_lines > 0 && start > total_lines {
            bail!("offset {start} is beyond the end of the file ({total_lines} lines)");
        }
        if total_lines == 0 && start > 1 {
            bail!("offset {start} is beyond the end of the empty file");
        }
        let requested = limit.unwrap_or(DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES);
        let relative = display_path(&resolved, &self.workspace);
        let mut builder = OutputBuilder::new(self.max_output_bytes);
        builder.push_required(&format!(
            "file: {relative}\nlines: {}-{} of {total_lines}\n",
            start,
            start.saturating_sub(1)
        ));

        let mut shown = 0usize;
        let mut next_offset = None;
        for (index, line) in lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(requested)
        {
            let line_number = index + 1;
            let rendered = format!("{line_number:>6}\t{}", truncate_chars(line, MAX_LINE_CHARS));
            if !builder.push_line(&rendered) {
                next_offset = Some(line_number);
                break;
            }
            shown += 1;
        }
        let end = start.saturating_add(shown).saturating_sub(1);
        let more_by_limit = end < total_lines && shown >= requested;
        if next_offset.is_none() && more_by_limit {
            next_offset = Some(end + 1);
        }
        if let Some(next) = next_offset {
            builder.push_notice(&format!(
                "[More lines available. Continue with read_file path={relative:?} offset={next}]"
            ));
        }
        let text = builder.finish().replacen(
            &format!("lines: {}-{} of", start, start.saturating_sub(1)),
            &format!("lines: {start}-{end} of"),
            1,
        );
        Ok(success_result(started, text, builder.omitted_bytes))
    }

    fn list_files_blocking(
        &self,
        path: &str,
        depth: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        let root = self.resolve_existing(default_dot(path))?;
        if !root.is_dir() {
            bail!("{path:?} is not a directory; use read_file for files");
        }
        let depth = depth.unwrap_or(2).clamp(1, 8);
        let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
        let walk = walk_entries(&root, Some(depth), true)?;
        let traversal_truncated = walk.truncated;
        let mut entries = walk
            .entries
            .into_iter()
            .map(|entry| {
                let mut value = display_path(&entry.path, &self.workspace);
                if entry.is_dir {
                    value.push('/');
                }
                value
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries.dedup();
        let total = entries.len();
        let relative = display_path(&root, &self.workspace);
        let mut builder = OutputBuilder::new(self.max_output_bytes);
        builder.push_required(&format!(
            "directory: {relative}\ndepth: {depth}\nentries: {total}\n"
        ));
        let mut shown = 0usize;
        for entry in entries.iter().take(limit) {
            if !builder.push_line(entry) {
                break;
            }
            shown += 1;
        }
        if shown < total || traversal_truncated {
            builder.push_notice(&format!(
                "[Showing {shown} of {total} collected entries{}. Narrow path/depth or increase limit up to {MAX_LIST_LIMIT}.]",
                if traversal_truncated {
                    format!("; traversal stopped at {MAX_WALKED_FILES} items")
                } else {
                    String::new()
                }
            ));
            builder.omitted_bytes = builder
                .omitted_bytes
                .saturating_add(entries[shown..].iter().map(String::len).sum::<usize>());
            if traversal_truncated {
                builder.omitted_bytes = builder.omitted_bytes.saturating_add(1);
            }
        }
        Ok(success_result(
            started,
            builder.finish(),
            builder.omitted_bytes,
        ))
    }

    fn glob_blocking(
        &self,
        pattern: &str,
        path: &str,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        if pattern.trim().is_empty() {
            bail!("glob pattern cannot be empty");
        }
        let root = self.resolve_existing(default_dot(path))?;
        if !root.is_dir() {
            bail!("glob search path {path:?} is not a directory");
        }
        let matcher = glob_matcher(pattern)?;
        let basename_only = !pattern.contains('/') && !pattern.contains('\\');
        let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
        let mut matches = Vec::new();
        let walk = walk_entries(&root, None, false)?;
        let traversal_truncated = walk.truncated;
        for entry in walk.entries {
            let relative_to_root = entry
                .path
                .strip_prefix(&root)
                .unwrap_or(&entry.path)
                .to_string_lossy()
                .replace('\\', "/");
            let matched = matcher.is_match(&relative_to_root)
                || (basename_only
                    && entry
                        .path
                        .file_name()
                        .is_some_and(|name| matcher.is_match(name)));
            if matched {
                matches.push(display_path(&entry.path, &self.workspace));
            }
        }
        matches.sort();
        matches.dedup();
        let total = matches.len();
        let mut builder = OutputBuilder::new(self.max_output_bytes);
        builder.push_required(&format!(
            "glob: {pattern}\npath: {}\nmatches: {total}\n",
            display_path(&root, &self.workspace)
        ));
        let mut shown = 0usize;
        for matched in matches.iter().take(limit) {
            if !builder.push_line(matched) {
                break;
            }
            shown += 1;
        }
        if shown < total || traversal_truncated {
            builder.push_notice(&format!(
                "[Results truncated: showing {shown} of {total} collected matches{}. Narrow the pattern or path.]",
                if traversal_truncated {
                    format!("; traversal stopped at {MAX_WALKED_FILES} files")
                } else {
                    String::new()
                }
            ));
            builder.omitted_bytes = builder
                .omitted_bytes
                .saturating_add(matches[shown..].iter().map(String::len).sum::<usize>());
            if traversal_truncated {
                builder.omitted_bytes = builder.omitted_bytes.saturating_add(1);
            }
        }
        Ok(success_result(
            started,
            builder.finish(),
            builder.omitted_bytes,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn grep_blocking(
        &self,
        pattern: &str,
        path: &str,
        glob: Option<&str>,
        literal: bool,
        ignore_case: bool,
        context_lines: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        if pattern.is_empty() {
            bail!("grep pattern cannot be empty");
        }
        let root = self.resolve_existing(default_dot(path))?;
        let regex = build_regex(pattern, literal, ignore_case)?;
        let glob_matcher = glob.map(glob_matcher).transpose()?;
        let glob_basename_only = glob.is_some_and(|value| !value.contains(['/', '\\']));
        let context_lines = context_lines.unwrap_or(0).min(5);
        let limit = limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        let (files, traversal_truncated) = if root.is_file() {
            (vec![root.clone()], false)
        } else {
            let walk = walk_entries(&root, None, false)?;
            (
                walk.entries.into_iter().map(|entry| entry.path).collect(),
                walk.truncated,
            )
        };

        let mut builder = OutputBuilder::new(self.max_output_bytes);
        builder.push_required(&format!(
            "pattern: {pattern}\npath: {}\n",
            display_path(&root, &self.workspace)
        ));
        let mut matches = 0usize;
        let mut searched_files = 0usize;
        let mut output_stopped = false;
        'files: for file in files {
            if !file.is_file() {
                continue;
            }
            let relative_to_root = file
                .strip_prefix(if root.is_file() {
                    root.parent().unwrap_or(&self.workspace)
                } else {
                    &root
                })
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(matcher) = &glob_matcher {
                let matched = matcher.is_match(&relative_to_root)
                    || (glob_basename_only
                        && file.file_name().is_some_and(|name| matcher.is_match(name)));
                if !matched {
                    continue;
                }
            }
            let metadata = match fs::metadata(&file) {
                Ok(metadata) if metadata.len() <= MAX_FILE_BYTES => metadata,
                _ => continue,
            };
            if !metadata.is_file() {
                continue;
            }
            let bytes = match fs::read(&file) {
                Ok(bytes) if !is_binary(&bytes) => bytes,
                _ => continue,
            };
            let Ok(content) = std::str::from_utf8(&bytes) else {
                continue;
            };
            searched_files += 1;
            let lines = content.lines().collect::<Vec<_>>();
            let relative = display_path(&file, &self.workspace);
            let mut emitted_lines = HashSet::new();
            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                matches += 1;
                let from = index.saturating_sub(context_lines);
                let to = (index + context_lines).min(lines.len().saturating_sub(1));
                for (line_index, context_line) in lines.iter().enumerate().take(to + 1).skip(from) {
                    if !emitted_lines.insert(line_index) {
                        continue;
                    }
                    let separator = if line_index == index { ':' } else { '-' };
                    let rendered = format!(
                        "{relative}{separator}{}{separator}{}",
                        line_index + 1,
                        truncate_chars(context_line, MAX_SEARCH_LINE_CHARS)
                    );
                    if !builder.push_line(&rendered) {
                        output_stopped = true;
                        break 'files;
                    }
                }
                if matches >= limit {
                    output_stopped = true;
                    break 'files;
                }
            }
        }
        if matches == 0 {
            builder.push_line("No matches found.");
        } else {
            builder.push_notice(&format!(
                "[{matches} matches across {searched_files} searched files{}{}]",
                if output_stopped {
                    "; results truncated"
                } else {
                    ""
                },
                if traversal_truncated {
                    format!("; traversal stopped at {MAX_WALKED_FILES} files")
                } else {
                    String::new()
                },
            ));
        }
        if output_stopped || traversal_truncated {
            builder.omitted_bytes = builder.omitted_bytes.saturating_add(1);
        }
        Ok(success_result(
            started,
            builder.finish(),
            builder.omitted_bytes,
        ))
    }

    fn resolve_existing(&self, input: &str) -> Result<PathBuf> {
        let workspace = self
            .workspace
            .canonicalize()
            .with_context(|| format!("failed to resolve workspace {}", self.workspace.display()))?;
        let candidate = Path::new(input);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            workspace.join(candidate)
        };
        let resolved = match candidate.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) => bail!("{}", missing_path_message(&candidate, input, &workspace)),
        };
        if !resolved.starts_with(&workspace) {
            bail!("path escapes the workspace: {input}");
        }
        if crate::config::wecode_home_dir()
            .canonicalize()
            .is_ok_and(|private| resolved.starts_with(private))
        {
            bail!("path is private WeCode state and cannot be accessed by agent tools: {input}");
        }
        Ok(resolved)
    }
}

fn missing_path_message(candidate: &Path, input: &str, workspace: &Path) -> String {
    let Some(parent) = candidate
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
    else {
        return format!("path does not exist: {input}");
    };
    if !parent.starts_with(workspace) {
        return format!("path does not exist or escapes the workspace: {input}");
    }
    let wanted = candidate
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut suggestions = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            similar_path_name(&wanted, &lower).then_some(name)
        })
        .collect::<Vec<_>>();
    suggestions.sort();
    suggestions.truncate(3);
    if suggestions.is_empty() {
        format!("path does not exist: {input}")
    } else {
        format!(
            "path does not exist: {input}\nDid you mean:\n{}",
            suggestions
                .into_iter()
                .map(|name| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

fn similar_path_name(wanted: &str, candidate: &str) -> bool {
    if wanted.is_empty() || candidate.is_empty() {
        return false;
    }
    if candidate.contains(wanted) || wanted.contains(candidate) {
        return true;
    }

    let wanted_chars = wanted.chars().collect::<Vec<_>>();
    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let shorter_len = wanted_chars.len().min(candidate_chars.len());
    let shared_prefix = wanted_chars
        .iter()
        .zip(&candidate_chars)
        .take_while(|(left, right)| left == right)
        .count();
    if shared_prefix >= 4 && shared_prefix * 2 >= shorter_len {
        return true;
    }

    let max_distance = if shorter_len <= 4 { 1 } else { 2 };
    bounded_levenshtein(&wanted_chars, &candidate_chars, max_distance).is_some()
}

fn bounded_levenshtein(left: &[char], right: &[char], limit: usize) -> Option<usize> {
    if left.len().abs_diff(right.len()) > limit {
        return None;
    }

    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_char) in left.iter().enumerate() {
        current[0] = left_index + 1;
        let mut row_min = current[0];
        for (right_index, right_char) in right.iter().enumerate() {
            let substitution = previous[right_index] + usize::from(left_char != right_char);
            let insertion = current[right_index] + 1;
            let deletion = previous[right_index + 1] + 1;
            current[right_index + 1] = substitution.min(insertion).min(deletion);
            row_min = row_min.min(current[right_index + 1]);
        }
        if row_min > limit {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    (previous[right.len()] <= limit).then_some(previous[right.len()])
}

#[derive(Debug)]
struct WalkedEntry {
    path: PathBuf,
    is_dir: bool,
}

struct WalkResult {
    entries: Vec<WalkedEntry>,
    truncated: bool,
}

fn walk_entries(root: &Path, max_depth: Option<usize>, include_dirs: bool) -> Result<WalkResult> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(not_private_metadata)
        .sort_by_file_path(|left, right| left.cmp(right));
    if let Some(max_depth) = max_depth {
        builder.max_depth(Some(max_depth));
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in builder.build() {
        let entry = item.with_context(|| format!("failed to walk {}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() || (include_dirs && file_type.is_dir()) {
            if entries.len() >= MAX_WALKED_FILES {
                truncated = true;
                break;
            }
            entries.push(WalkedEntry {
                path: entry.into_path(),
                is_dir: file_type.is_dir(),
            });
        }
    }
    Ok(WalkResult { entries, truncated })
}

fn not_private_metadata(entry: &DirEntry) -> bool {
    entry.depth() == 0 || (entry.file_name() != ".git" && entry.file_name() != ".wecode")
}

fn glob_matcher(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(&pattern.replace('\\', "/"))
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .with_context(|| format!("invalid glob pattern {pattern:?}"))?
        .compile_matcher())
}

fn build_regex(pattern: &str, literal: bool, ignore_case: bool) -> Result<Regex> {
    let pattern = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .build()
        .with_context(|| format!("invalid regular expression {pattern:?}"))
}

fn default_dot(path: &str) -> &str {
    if path.trim().is_empty() { "." } else { path }
}

fn display_path(path: &Path, workspace: &Path) -> String {
    let relative = path.strip_prefix(workspace).unwrap_or(path);
    let value = relative.to_string_lossy().replace('\\', "/");
    if value.is_empty() { ".".into() } else { value }
}

fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4_096)];
    if sample.contains(&0) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    let controls = sample
        .iter()
        .filter(|byte| **byte < 9 || (**byte > 13 && **byte < 32))
        .count();
    controls * 10 > sample.len() * 3
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut output = value.chars().take(max).collect::<String>();
    output.push_str(" … [line truncated]");
    output
}

fn success_result(started: Instant, stdout: String, truncated_bytes: usize) -> ExecutionResult {
    ExecutionResult {
        exit_code: Some(0),
        stdout,
        stderr: String::new(),
        duration_ms: started.elapsed().as_millis(),
        timed_out: false,
        truncated_bytes,
    }
}

struct OutputBuilder {
    text: String,
    max_bytes: usize,
    omitted_bytes: usize,
}

impl OutputBuilder {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            omitted_bytes: 0,
        }
    }

    fn push_required(&mut self, value: &str) {
        self.text.push_str(value);
    }

    fn push_line(&mut self, value: &str) -> bool {
        let required = value.len() + 1;
        if self.text.len().saturating_add(required) > self.max_bytes {
            self.omitted_bytes = self.omitted_bytes.saturating_add(required);
            return false;
        }
        self.text.push_str(value);
        self.text.push('\n');
        true
    }

    fn push_notice(&mut self, value: &str) {
        if !self.push_line(value) {
            let notice = if value.len().saturating_add(2) <= self.max_bytes {
                value.to_owned()
            } else {
                truncate_chars(value, self.max_bytes.saturating_div(2))
            };
            let reserve = self
                .max_bytes
                .saturating_sub(notice.len().saturating_add(2));
            self.text.truncate(floor_char_boundary(&self.text, reserve));
            self.text.push('\n');
            self.text.push_str(&notice);
            self.text.push('\n');
        }
    }

    fn finish(&self) -> String {
        self.text.trim_end().to_owned()
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(temp: &tempfile::TempDir) -> FileTools {
        FileTools::new(temp.path().to_path_buf(), 16_000)
    }

    #[tokio::test]
    async fn reads_numbered_ranges_with_continuation() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("sample.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let result = tools(&temp)
            .read_file("sample.txt", Some(2), Some(2))
            .await
            .unwrap();
        assert!(result.stdout.contains("lines: 2-3 of 4"));
        assert!(result.stdout.contains("     2\ttwo"));
        assert!(result.stdout.contains("offset=4"));
    }

    #[tokio::test]
    async fn rejects_parent_and_symlink_workspace_escape() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            tools(&temp)
                .read_file("../outside", None, None)
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", temp.path().join("escape")).unwrap();
            assert!(tools(&temp).read_file("escape", None, None).await.is_err());
        }
    }

    #[tokio::test]
    async fn lists_and_globs_deterministically_while_respecting_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        fs::write(temp.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        fs::write(temp.path().join("src/nested/b.rs"), "fn b() {}\n").unwrap();
        fs::write(temp.path().join("ignored.rs"), "ignored\n").unwrap();
        fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        let tools = tools(&temp);
        let listed = tools.list_files(".", Some(3), None).await.unwrap();
        assert!(listed.stdout.contains("src/a.rs"));
        assert!(!listed.stdout.contains("ignored.rs"));
        let found = tools.glob("**/*.rs", ".", None).await.unwrap();
        assert!(found.stdout.contains("src/a.rs"));
        assert!(found.stdout.contains("src/nested/b.rs"));
        assert!(!found.stdout.contains("ignored.rs"));
    }

    #[tokio::test]
    async fn grep_supports_regex_literal_context_and_glob_filters() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("a.rs"),
            "before\nfn Alpha() {}\nafter\nliteral [x]\n",
        )
        .unwrap();
        fs::write(temp.path().join("a.txt"), "fn ignored() {}\n").unwrap();
        let tools = tools(&temp);
        let regex = tools
            .grep("alpha", ".", Some("*.rs"), false, true, Some(1), None)
            .await
            .unwrap();
        assert!(regex.stdout.contains("a.rs:2:fn Alpha() {}"));
        assert!(regex.stdout.contains("a.rs-1-before"));
        assert!(!regex.stdout.contains("a.txt"));
        let literal = tools
            .grep("[x]", ".", None, true, false, None, None)
            .await
            .unwrap();
        assert!(literal.stdout.contains("literal [x]"));
    }

    #[tokio::test]
    async fn output_budget_is_hard_bounded_and_reports_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let content = (1..=1_000)
            .map(|index| format!("line {index} {}", "x".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(temp.path().join("large.txt"), content).unwrap();
        let tools = FileTools::new(temp.path().to_path_buf(), 4_096);
        let result = tools
            .read_file("large.txt", Some(1), Some(1_000))
            .await
            .unwrap();
        assert!(result.stdout.len() <= 4_096);
        assert!(result.stdout.contains("Continue with read_file"));
        assert!(result.truncated_bytes > 0);
    }

    #[tokio::test]
    async fn binary_files_and_invalid_regexes_are_recoverable_tool_errors() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("binary.bin"), [0, 1, 2, 3]).unwrap();
        fs::write(temp.path().join("configuration.toml"), "enabled = true\n").unwrap();
        let tools = tools(&temp);
        assert!(tools.read_file("binary.bin", None, None).await.is_err());
        let missing = tools
            .read_file("config.toml", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("Did you mean"));
        assert!(missing.contains("configuration.toml"));
        assert!(
            tools
                .grep("[", ".", None, false, false, None, None)
                .await
                .is_err()
        );
    }
}
