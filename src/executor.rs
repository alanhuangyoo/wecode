use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::patch;

const SECRET_ENV_VARS: &[&str] = &[
    "WECODE_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
    "DEEPSEEK_API_KEY",
    "GROQ_API_KEY",
    "XAI_API_KEY",
    "MISTRAL_API_KEY",
];

#[derive(Clone, Debug)]
pub struct Executor {
    workspace: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    deny_dangerous_commands: bool,
    extra_secret_env: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub timed_out: bool,
    pub truncated_bytes: usize,
}

impl ExecutionResult {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    pub fn observation(&self) -> String {
        let mut value = format!(
            "exit_code: {}\nduration_ms: {}\ntimed_out: {}\n",
            self.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".into()),
            self.duration_ms,
            self.timed_out
        );
        if !self.stdout.is_empty() {
            value.push_str("\nstdout:\n");
            value.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            value.push_str("\nstderr:\n");
            value.push_str(&self.stderr);
        }
        value
    }
}

impl Executor {
    pub fn new(
        workspace: PathBuf,
        timeout: Duration,
        max_output_bytes: usize,
        deny_dangerous_commands: bool,
        extra_secret_env: Option<String>,
    ) -> Self {
        Self {
            workspace,
            timeout,
            max_output_bytes,
            deny_dangerous_commands,
            extra_secret_env,
        }
    }

    pub async fn shell(&self, command: &str) -> Result<ExecutionResult> {
        self.shell_with_stdin(command, None).await
    }

    pub async fn shell_with_input(&self, command: &str, stdin: &[u8]) -> Result<ExecutionResult> {
        self.shell_with_stdin(command, Some(stdin)).await
    }

    async fn shell_with_stdin(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
    ) -> Result<ExecutionResult> {
        let command = self.prepare_shell_command(command)?;
        self.run(command, stdin).await
    }

    pub(crate) fn prepare_shell_command(&self, shell_command: &str) -> Result<Command> {
        if self.deny_dangerous_commands {
            reject_dangerous_command(shell_command)?;
        }
        #[cfg(windows)]
        let (program, args): (&str, &[&str]) = ("cmd", &["/D", "/S", "/C", shell_command]);
        #[cfg(not(windows))]
        let (program, args): (&str, &[&str]) = ("/bin/sh", &["-lc", shell_command]);
        let mut command = Command::new(program);
        command.args(args).current_dir(&self.workspace);
        scrub_secret_environment(&mut command, self.extra_secret_env.as_deref());
        Ok(command)
    }

    pub async fn apply_patch(&self, patch: &str) -> Result<ExecutionResult> {
        let started = Instant::now();
        let stdout = patch::apply_patch(&self.workspace, patch).await?;
        Ok(ExecutionResult {
            exit_code: Some(0),
            stdout,
            stderr: String::new(),
            duration_ms: started.elapsed().as_millis(),
            timed_out: false,
            truncated_bytes: 0,
        })
    }

    async fn run(&self, mut command: Command, stdin: Option<&[u8]>) -> Result<ExecutionResult> {
        let started = Instant::now();
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().context("failed to start shell command")?;
        if let Some(input) = stdin {
            let mut child_stdin = child.stdin.take().context("child stdin unavailable")?;
            child_stdin.write_all(input).await?;
            drop(child_stdin);
        }
        let stdout = child.stdout.take().context("child stdout unavailable")?;
        let stderr = child.stderr.take().context("child stderr unavailable")?;
        let output_budget = self.max_output_bytes / 2;
        let stdout_task = tokio::spawn(read_capped(stdout, output_budget));
        let stderr_task = tokio::spawn(read_capped(stderr, output_budget));

        let (status, timed_out) = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(status) => (Some(status?), false),
            Err(_) => {
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                (status, true)
            }
        };
        let stdout = stdout_task.await??;
        let stderr = stderr_task.await??;
        Ok(ExecutionResult {
            exit_code: status.and_then(|status| status.code()),
            stdout: stdout.text(),
            stderr: stderr.text(),
            duration_ms: started.elapsed().as_millis(),
            timed_out,
            truncated_bytes: stdout.omitted.saturating_add(stderr.omitted),
        })
    }
}

pub(crate) fn scrub_secret_environment(command: &mut Command, extra_secret_env: Option<&str>) {
    for name in SECRET_ENV_VARS {
        command.env_remove(name);
    }
    if let Some(name) = extra_secret_env {
        command.env_remove(name);
    }
}

fn reject_dangerous_command(command: &str) -> Result<()> {
    let normalized = command.to_ascii_lowercase();
    let denied = [
        "rm -rf /",
        "rm -fr /",
        "sudo ",
        "shutdown",
        "reboot",
        "mkfs",
        "diskutil erase",
        "format c:",
        "dd if=",
        ":(){",
        "git reset --hard",
        "git clean -fd",
        "git clean -df",
        ".wecode/credentials",
    ];
    if let Some(pattern) = denied.iter().find(|pattern| normalized.contains(**pattern)) {
        bail!(
            "command rejected by the local safety policy ({pattern:?}); use a sandbox and --unsafe-local if this command is intentional"
        );
    }
    Ok(())
}

#[derive(Debug)]
struct CappedBytes {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    max: usize,
    omitted: usize,
}

impl CappedBytes {
    fn new(max: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            max,
            omitted: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let head_budget = self.max / 2;
        let remaining_head = head_budget.saturating_sub(self.head.len());
        let head_count = remaining_head.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_count]);
        let tail_budget = self.max.saturating_sub(head_budget);
        for byte in &bytes[head_count..] {
            self.tail.push_back(*byte);
            if self.tail.len() > tail_budget {
                self.tail.pop_front();
                self.omitted = self.omitted.saturating_add(1);
            }
        }
    }

    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        if self.omitted > 0 {
            bytes.extend_from_slice(
                format!("\n... {} bytes omitted from the middle ...\n", self.omitted).as_bytes(),
            );
        }
        bytes.extend(self.tail.iter());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, max: usize) -> Result<CappedBytes> {
    let mut result = CappedBytes::new(max);
    let mut buffer = vec![0; 8_192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        result.push(&buffer[..read]);
    }
    Ok(result)
}

pub fn workspace_is_within(path: &Path, workspace: &Path) -> bool {
    path.canonicalize()
        .ok()
        .is_some_and(|path| path.starts_with(workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_buffer_preserves_both_ends() {
        let mut buffer = CappedBytes::new(10);
        buffer.push(b"abcdefghijklmnopqrst");
        let text = buffer.text();
        assert!(text.starts_with("abcde"));
        assert!(text.ends_with("pqrst"));
        assert_eq!(buffer.omitted, 10);
    }

    #[test]
    fn safety_policy_blocks_destructive_commands() {
        assert!(reject_dangerous_command("sudo rm -rf /tmp/x").is_err());
        assert!(reject_dangerous_command("cargo test").is_ok());
    }

    #[tokio::test]
    async fn shell_executes_in_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let executor = Executor::new(
            temp.path().into(),
            Duration::from_secs(2),
            4_096,
            true,
            None,
        );
        let result = executor.shell("echo hello").await.unwrap();
        assert!(result.success());
        assert_eq!(result.stdout.trim(), "hello");
    }
}
