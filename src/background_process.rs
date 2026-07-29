use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::ProcessesConfig;
use crate::executor::Executor;

const MAX_COMMAND_BYTES: usize = 64 * 1_024;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_INPUT_BYTES: usize = 64 * 1_024;
const MAX_POLL_BYTES: usize = 16 * 1_024;
const MAX_NOTIFICATION_BYTES: usize = 4 * 1_024;
const MAX_NOTIFICATIONS: usize = 64;
const PROCESS_TICK: Duration = Duration::from_millis(50);
const TERMINATION_GRACE: Duration = Duration::from_millis(300);

type ProcessEventHandler = Arc<dyn Fn(BackgroundProcessEvent) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundProcessStatus {
    Running,
    Exited,
    Failed,
    Killed,
    TimedOut,
}

impl BackgroundProcessStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

#[derive(Clone, Debug)]
pub struct BackgroundProcessSummary {
    pub process_id: u64,
    pub command: String,
    pub description: String,
    pub status: BackgroundProcessStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub total_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct BackgroundProcessEvent {
    pub summary: BackgroundProcessSummary,
    pub output_tail: String,
}

#[derive(Clone)]
pub struct BackgroundProcessManager {
    inner: Arc<ManagerInner>,
}

impl fmt::Debug for BackgroundProcessManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundProcessManager")
            .field("enabled", &self.inner.config.enabled)
            .field("max_processes", &self.inner.config.max_processes)
            .field("process_count", &self.summaries().len())
            .finish()
    }
}

impl Drop for BackgroundProcessManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let state = self
            .inner
            .state
            .lock()
            .expect("process state lock poisoned");
        for entry in state.entries.values() {
            if entry.status() == BackgroundProcessStatus::Running {
                let _ = entry
                    .commands
                    .try_send(ProcessCommand::Stop { response: None });
            }
        }
    }
}

struct ManagerInner {
    config: ProcessesConfig,
    executor: Executor,
    state: Mutex<ManagerState>,
    notifications: Mutex<VecDeque<String>>,
    event_handler: Mutex<Option<ProcessEventHandler>>,
}

struct ManagerState {
    next_id: u64,
    entries: HashMap<u64, Arc<ProcessEntry>>,
}

struct ProcessEntry {
    process_id: u64,
    command: String,
    description: String,
    started: Instant,
    state: Mutex<ProcessState>,
    output: Mutex<OutputLog>,
    commands: mpsc::Sender<ProcessCommand>,
}

struct ProcessState {
    status: BackgroundProcessStatus,
    exit_code: Option<i32>,
    failure: Option<String>,
    duration_ms: u128,
}

enum ProcessCommand {
    Write {
        input: Vec<u8>,
        newline: bool,
        response: oneshot::Sender<Result<(), String>>,
    },
    Stop {
        response: Option<oneshot::Sender<()>>,
    },
}

struct OutputLog {
    bytes: VecDeque<u8>,
    max_bytes: usize,
    start_cursor: u64,
    next_cursor: u64,
}

struct OutputChunk {
    text: String,
    next_cursor: u64,
    truncated_bytes: u64,
    remaining_bytes: u64,
}

enum Completion {
    Natural(Option<i32>),
    Failed(String),
    Killed(Option<oneshot::Sender<()>>),
    TimedOut,
}

struct Supervision {
    child: Child,
    stdin: Option<ChildStdin>,
    commands: mpsc::Receiver<ProcessCommand>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    max_runtime: Duration,
}

impl BackgroundProcessManager {
    pub fn new(
        config: ProcessesConfig,
        workspace: std::path::PathBuf,
        deny_dangerous_commands: bool,
        secret_env: Option<String>,
    ) -> Self {
        let executor = Executor::new(
            workspace,
            Duration::from_secs(config.max_runtime_seconds),
            config.max_output_bytes,
            deny_dangerous_commands,
            secret_env,
        );
        Self {
            inner: Arc::new(ManagerInner {
                config,
                executor,
                state: Mutex::new(ManagerState {
                    next_id: 1,
                    entries: HashMap::new(),
                }),
                notifications: Mutex::new(VecDeque::new()),
                event_handler: Mutex::new(None),
            }),
        }
    }

    pub fn set_event_handler<F>(&self, handler: F)
    where
        F: Fn(BackgroundProcessEvent) + Send + Sync + 'static,
    {
        *self
            .inner
            .event_handler
            .lock()
            .expect("process event handler lock poisoned") = Some(Arc::new(handler));
    }

