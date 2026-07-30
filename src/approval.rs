use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    Shell,
    Patch,
    Mcp,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Patch => "patch",
            Self::Mcp => "mcp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskLevel {
    ReadOnly,
    WorkspaceWrite,
    Elevated,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Elevated => "elevated",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: u64,
    pub kind: ApprovalKind,
    pub risk: RiskLevel,
    pub summary: String,
    pub detail: String,
    pub fingerprint: String,
    pub session_scope: ApprovalScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalScope {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny { reason: String },
}

impl ApprovalDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllowOnce => "allow-once",
            Self::AllowSession => "allow-session",
            Self::Deny { .. } => "deny",
        }
    }
}

pub struct ApprovalEnvelope {
    pub request: ApprovalRequest,
    response: Option<oneshot::Sender<ApprovalDecision>>,
}

impl ApprovalEnvelope {
    pub fn resolve(mut self, decision: ApprovalDecision) {
        if let Some(response) = self.response.take() {
            let _ = response.send(decision);
        }
    }
}

impl Drop for ApprovalEnvelope {
    fn drop(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(ApprovalDecision::Deny {
                reason: "approval request was abandoned".into(),
            });
        }
    }
}

#[derive(Clone)]
pub struct ApprovalClient {
    inner: Arc<ApprovalState>,
}

impl std::fmt::Debug for ApprovalClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalClient")
            .finish_non_exhaustive()
    }
}

struct ApprovalState {
    next_id: AtomicU64,
    sender: mpsc::UnboundedSender<ApprovalEnvelope>,
    session_grants: Mutex<HashSet<String>>,
}

impl ApprovalClient {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<ApprovalEnvelope>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                inner: Arc::new(ApprovalState {
                    next_id: AtomicU64::new(0),
                    sender,
                    session_grants: Mutex::new(HashSet::new()),
                }),
            },
            receiver,
        )
    }

    pub fn prepare(
        &self,
        kind: ApprovalKind,
        risk: RiskLevel,
        summary: impl Into<String>,
        detail: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> ApprovalRequest {
        let detail = detail.into();
        let fingerprint = fingerprint.into();
        ApprovalRequest {
            id: self
                .inner
                .next_id
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1),
            kind,
            risk,
            summary: summary.into(),
            session_scope: session_scope(kind, risk, &fingerprint, &detail),
            detail,
            fingerprint,
        }
    }

    pub async fn request(&self, request: ApprovalRequest) -> ApprovalDecision {
        if self
            .inner
            .session_grants
            .lock()
            .expect("approval grants lock poisoned")
            .contains(&request.session_scope.key)
        {
            return ApprovalDecision::AllowSession;
        }
        let session_scope = request.session_scope.key.clone();
        let (response, decision) = oneshot::channel();
        if self
            .inner
            .sender
            .send(ApprovalEnvelope {
                request,
                response: Some(response),
            })
            .is_err()
        {
            return ApprovalDecision::Deny {
                reason: "no approval reviewer is available".into(),
            };
        }
        let decision = decision.await.unwrap_or_else(|_| ApprovalDecision::Deny {
            reason: "approval reviewer disconnected".into(),
        });
        if decision == ApprovalDecision::AllowSession {
            self.inner
                .session_grants
                .lock()
                .expect("approval grants lock poisoned")
                .insert(session_scope);
        }
        decision
    }
}

fn session_scope(
    kind: ApprovalKind,
    risk: RiskLevel,
    fingerprint: &str,
    detail: &str,
) -> ApprovalScope {
    match kind {
        ApprovalKind::Patch => ApprovalScope {
            key: "patch:workspace".into(),
            label: "all workspace patches".into(),
        },
        ApprovalKind::Mcp => ApprovalScope {
            key: fingerprint.to_owned(),
            label: format!("all calls to {}", fingerprint.trim_start_matches("mcp:")),
        },
        ApprovalKind::Shell => shell_session_scope(risk, fingerprint, detail),
    }
}

