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
        ApprovalRequest {
            id: self
                .inner
                .next_id
                .fetch_add(1, Ordering::AcqRel)
                .saturating_add(1),
            kind,
            risk,
            summary: summary.into(),
            detail: detail.into(),
            fingerprint: fingerprint.into(),
        }
    }

    pub async fn request(&self, request: ApprovalRequest) -> ApprovalDecision {
        if self
            .inner
            .session_grants
            .lock()
            .expect("approval grants lock poisoned")
            .contains(&request.fingerprint)
        {
            return ApprovalDecision::AllowSession;
        }
        let fingerprint = request.fingerprint.clone();
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
                .insert(fingerprint);
        }
        decision
    }
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
}