    pub async fn start(&self, command: &str, description: &str) -> Result<String> {
        if !self.inner.config.enabled {
            bail!("background processes are disabled in config");
        }
        if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES {
            bail!("background command must contain between 1 and {MAX_COMMAND_BYTES} bytes");
        }
        if description.len() > MAX_DESCRIPTION_BYTES {
            bail!("background process description cannot exceed {MAX_DESCRIPTION_BYTES} bytes");
        }

        let process_id = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("process state lock poisoned");
            prune_finished(
                &mut state,
                self.inner.config.max_processes.saturating_mul(4),
            );
            let running = state
                .entries
                .values()
                .filter(|entry| entry.status() == BackgroundProcessStatus::Running)
                .count();
            if running >= self.inner.config.max_processes {
                bail!(
                    "background process limit reached ({})",
                    self.inner.config.max_processes
                );
            }
            let process_id = state.next_id;
            state.next_id = state.next_id.saturating_add(1);
            process_id
        };

        let mut process = self.inner.executor.prepare_shell_command(command)?;
        configure_process_group(&mut process);
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = process
            .spawn()
            .with_context(|| format!("failed to start background process {process_id}"))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .context("background stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("background stderr unavailable")?;
        let (command_tx, command_rx) = mpsc::channel(16);
        let entry = Arc::new(ProcessEntry {
            process_id,
            command: command.to_owned(),
            description: if description.trim().is_empty() {
                command.to_owned()
            } else {
                description.trim().to_owned()
            },
            started: Instant::now(),
            state: Mutex::new(ProcessState {
                status: BackgroundProcessStatus::Running,
                exit_code: None,
                failure: None,
                duration_ms: 0,
            }),
            output: Mutex::new(OutputLog::new(self.inner.config.max_output_bytes)),
            commands: command_tx,
        });
        self.inner
            .state
            .lock()
            .expect("process state lock poisoned")
            .entries
            .insert(process_id, entry.clone());

        let stdout_entry = entry.clone();
        let stdout_task = tokio::spawn(async move {
            read_output(stdout, stdout_entry, false).await;
        });
        let stderr_entry = entry.clone();
        let stderr_task = tokio::spawn(async move {
            read_output(stderr, stderr_entry, true).await;
        });
        let weak = Arc::downgrade(&self.inner);
        let max_runtime = Duration::from_secs(self.inner.config.max_runtime_seconds);
        tokio::spawn(supervise(
            weak,
            entry,
            Supervision {
                child,
                stdin,
                commands: command_rx,
                stdout_task,
                stderr_task,
                max_runtime,
            },
        ));

        Ok(format!(
            "BACKGROUND PROCESS STARTED\nprocess_id: {process_id}\nstatus: running\n\
             output_cursor: 0\nUse process_status to read output. Do not add shell background \
             operators; WeCode owns this process lifecycle."
        ))
    }

    pub fn status(&self, process_id: Option<u64>, cursor: Option<u64>) -> Result<String> {
        let Some(process_id) = process_id else {
            return Ok(format_process_list(&self.summaries()));
        };
        let entry = self.entry(process_id)?;
        let state = entry.state.lock().expect("process entry lock poisoned");
        let output = entry
            .output
            .lock()
            .expect("process output lock poisoned")
            .read(cursor.unwrap_or(0), MAX_POLL_BYTES);
        let exit_code = state
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".into());
        let failure = state
            .failure
            .as_deref()
            .map(|failure| format!("\nfailure: {failure}"))
            .unwrap_or_default();
        let output_text = if output.text.is_empty() {
            "(no new output)".to_owned()
        } else {
            output.text
        };
        Ok(format!(
            "BACKGROUND PROCESS STATUS\nprocess_id: {process_id}\nstatus: {}\nexit_code: \
             {exit_code}\nduration_ms: {}\nnext_cursor: {}\ntruncated_before_cursor: \
             {}\nremaining_bytes: {}{failure}\noutput:\n{output_text}",
            state.status.as_str(),
            if state.status == BackgroundProcessStatus::Running {
                entry.started.elapsed().as_millis()
            } else {
                state.duration_ms
            },
            output.next_cursor,
            output.truncated_bytes,
            output.remaining_bytes,
        ))
    }

