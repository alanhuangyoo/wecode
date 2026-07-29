use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::Conversation;
use crate::context::Message;

const SESSION_VERSION: u32 = 1;
const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub path: PathBuf,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: usize,
    pub provider: String,
    pub model: String,
}

pub struct ChatSession {
    summary: SessionSummary,
    saved_messages: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEntry {
    Session {
        version: u32,
        id: String,
        workspace: PathBuf,
        created_at_ms: u64,
        provider: String,
        model: String,
    },
    Message {
        timestamp_ms: u64,
        message: Message,
    },
    Rename {
        timestamp_ms: u64,
        title: String,
    },
}

struct LoadedSession {
    summary: SessionSummary,
    workspace: PathBuf,
    messages: Vec<Message>,
}

impl ChatSession {
    pub fn create(
        state_directory: &Path,
        workspace: &Path,
        provider: &str,
        model: &str,
    ) -> Result<Self> {
        let directory = session_directory(state_directory, workspace);
        create_private_directory(&directory)?;
        let created_at_ms = timestamp_ms();
        let id = new_session_id(workspace, created_at_ms);
        let path = directory.join(format!("{created_at_ms}-{id}.jsonl"));
        let entry = SessionEntry::Session {
            version: SESSION_VERSION,
            id: id.clone(),
            workspace: workspace.to_path_buf(),
            created_at_ms,
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        let file = create_private_file(&path)?;
        write_entries(file, std::slice::from_ref(&entry))?;
        Ok(Self {
            summary: SessionSummary {
                id,
                title: None,
                path,
                created_at_ms,
                updated_at_ms: created_at_ms,
                message_count: 0,
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            saved_messages: 0,
        })
    }

    pub fn resume(
        state_directory: &Path,
        workspace: &Path,
        selector: Option<&str>,
    ) -> Result<(Self, Conversation)> {
        let sessions = load_sessions(state_directory, workspace)?;
        let loaded = select_session(sessions, selector)?;
        let conversation = Conversation::from_messages(loaded.messages);
        let saved_messages = conversation.message_count();
        Ok((
            Self {
                summary: loaded.summary,
                saved_messages,
            },
            conversation,
        ))
    }

    pub fn list(state_directory: &Path, workspace: &Path) -> Result<Vec<SessionSummary>> {
        Ok(load_sessions(state_directory, workspace)?
            .into_iter()
            .map(|loaded| loaded.summary)
            .collect())
    }

    pub fn save(&mut self, conversation: &Conversation) -> Result<()> {
        if conversation.message_count() < self.saved_messages {
            bail!("cannot overwrite persisted session history");
        }
        let entries = conversation.messages()[self.saved_messages..]
            .iter()
            .cloned()
            .map(|message| SessionEntry::Message {
                timestamp_ms: timestamp_ms(),
                message,
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(());
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&self.summary.path)
            .with_context(|| format!("failed to append session {}", self.summary.path.display()))?;
        write_entries(file, &entries)?;
        self.saved_messages = conversation.message_count();
        self.summary.message_count = self.saved_messages;
        self.summary.updated_at_ms = timestamp_ms();
        Ok(())
    }

    pub fn rename(&mut self, title: &str) -> Result<()> {
        let title = normalized_title(title);
        if title.is_empty() {
            bail!("session title cannot be empty");
        }
        let timestamp_ms = timestamp_ms();
        let file = OpenOptions::new()
            .append(true)
            .open(&self.summary.path)
            .with_context(|| format!("failed to append session {}", self.summary.path.display()))?;
        write_entries(
            file,
            &[SessionEntry::Rename {
                timestamp_ms,
                title: title.clone(),
            }],
        )?;
        self.summary.title = Some(title);
        self.summary.updated_at_ms = timestamp_ms;
        Ok(())
    }

    pub fn set_initial_title(&mut self, task: &str) -> Result<()> {
        if self.summary.title.is_none() {
            self.rename(&task.chars().take(72).collect::<String>())?;
        }
        Ok(())
    }

    pub fn summary(&self) -> &SessionSummary {
        &self.summary
    }
}

fn load_sessions(state_directory: &Path, workspace: &Path) -> Result<Vec<LoadedSession>> {
    let directory = session_directory(state_directory, workspace);
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = std::fs::read_dir(&directory)
        .with_context(|| format!("failed to read session directory {}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|path| match load_session(&path) {
            Ok(session) if session.workspace == workspace => Some(session),
            Ok(_) => None,
            Err(error) => {
                eprintln!("warning: skipped session {}: {error:#}", path.display());
                None
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.summary.updated_at_ms));
    Ok(sessions)
}

fn load_session(path: &Path) -> Result<LoadedSession> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect session {}", path.display()))?;
    if metadata.len() > MAX_SESSION_BYTES {
        bail!(
            "session exceeds the {} MiB safety limit",
            MAX_SESSION_BYTES / 1_048_576
        );
    }
    let file =
        File::open(path).with_context(|| format!("failed to open session {}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let header = lines.next().context("session file is empty")??;
    let SessionEntry::Session {
        version,
        id,
        workspace,
        created_at_ms,
        provider,
        model,
    } = serde_json::from_str(&header).context("invalid session header")?
    else {
        bail!("first session record is not a header");
    };
    if version != SESSION_VERSION {
        bail!("unsupported session version {version}");
    }
    let mut title = None;
    let mut messages = Vec::new();
    let mut updated_at_ms = created_at_ms;
    for (index, line) in lines.enumerate() {
        let line = line.with_context(|| format!("failed to read session line {}", index + 2))?;
        let entry: SessionEntry = serde_json::from_str(&line)
            .with_context(|| format!("invalid session record at line {}", index + 2))?;
        match entry {
            SessionEntry::Message {
                timestamp_ms,
                message,
            } => {
                updated_at_ms = updated_at_ms.max(timestamp_ms);
                messages.push(message);
            }
            SessionEntry::Rename {
                timestamp_ms,
                title: next_title,
            } => {
                updated_at_ms = updated_at_ms.max(timestamp_ms);
                title = Some(next_title);
            }
            SessionEntry::Session { .. } => bail!("duplicate session header"),
        }
    }
    Ok(LoadedSession {
        summary: SessionSummary {
            id,
            title,
            path: path.to_path_buf(),
            created_at_ms,
            updated_at_ms,
            message_count: messages.len(),
            provider,
            model,
        },
        workspace,
        messages,
    })
}

fn select_session(
    mut sessions: Vec<LoadedSession>,
    selector: Option<&str>,
) -> Result<LoadedSession> {
    let Some(selector) = selector else {
        return sessions
            .into_iter()
            .next()
            .context("no saved session exists for this workspace");
    };
    let selector_lower = selector.to_ascii_lowercase();
    sessions.retain(|session| {
        session.summary.id.starts_with(selector)
            || session
                .summary
                .title
                .as_deref()
                .is_some_and(|title| title.to_ascii_lowercase() == selector_lower)
    });
    match sessions.len() {
        0 => bail!("no session matching {selector:?} exists for this workspace"),
        1 => Ok(sessions.remove(0)),
        _ => bail!("session selector {selector:?} is ambiguous; use a longer ID prefix"),
    }
}

fn session_directory(state_directory: &Path, workspace: &Path) -> PathBuf {
    let digest = format!(
        "{:x}",
        Sha256::digest(workspace.to_string_lossy().as_bytes())
    );
    state_directory.join("chat").join(&digest[..16])
}

fn new_session_id(workspace: &Path, created_at_ms: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    hasher.update(created_at_ms.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    format!("{:x}", hasher.finalize())[..24].to_owned()
}

fn normalized_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to create session {}", path.display()))
}

fn write_entries(file: File, entries: &[SessionEntry]) -> Result<()> {
    let mut writer = BufWriter::new(file);
    for entry in entries {
        serde_json::to_writer(&mut writer, entry)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer.get_ref().sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_lists_renames_and_resumes_a_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();
        let conversation = Conversation::from_messages(vec![
            Message::user("Task:\nfix parser"),
            Message::assistant(r#"{"action":"finish","summary":"done"}"#),
        ]);
        session.set_initial_title("fix parser").unwrap();
        session.save(&conversation).unwrap();

        let summaries = ChatSession::list(&state, &workspace).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title.as_deref(), Some("fix parser"));
        assert_eq!(summaries[0].message_count, 2);

        let (resumed, resumed_conversation) =
            ChatSession::resume(&state, &workspace, Some(&session.summary().id[..8])).unwrap();
        assert_eq!(resumed.summary().id, session.summary().id);
        assert_eq!(resumed_conversation, conversation);
    }

    #[cfg(unix)]
    #[test]
    fn session_storage_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let session = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();

        let directory = session.summary().path.parent().unwrap();
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&session.summary().path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
