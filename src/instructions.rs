use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Take};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::config::wecode_home_dir;

const INSTRUCTION_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "CLAUDE.local.md"];
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_TOTAL_BYTES: usize = 192 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionSet {
    pub files: Vec<InstructionFile>,
}

impl InstructionSet {
    pub fn render(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut output = String::from(
            "\nProject instructions follow. Later files are more specific and take precedence.\n",
        );
        for file in &self.files {
            output.push_str("\n<project_instructions path=\"");
            output.push_str(&file.path.display().to_string());
            output.push_str("\">\n");
            output.push_str(&file.content);
            if !file.content.ends_with('\n') {
                output.push('\n');
            }
            if file.truncated {
                output.push_str("[instruction file truncated by WeCode]\n");
            }
            output.push_str("</project_instructions>\n");
        }
        output
    }
}

pub fn discover(workspace: &Path) -> Result<InstructionSet> {
    discover_with_home(workspace, &wecode_home_dir())
}

fn discover_with_home(workspace: &Path, home: &Path) -> Result<InstructionSet> {
    let mut candidates = Vec::new();
    for name in INSTRUCTION_NAMES {
        candidates.push(home.join(name));
    }
    append_markdown_files(&home.join("rules"), &mut candidates)?;

    let root = repository_root(workspace);
    let mut directories = workspace
        .ancestors()
        .take_while(|directory| *directory != root)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.push(root.to_path_buf());
    directories.reverse();
    for directory in directories {
        for name in INSTRUCTION_NAMES {
            candidates.push(directory.join(name));
        }
        append_markdown_files(&directory.join(".wecode/rules"), &mut candidates)?;
        append_markdown_files(&directory.join(".claude/rules"), &mut candidates)?;
    }

    let mut seen = HashSet::new();
    let mut seen_contents = HashSet::new();
    let mut files = Vec::new();
    let mut remaining = MAX_TOTAL_BYTES;
    for candidate in candidates {
        if remaining == 0 || !candidate.is_file() {
            continue;
        }
        let canonical = candidate.canonicalize().with_context(|| {
            format!("failed to resolve instruction file {}", candidate.display())
        })?;
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let limit = remaining.min(MAX_FILE_BYTES);
        let (content, truncated) = read_bounded(&canonical, limit)?;
        if !truncated && !seen_contents.insert(format!("{:x}", Sha256::digest(content.as_bytes())))
        {
            continue;
        }
        remaining = remaining.saturating_sub(content.len());
        files.push(InstructionFile {
            path: canonical,
            content,
            truncated,
        });
    }
    Ok(InstructionSet { files })
}

fn repository_root(workspace: &Path) -> &Path {
    workspace
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(workspace)
}

fn append_markdown_files(directory: &Path, candidates: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    let mut paths = std::fs::read_dir(directory)
        .with_context(|| format!("failed to read rules directory {}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    candidates.extend(paths);
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<(String, bool)> {
    let file = File::open(path)
        .with_context(|| format!("failed to read instruction {}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut reader: Take<File> = file.take(limit.saturating_add(1) as u64);
    reader.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    while content.len() > limit {
        content.pop();
    }
    Ok((content, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_global_and_hierarchical_project_rules_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("repo");
        let workspace = root.join("crates/app");
        std::fs::create_dir_all(home.join("rules")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".wecode/rules")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(home.join("rules/10-style.md"), "global rules").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "root").unwrap();
        std::fs::write(root.join(".wecode/rules/build.md"), "build").unwrap();
        std::fs::write(root.join("crates/CLAUDE.md"), "crates").unwrap();
        std::fs::write(workspace.join("CLAUDE.local.md"), "local").unwrap();

        let instructions = discover_with_home(&workspace, &home).unwrap();
        let contents = instructions
            .files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            contents,
            ["global", "global rules", "root", "build", "crates", "local"]
        );
        assert!(instructions.render().contains("take precedence"));
    }

    #[test]
    fn bounds_each_instruction_file() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("repo");
        let home = temp.path().join("home");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "x".repeat(MAX_FILE_BYTES + 20)).unwrap();

        let instructions = discover_with_home(&workspace, &home).unwrap();

        assert_eq!(instructions.files[0].content.len(), MAX_FILE_BYTES);
        assert!(instructions.files[0].truncated);
    }
}