    pub async fn write(&self, process_id: u64, input: &str, newline: bool) -> Result<String> {
        if input.len() > MAX_INPUT_BYTES {
            bail!("process input cannot exceed {MAX_INPUT_BYTES} bytes");
        }
        let entry = self.entry(process_id)?;
        if entry.status() != BackgroundProcessStatus::Running {
            bail!("background process {process_id} is not running");
        }
        let (response_tx, response_rx) = oneshot::channel();
        entry
            .commands
            .send(ProcessCommand::Write {
                input: input.as_bytes().to_vec(),
                newline,
                response: response_tx,
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!("background process {process_id} is no longer available")
            })?;
        response_rx
            .await
            .context("background process write response dropped")?
            .map_err(anyhow::Error::msg)?;
        Ok(format!(
            "BACKGROUND PROCESS INPUT WRITTEN\nprocess_id: {process_id}\nbytes: {}",
            input.len() + usize::from(newline)
        ))
    }

    pub async fn stop(&self, process_id: u64) -> Result<String> {
        let entry = self.entry(process_id)?;
        if entry.status() != BackgroundProcessStatus::Running {
            return self.status(Some(process_id), None);
        }
        let (response_tx, response_rx) = oneshot::channel();
        entry
            .commands
            .send(ProcessCommand::Stop {
                response: Some(response_tx),
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!("background process {process_id} is no longer available")
            })?;
        tokio::time::timeout(Duration::from_secs(5), response_rx)
            .await
            .context("timed out stopping background process")?
            .context("background process stop response dropped")?;
        self.status(Some(process_id), None)
    }

    pub fn summaries(&self) -> Vec<BackgroundProcessSummary> {
        let state = self
            .inner
            .state
            .lock()
            .expect("process state lock poisoned");
        let mut summaries = state
            .entries
            .values()
            .map(|entry| entry.summary())
            .collect::<Vec<_>>();
        summaries.sort_by_key(|summary| summary.process_id);
        summaries
    }

    pub fn take_notifications(&self) -> Vec<String> {
        self.inner
            .notifications
            .lock()
            .expect("process notification lock poisoned")
            .drain(..)
            .collect()
    }

    pub async fn shutdown_all(&self) {
        let running = self
            .summaries()
            .into_iter()
            .filter(|summary| summary.status == BackgroundProcessStatus::Running)
            .map(|summary| summary.process_id)
            .collect::<Vec<_>>();
        join_all(running.into_iter().map(|process_id| self.stop(process_id))).await;
    }

    fn entry(&self, process_id: u64) -> Result<Arc<ProcessEntry>> {
        self.inner
            .state
            .lock()
            .expect("process state lock poisoned")
            .entries
            .get(&process_id)
            .cloned()
            .with_context(|| format!("unknown background process {process_id}"))
    }
}

impl ProcessEntry {
    fn status(&self) -> BackgroundProcessStatus {
        self.state
            .lock()
            .expect("process entry lock poisoned")
            .status
    }

    fn summary(&self) -> BackgroundProcessSummary {
        let state = self.state.lock().expect("process entry lock poisoned");
        let total_output_bytes = self
            .output
            .lock()
            .expect("process output lock poisoned")
            .next_cursor;
        BackgroundProcessSummary {
            process_id: self.process_id,
            command: self.command.clone(),
            description: self.description.clone(),
            status: state.status,
            exit_code: state.exit_code,
            duration_ms: if state.status == BackgroundProcessStatus::Running {
                self.started.elapsed().as_millis()
            } else {
                state.duration_ms
            },
            total_output_bytes,
        }
    }
}

impl OutputLog {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            max_bytes,
            start_cursor: 0,
            next_cursor: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.next_cursor = self
            .next_cursor
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.bytes.extend(bytes);
        while self.bytes.len() > self.max_bytes {
            self.bytes.pop_front();
            self.start_cursor = self.start_cursor.saturating_add(1);
        }
    }

    fn read(&self, requested_cursor: u64, limit: usize) -> OutputChunk {
        let cursor = requested_cursor.clamp(self.start_cursor, self.next_cursor);
        let skipped = usize::try_from(cursor.saturating_sub(self.start_cursor))
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        let bytes = self
            .bytes
            .iter()
            .skip(skipped)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();
        let next_cursor = cursor.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        OutputChunk {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            next_cursor,
            truncated_bytes: self.start_cursor.saturating_sub(requested_cursor),
            remaining_bytes: self.next_cursor.saturating_sub(next_cursor),
        }
    }

    fn tail(&self, limit: usize) -> String {
        let skip = self.bytes.len().saturating_sub(limit);
        String::from_utf8_lossy(&self.bytes.iter().skip(skip).copied().collect::<Vec<_>>())
            .into_owned()
    }
}

