use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use codex_apply_patch_lite::{Hunk, derive_new_contents, parse_patch};

pub async fn apply_patch(workspace: &Path, patch: &str) -> Result<String> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("workspace {} does not exist", workspace.display()))?;
    let patch = patch.to_owned();
    tokio::task::spawn_blocking(move || apply_patch_sync(&workspace, &patch)).await?
}

pub fn affected_paths(patch: &str) -> Result<Vec<PathBuf>> {
    let parsed = parse_patch(patch).context("invalid Codex apply_patch payload")?;
    let mut paths = Vec::new();
    for hunk in parsed.hunks {
        match hunk {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => paths.push(path),
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                paths.push(path);
                if let Some(move_path) = move_path {
                    paths.push(move_path);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn apply_patch_sync(workspace: &Path, patch: &str) -> Result<String> {
    let parsed = parse_patch(patch).context("invalid Codex apply_patch payload")?;
    let mut changed = Vec::with_capacity(parsed.hunks.len());

    for hunk in parsed.hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let target = resolve_workspace_path(workspace, &path)?;
                ensure_parent(&target)?;
                std::fs::write(&target, contents)
                    .with_context(|| format!("failed to add {}", path.display()))?;
                changed.push(format!("A {}", path.display()));
            }
            Hunk::DeleteFile { path } => {
                let target = resolve_workspace_path(workspace, &path)?;
                let metadata = std::fs::metadata(&target)
                    .with_context(|| format!("failed to inspect {}", path.display()))?;
                if !metadata.is_file() {
                    bail!("cannot delete non-file {}", path.display());
                }
                std::fs::remove_file(&target)
                    .with_context(|| format!("failed to delete {}", path.display()))?;
                changed.push(format!("D {}", path.display()));
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let source = resolve_workspace_path(workspace, &path)?;
                let original = std::fs::read_to_string(&source)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let updated = derive_new_contents(&original, &path, &chunks)?;
                if let Some(move_path) = move_path {
                    let target = resolve_workspace_path(workspace, &move_path)?;
                    ensure_parent(&target)?;
                    std::fs::write(&target, updated)
                        .with_context(|| format!("failed to write {}", move_path.display()))?;
                    if target != source {
                        std::fs::remove_file(&source)
                            .with_context(|| format!("failed to remove {}", path.display()))?;
                    }
                    changed.push(format!("R {} -> {}", path.display(), move_path.display()));
                } else {
                    std::fs::write(&source, updated)
                        .with_context(|| format!("failed to update {}", path.display()))?;
                    changed.push(format!("M {}", path.display()));
                }
            }
        }
    }

    Ok(format!("Done!\n{}", changed.join("\n")))
}

fn resolve_workspace_path(workspace: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!(
            "patch path must be a non-empty relative path: {}",
            relative.display()
        );
    }
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            _ => bail!("patch path escapes the workspace: {}", relative.display()),
        }
    }

    let target = workspace.join(normalized);
    let mut existing = target.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .context("patch target has no existing parent")?;
    }
    let canonical_existing = existing
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", existing.display()))?;
    if !canonical_existing.starts_with(workspace) {
        bail!("patch path escapes the workspace: {}", relative.display());
    }
    Ok(target)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applies_codex_patch_with_fuzzy_whitespace_matching() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("sample.rs"),
            "fn main() {\n    old();   \n}\n",
        )
        .unwrap();
        let patch = "*** Begin Patch\n*** Update File: sample.rs\n@@\n fn main() {\n-old();\n+new();\n }\n*** End Patch";

        let output = apply_patch(temp.path(), patch).await.unwrap();

        assert!(output.contains("M sample.rs"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sample.rs")).unwrap(),
            "fn main() {\nnew();\n}\n"
        );
    }

    #[tokio::test]
    async fn rejects_parent_and_symlink_escape_paths() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let outside = tempfile::tempdir().unwrap();
        let parent_patch = "*** Begin Patch\n*** Add File: ../escape.txt\n+bad\n*** End Patch";
        assert!(apply_patch(temp.path(), parent_patch).await.is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), temp.path().join("link")).unwrap();
            let symlink_patch =
                "*** Begin Patch\n*** Add File: link/escape.txt\n+bad\n*** End Patch";
            assert!(apply_patch(temp.path(), symlink_patch).await.is_err());
        }
    }

    #[test]
    fn reports_every_path_affected_by_a_patch() {
        let patch = "*** Begin Patch\n*** Add File: new.rs\n+new\n*** Update File: old.rs\n*** Move to: moved.rs\n@@\n-old\n+changed\n*** Delete File: gone.rs\n*** End Patch";
        assert_eq!(
            affected_paths(patch).unwrap(),
            vec![
                PathBuf::from("gone.rs"),
                PathBuf::from("moved.rs"),
                PathBuf::from("new.rs"),
                PathBuf::from("old.rs"),
            ]
        );
    }
}
