use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::executor::scrub_secret_environment;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const EXECUTABLE_FILTER_CONFIG_PATTERN: &str = r"^filter\..*\.(clean|process)$";

pub async fn head_id(workspace: &Path) -> Option<String> {
    let output = git_output(workspace, &[], &["rev-parse", "HEAD"])
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Collect a complete patch against `HEAD`, including staged and untracked files.
///
/// Repository-defined textconv, external diff, clean/process filters, hooks, and fsmonitor are
/// disabled. Patch collection is used by the benchmark path, so reading a repository must never
/// execute repository-controlled helpers.
pub async fn collect_patch(workspace: &Path) -> Result<String> {
    collect_worktree_diff(workspace, true)
        .await
        .map(|diff| diff.unwrap_or_default())
}

/// Collect a human-readable diff for `/diff`.
///
/// `None` means the workspace is not inside a Git worktree. `Some("")` means it is clean.
pub async fn collect_diff(workspace: &Path) -> Result<Option<String>> {
    collect_worktree_diff(workspace, false).await
}

async fn collect_worktree_diff(workspace: &Path, binary: bool) -> Result<Option<String>> {
    if !inside_git_repo(workspace).await? {
        return Ok(None);
    }
    let filter_overrides = executable_filter_overrides(workspace).await?;
    let has_head = git_output(workspace, &[], &["rev-parse", "--verify", "HEAD"])
        .await?
        .status
        .success();

    if !has_head {
        return collect_unborn_diff(workspace, &filter_overrides, binary)
            .await
            .map(Some);
    }

    let mut patch = tracked_diff(workspace, &filter_overrides, binary, true).await?;
    let untracked = git_stdout(
        workspace,
        &filter_overrides,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        false,
    )
    .await?;
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(raw_path);
        patch.push_str(
            &untracked_file_diff(workspace, &filter_overrides, path.as_ref(), binary).await?,
        );
    }
    Ok(Some(patch))
}

async fn collect_unborn_diff(
    workspace: &Path,
    filter_overrides: &[String],
    binary: bool,
) -> Result<String> {
    let paths = git_stdout(
        workspace,
        filter_overrides,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        false,
    )
    .await?;
    let mut patch = String::new();
    for raw_path in paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(raw_path);
        if workspace.join(path.as_ref()).exists() {
            patch.push_str(
                &untracked_file_diff(workspace, filter_overrides, path.as_ref(), binary).await?,
            );
        }
    }
    Ok(patch)
}

async fn tracked_diff(
    workspace: &Path,
    filter_overrides: &[String],
    binary: bool,
    against_head: bool,
) -> Result<String> {
    let mut args = vec![
        "diff",
        "--no-textconv",
        "--no-ext-diff",
        "--submodule=short",
        "--ignore-submodules=dirty",
        "--no-color",
    ];
    if binary {
        args.push("--binary");
    }
    if against_head {
        args.push("HEAD");
    }
    args.push("--");
    let output = git_output(workspace, filter_overrides, &args).await?;
    checked_diff_stdout(output, &args)
}

async fn untracked_file_diff(
    workspace: &Path,
    filter_overrides: &[String],
    path: &str,
    binary: bool,
) -> Result<String> {
    #[cfg(windows)]
    let null_device = "NUL";
    #[cfg(not(windows))]
    let null_device = "/dev/null";

    let mut args = vec![
        "diff",
        "--no-textconv",
        "--no-ext-diff",
        "--submodule=short",
        "--ignore-submodules=dirty",
        "--no-color",
        "--no-index",
    ];
    if binary {
        args.push("--binary");
    }
    args.extend(["--", null_device, path]);
    let output = git_output(workspace, filter_overrides, &args).await?;
    checked_diff_stdout(output, &args)
}