async fn supervise(
    manager: Weak<ManagerInner>,
    entry: Arc<ProcessEntry>,
    supervision: Supervision,
) {
    let Supervision {
        mut child,
        mut stdin,
        mut commands,
        mut stdout_task,
        mut stderr_task,
        max_runtime,
    } = supervision;
    let mut interval = tokio::time::interval(PROCESS_TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = tokio::time::sleep(max_runtime);
    tokio::pin!(deadline);

    let completion = loop {
        tokio::select! {
            _ = &mut deadline => {
                terminate_process_tree(&mut child).await;
                break Completion::TimedOut;
            }
            command = commands.recv() => {
                match command {
                    Some(ProcessCommand::Write { input, newline, response }) => {
                        let result = write_process_input(&mut stdin, &input, newline).await;
                        let _ = response.send(result.map_err(|error| error.to_string()));
                    }
                    Some(ProcessCommand::Stop { response }) => {
                        terminate_process_tree(&mut child).await;
                        break Completion::Killed(response);
                    }
                    None => {
                        terminate_process_tree(&mut child).await;
                        break Completion::Killed(None);
                    }
                }
            }
            _ = interval.tick() => {
                match child.try_wait() {
                    Ok(Some(status)) => break Completion::Natural(status.code()),
                    Ok(None) => {}
                    Err(error) => break Completion::Failed(error.to_string()),
                }
            }
        }
    };

    stdin.take();
    if tokio::time::timeout(Duration::from_secs(1), async {
        let _ = (&mut stdout_task).await;
        let _ = (&mut stderr_task).await;
    })
    .await
    .is_err()
    {
        stdout_task.abort();
        stderr_task.abort();
    }

    let stop_response = match completion {
        Completion::Natural(exit_code) => {
            let status = if exit_code == Some(0) {
                BackgroundProcessStatus::Exited
            } else {
                BackgroundProcessStatus::Failed
            };
            finish_entry(&entry, status, exit_code, None);
            None
        }
        Completion::Failed(failure) => {
            finish_entry(&entry, BackgroundProcessStatus::Failed, None, Some(failure));
            None
        }
        Completion::Killed(response) => {
            finish_entry(&entry, BackgroundProcessStatus::Killed, None, None);
            response
        }
        Completion::TimedOut => {
            finish_entry(&entry, BackgroundProcessStatus::TimedOut, None, None);
            None
        }
    };
    publish_completion(&manager, &entry);
    if let Some(response) = stop_response {
        let _ = response.send(());
    }
}

fn finish_entry(
    entry: &ProcessEntry,
    status: BackgroundProcessStatus,
    exit_code: Option<i32>,
    failure: Option<String>,
) {
    let mut state = entry.state.lock().expect("process entry lock poisoned");
    state.status = status;
    state.exit_code = exit_code;
    state.failure = failure;
    state.duration_ms = entry.started.elapsed().as_millis();
}

fn publish_completion(manager: &Weak<ManagerInner>, entry: &ProcessEntry) {
    let Some(manager) = manager.upgrade() else {
        return;
    };
    let summary = entry.summary();
    let output_tail = entry
        .output
        .lock()
        .expect("process output lock poisoned")
        .tail(MAX_NOTIFICATION_BYTES);
    let notification = format_process_notification(&summary, &output_tail);
    {
        let mut notifications = manager
            .notifications
            .lock()
            .expect("process notification lock poisoned");
        if notifications.len() >= MAX_NOTIFICATIONS {
            notifications.pop_front();
        }
        notifications.push_back(notification);
    }
    let handler = manager
        .event_handler
        .lock()
        .expect("process event handler lock poisoned")
        .clone();
    if let Some(handler) = handler {
        handler(BackgroundProcessEvent {
            summary,
            output_tail,
        });
    }
}

async fn read_output<R>(mut reader: R, entry: Arc<ProcessEntry>, stderr: bool)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4_096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut output = entry.output.lock().expect("process output lock poisoned");
                if stderr {
                    output.append(b"[stderr] ");
                }
                output.append(&buffer[..read]);
            }
        }
    }
}