fn shell_session_scope(risk: RiskLevel, fingerprint: &str, command: &str) -> ApprovalScope {
    let namespace = fingerprint.split(':').next().unwrap_or("shell");
    let tokens = match shell_words::split(command) {
        Ok(tokens) if !tokens.is_empty() => tokens,
        _ => return exact_scope(fingerprint, command),
    };
    let executable = executable_name(&tokens[0]);
    let lowered = tokens
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();

    if exact_only_command(&lowered, risk) {
        return exact_scope(fingerprint, command);
    }

    if executable == "ssh"
        && let Some(host) = ssh_host(&tokens)
    {
        return ApprovalScope {
            key: format!("{namespace}:ssh:{host}"),
            label: format!("`ssh {host} ...` for this session"),
        };
    }

    let arity = command_arity(&tokens);
    ApprovalScope {
        key: format!("{namespace}:{}", arity.join("\u{1f}")),
        label: format!("`{} ...` for this session", arity.join(" ")),
    }
}

fn exact_scope(fingerprint: &str, command: &str) -> ApprovalScope {
    let label = command.split_whitespace().collect::<Vec<_>>().join(" ");
    ApprovalScope {
        key: fingerprint.to_owned(),
        label: format!("only `{}`", truncate_scope_label(&label)),
    }
}

fn executable_name(value: &str) -> String {
    std::path::Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn command_arity(tokens: &[String]) -> Vec<String> {
    let mut normalized = tokens.to_vec();
    normalized[0] = executable_name(&normalized[0]);
    crate::bash_arity::prefix(&normalized)
}

fn ssh_host(tokens: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for token in tokens.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            token.as_str(),
            "-b" | "-c"
                | "-D"
                | "-E"
                | "-e"
                | "-F"
                | "-I"
                | "-i"
                | "-J"
                | "-L"
                | "-l"
                | "-m"
                | "-O"
                | "-o"
                | "-p"
                | "-Q"
                | "-R"
                | "-S"
                | "-W"
                | "-w"
        ) {
            skip_next = true;
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return Some(token);
    }
    None
}

fn exact_only_command(tokens: &[String], risk: RiskLevel) -> bool {
    let first = tokens.first().map(String::as_str).unwrap_or_default();
    let second = tokens.get(1).map(String::as_str).unwrap_or_default();
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            ";" | "&&" | "||" | "|" | "&" | ">" | ">>" | "2>" | "2>>"
        )
    }) || matches!(
        first,
        "sudo"
            | "su"
            | "rm"
            | "kill"
            | "pkill"
            | "shutdown"
            | "reboot"
            | "chmod"
            | "chown"
            | "curl"
            | "wget"
            | "scp"
            | "python"
            | "python3"
            | "node"
            | "bash"
            | "sh"
            | "zsh"
    ) || (first == "git" && matches!(second, "push" | "reset" | "clean"))
        || matches!(second, "publish" | "install" | "uninstall")
        || (risk == RiskLevel::Elevated && first != "ssh")
}

fn truncate_scope_label(value: &str) -> String {
    const MAX_CHARS: usize = 96;
    if value.chars().count() <= MAX_CHARS {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(MAX_CHARS - 1).collect::<String>();
    truncated.push('…');
    truncated
}

pub fn classify_shell(command: &str) -> RiskLevel {
    let normalized = command.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return RiskLevel::ReadOnly;
    }
    if contains_any(
        &normalized,
        &[
            "sudo ",
            "git push",
            "git reset --hard",
            "git clean -",
            "curl ",
            "wget ",
            "ssh ",
            "scp ",
            "chmod ",
            "chown ",
            "kill ",
            "pkill ",
            "shutdown",
            "reboot",
            "brew install",
            "apt install",
            "apt-get install",
            "dnf install",
            "yum install",
            "pip install",
            "npm publish",
            "cargo publish",
        ],
    ) {
        return RiskLevel::Elevated;
    }
    if has_write_operator(&normalized)
        || contains_any(
            &normalized,
            &[";", "&&", "||", "$(", "`", "| sh", "| bash", "| zsh"],
        )
        || contains_any(
            &normalized,
            &[
                "rm ",
                "mv ",
                "cp ",
                "mkdir ",
                "touch ",
                "tee ",
                "sed -i",
                "git add",
                "git commit",
                "git checkout",
                "git switch",
                "git merge",
                "git rebase",
                "cargo fmt",
                "npm install",
                "pnpm install",
                "yarn install",
            ],
        )
    {
        return RiskLevel::WorkspaceWrite;
    }
    let first = normalized
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['(', '{']);
    if first == "git" {
        return if [
            "git status",
            "git diff",
            "git log",
            "git show",
            "git rev-parse",
            "git ls-files",
            "git grep",
            "git blame",
            "git branch --show-current",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
        {
            RiskLevel::ReadOnly
        } else {
            RiskLevel::WorkspaceWrite
        };
    }
    if first == "sed" {
        return if normalized.contains(" -i") {
            RiskLevel::WorkspaceWrite
        } else {
            RiskLevel::ReadOnly
        };
    }
    if first == "env" {
        return if normalized == "env" {
            RiskLevel::ReadOnly
        } else {
            RiskLevel::WorkspaceWrite
        };
    }
    if first == "find" && contains_any(&normalized, &[" -delete", " -exec", " -execdir", " -ok"]) {
        return RiskLevel::WorkspaceWrite;
    }
    if matches!(
        first,
        "ls" | "pwd"
            | "rg"
            | "grep"
            | "find"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "stat"
            | "file"
            | "which"
            | "type"
            | "printenv"
    ) {
        RiskLevel::ReadOnly
    } else {
        RiskLevel::WorkspaceWrite
    }
}