fn checked_diff_stdout(output: Output, args: &[&str]) -> Result<String> {
    if output.status.success() || output.status.code() == Some(1) {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    bail!(
        "git {:?} failed with status {}: {}",
        args,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into()),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

async fn inside_git_repo(workspace: &Path) -> Result<bool> {
    Ok(
        git_output(workspace, &[], &["rev-parse", "--is-inside-work-tree"])
            .await?
            .status
            .success(),
    )
}

async fn executable_filter_overrides(workspace: &Path) -> Result<Vec<String>> {
    let args = [
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        EXECUTABLE_FILTER_CONFIG_PATTERN,
    ];
    let output = git_output(workspace, &[], &args).await?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git {:?} failed with status {}: {}",
            args,
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut drivers = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter_map(|key| {
            key.strip_suffix(".clean")
                .or_else(|| key.strip_suffix(".process"))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    drivers.sort();
    drivers.dedup();

    Ok(drivers
        .into_iter()
        .flat_map(|driver| {
            [
                format!("{driver}.clean="),
                format!("{driver}.process="),
                format!("{driver}.required=false"),
            ]
        })
        .collect())
}

async fn git_stdout(
    workspace: &Path,
    config_overrides: &[String],
    args: &[&str],
    allow_diff: bool,
) -> Result<Vec<u8>> {
    let output = git_output(workspace, config_overrides, args).await?;
    if output.status.success() || (allow_diff && output.status.code() == Some(1)) {
        return Ok(output.stdout);
    }
    bail!(
        "git {:?} failed with status {}: {}",
        args,
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into()),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

async fn git_output(
    workspace: &Path,
    config_overrides: &[String],
    args: &[&str],
) -> Result<Output> {
    #[cfg(windows)]
    let null_device = "NUL";
    #[cfg(not(windows))]
    let null_device = "/dev/null";

    let mut command = Command::new("git");
    command
        .args(["-c", &format!("core.hooksPath={null_device}")])
        .args(["-c", "core.fsmonitor=false"]);
    for value in config_overrides {
        command.args(["-c", value]);
    }
    command
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    scrub_secret_environment(&mut command, None);

    tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .context("git command timed out after 30 seconds")?
        .context("failed to execute git")
}

pub fn patch_output_path(base: &Path, task_id: &str) -> PathBuf {
    base.join(format!("{task_id}.patch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(workspace: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        std::fs::write(temp.path().join("tracked.txt"), "before\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=WeCode Test",
                "-c",
                "user.email=wecode@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        temp
    }

    #[tokio::test]
    async fn diff_includes_staged_unstaged_and_untracked_files() {
        let temp = repository();
        std::fs::write(temp.path().join("tracked.txt"), "after\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        std::fs::write(temp.path().join("tracked.txt"), "final\n").unwrap();
        std::fs::write(temp.path().join("new.txt"), "new\n").unwrap();

        let diff = collect_diff(temp.path()).await.unwrap().unwrap();
        assert!(diff.contains("+final"));
        assert!(!diff.contains("+after"));
        assert!(diff.contains("new.txt"));
        assert!(diff.contains("+new"));
    }

    #[tokio::test]
    async fn diff_distinguishes_clean_and_non_repository_workspaces() {
        let clean = repository();
        assert_eq!(
            collect_diff(clean.path()).await.unwrap().as_deref(),
            Some("")
        );
        let outside = tempfile::tempdir().unwrap();
        assert!(collect_diff(outside.path()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn diff_in_unborn_repository_uses_current_file_contents_once() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        std::fs::write(temp.path().join("staged.txt"), "staged\n").unwrap();
        git(temp.path(), &["add", "staged.txt"]);
        std::fs::write(temp.path().join("staged.txt"), "current\n").unwrap();

        let diff = collect_diff(temp.path()).await.unwrap().unwrap();
        assert_eq!(diff.matches("diff --git").count(), 1);
        assert!(diff.contains("+current"));
        assert!(!diff.contains("+staged"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diff_does_not_execute_repository_filter_commands() {
        use std::os::unix::fs::PermissionsExt;

        let temp = repository();
        std::fs::write(temp.path().join(".gitattributes"), "*.txt filter=hostile\n").unwrap();
        git(temp.path(), &["add", ".gitattributes"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=WeCode Test",
                "-c",
                "user.email=wecode@example.invalid",
                "commit",
                "-m",
                "attributes",
            ],
        );
        let marker = temp.path().join("filter-ran");
        let filter = temp.path().join("hostile-filter.sh");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o700)).unwrap();
        git(
            temp.path(),
            &["config", "filter.hostile.clean", &filter.to_string_lossy()],
        );
        std::fs::write(temp.path().join("tracked.txt"), "changed\n").unwrap();

        let diff = collect_diff(temp.path()).await.unwrap().unwrap();
        assert!(diff.contains("changed"));
        assert!(!marker.exists(), "repository clean filter executed");
    }
}
