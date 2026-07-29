use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

#[cfg(unix)]
const TERMINATION_GRACE: Duration = Duration::from_millis(300);

#[cfg(unix)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure(_command: &mut Command) {}

#[cfg(unix)]
pub(crate) async fn terminate(child: &mut Child) {
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
pub(crate) async fn terminate(child: &mut Child) {
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