async fn write_process_input(
    stdin: &mut Option<ChildStdin>,
    input: &[u8],
    newline: bool,
) -> Result<()> {
    let stdin = stdin
        .as_mut()
        .context("background process stdin is closed")?;
    stdin.write_all(input).await?;
    if newline {
        stdin.write_all(b"\n").await?;
    }
    stdin.flush().await?;
    Ok(())
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_process_tree(child: &mut Child) {
    let Some(process_id) = child.id() else {
        return;
    };
    send_process_group_signal(process_id, "-TERM").await;
    tokio::time::sleep(TERMINATION_GRACE).await;
    send_process_group_signal(process_id, "-KILL").await;
    let _ = child.kill().await;
}

#[cfg(unix)]
async fn send_process_group_signal(process_id: u32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .arg(signal)
        .arg(format!("-{process_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(windows)]
async fn terminate_process_tree(child: &mut Child) {
    if let Some(process_id) = child.id() {
        let process_id = process_id.to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &process_id, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

fn prune_finished(state: &mut ManagerState, max_entries: usize) {
    while state.entries.len() >= max_entries.max(1) {
        let Some(process_id) = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.status().is_terminal())
            .map(|(process_id, _)| *process_id)
            .min()
        else {
            break;
        };
        state.entries.remove(&process_id);
    }
}

fn format_process_list(summaries: &[BackgroundProcessSummary]) -> String {
    if summaries.is_empty() {
        return "BACKGROUND PROCESSES\n(no processes)".into();
    }
    let mut output = String::from("BACKGROUND PROCESSES");
    for summary in summaries {
        output.push_str(&format!(
            "\n- {} · {} · {}ms · {}",
            summary.process_id,
            summary.status.as_str(),
            summary.duration_ms,
            summary.description
        ));
    }
    output
}

fn format_process_notification(summary: &BackgroundProcessSummary, output_tail: &str) -> String {
    let exit_code = summary
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".into());
    let output = if output_tail.trim().is_empty() {
        "(no output)"
    } else {
        output_tail
    };
    format!(
        "<background_process_notification>\nprocess_id: {}\nstatus: {}\nexit_code: \
         {exit_code}\ndescription: {}\noutput_tail:\n{output}\n</background_process_notification>",
        summary.process_id,
        summary.status.as_str(),
        summary.description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(workspace: &std::path::Path) -> BackgroundProcessManager {
        BackgroundProcessManager::new(
            ProcessesConfig {
                max_runtime_seconds: 10,
                max_output_bytes: 8 * 1_024,
                ..Default::default()
            },
            workspace.to_path_buf(),
            true,
            None,
        )
    }

    async fn wait_for_terminal(
        manager: &BackgroundProcessManager,
        process_id: u64,
    ) -> BackgroundProcessSummary {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let summary = manager
                    .summaries()
                    .into_iter()
                    .find(|summary| summary.process_id == process_id)
                    .unwrap();
                if summary.status.is_terminal() {
                    return summary;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn captures_incremental_output_and_completion_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path());
        #[cfg(unix)]
        let command = "printf 'first\\n'; sleep 0.1; printf 'second\\n'";
        #[cfg(windows)]
        let command = "echo first & ping -n 2 127.0.0.1 >nul & echo second";
        let started = manager.start(command, "fixture").await.unwrap();
        assert!(started.contains("process_id: 1"));
        let summary = wait_for_terminal(&manager, 1).await;
        assert_eq!(summary.status, BackgroundProcessStatus::Exited);
        assert_eq!(summary.exit_code, Some(0));
        let status = manager.status(Some(1), Some(0)).unwrap();
        assert!(status.contains("first"));
        assert!(status.contains("second"));
        let notifications = manager.take_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("fixture"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writes_stdin_and_stops_process_groups() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path());
        manager
            .start(
                "IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 10",
                "stdin fixture",
            )
            .await
            .unwrap();
        manager.write(1, "hello", true).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if manager
                    .status(Some(1), Some(0))
                    .unwrap()
                    .contains("got:hello")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let stopped = manager.stop(1).await.unwrap();
        assert!(stopped.contains("status: killed"));
    }

    #[test]
    fn output_log_reports_eviction_and_continuation() {
        let mut output = OutputLog::new(5);
        output.append(b"12345678");
        let first = output.read(0, 2);
        assert_eq!(first.truncated_bytes, 3);
        assert_eq!(first.text, "45");
        assert_eq!(first.next_cursor, 5);
        assert_eq!(first.remaining_bytes, 3);
    }
}
