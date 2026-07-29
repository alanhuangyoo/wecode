use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn head_id(workspace: &Path) -> Option<String> {
    command_output(workspace, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|value| value.trim().to_owned())
}

pub async fn collect_patch(workspace: &Path) -> Result<String> {
    if command_output(workspace, &["rev-parse", "--is-inside-work-tree"])
        .await
        .is_err()
    {
        return Ok(String::new());
    }
    let mut patch = command_output(
        workspace,
        &["diff", "--binary", "--no-ext-diff", "--no-color"],
    )
    .await?;
    let untracked = command_output_bytes(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(raw_path);
        #[cfg(windows)]
        let null_device = "NUL";
        #[cfg(not(windows))]
        let null_device = "/dev/null";
        let output = Command::new("git")
            .args([
                "diff",
                "--no-index",
                "--binary",
                "--no-color",
                "--",
                null_device,
                path.as_ref(),
            ])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to diff untracked file {path}"))?;
        // git diff --no-index uses exit code 1 when differences are present.
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!(
                "git failed to diff untracked file {path}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        patch.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(patch)
}

async fn command_output(workspace: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&command_output_bytes(workspace, args).await?).into_owned())
}

async fn command_output_bytes(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to execute git")?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout)
}

pub fn patch_output_path(base: &Path, task_id: &str) -> PathBuf {
    base.join(format!("{task_id}.patch"))
}