fn has_write_operator(command: &str) -> bool {
    command.contains(" >")
        || command.starts_with('>')
        || command.contains(">>")
        || command.contains(" 2>")
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_shell_risks() {
        assert_eq!(classify_shell("rg foo src"), RiskLevel::ReadOnly);
        assert_eq!(classify_shell("cargo test"), RiskLevel::WorkspaceWrite);
        assert_eq!(classify_shell("git status --short"), RiskLevel::ReadOnly);
        assert_eq!(classify_shell("git stash"), RiskLevel::WorkspaceWrite);
        assert_eq!(
            classify_shell("python -c 'open(\"x\", \"w\")'"),
            RiskLevel::WorkspaceWrite
        );
        assert_eq!(
            classify_shell("printf hi > result.txt"),
            RiskLevel::WorkspaceWrite
        );
        assert_eq!(
            classify_shell("git commit -m fix"),
            RiskLevel::WorkspaceWrite
        );
        assert_eq!(classify_shell("git push origin main"), RiskLevel::Elevated);
        assert_eq!(
            classify_shell("curl https://example.com"),
            RiskLevel::Elevated
        );
    }

    #[tokio::test]
    async fn session_grant_skips_repeated_review() {
        let (client, mut requests) = ApprovalClient::channel();
        let first = client.prepare(
            ApprovalKind::Shell,
            RiskLevel::Elevated,
            "network",
            "curl example.com",
            "shell:curl example.com",
        );
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.request(first).await }
        });
        requests
            .recv()
            .await
            .unwrap()
            .resolve(ApprovalDecision::AllowSession);
        assert_eq!(task.await.unwrap(), ApprovalDecision::AllowSession);

        let repeated = client.prepare(
            ApprovalKind::Shell,
            RiskLevel::Elevated,
            "network",
            "curl example.com",
            "shell:curl example.com",
        );
        assert_eq!(
            client.request(repeated).await,
            ApprovalDecision::AllowSession
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_grant_reuses_an_ssh_host_scope() {
        let (client, mut requests) = ApprovalClient::channel();
        let first = client.prepare(
            ApprovalKind::Shell,
            RiskLevel::Elevated,
            "inspect host",
            "ssh -o BatchMode=yes -o ConnectTimeout=8 5090-2 uptime",
            "shell:ssh -o BatchMode=yes -o ConnectTimeout=8 5090-2 uptime",
        );
        assert_eq!(first.session_scope.key, "shell:ssh:5090-2");
        assert!(first.session_scope.label.contains("ssh 5090-2"));
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.request(first).await }
        });
        requests
            .recv()
            .await
            .unwrap()
            .resolve(ApprovalDecision::AllowSession);
        assert_eq!(task.await.unwrap(), ApprovalDecision::AllowSession);

        let second = client.prepare(
            ApprovalKind::Shell,
            RiskLevel::Elevated,
            "inspect GPU",
            "ssh 5090-2 nvidia-smi",
            "shell:ssh 5090-2 nvidia-smi",
        );
        assert_eq!(client.request(second).await, ApprovalDecision::AllowSession);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn destructive_session_scope_stays_exact() {
        let scope = shell_session_scope(
            RiskLevel::Elevated,
            "shell:git push origin main",
            "git push origin main",
        );
        assert_eq!(scope.key, "shell:git push origin main");
        assert!(scope.label.starts_with("only `git push"));

        let compound = shell_session_scope(
            RiskLevel::WorkspaceWrite,
            "shell:echo ok && rm output",
            "echo ok && rm output",
        );
        assert_eq!(compound.key, "shell:echo ok && rm output");
    }
}
