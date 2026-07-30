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
    pub parent_session_id: Option<String>,
    pub checkpoint_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCheckpoint {
    pub id: String,
    pub label: String,
    pub message_count: usize,
    pub created_at_ms: u64,
    pub automatic: bool,
    state_digest: [u8; 32],
}

pub struct ChatSession {
    summary: SessionSummary,
    persisted_messages: Vec<Message>,
    checkpoints: Vec<SessionCheckpoint>,
    workspace: PathBuf,
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
    Snapshot {
        timestamp_ms: u64,
        messages: Vec<Message>,
    },
    Rename {
        timestamp_ms: u64,
        title: String,
    },
    Checkpoint {
        timestamp_ms: u64,
        id: String,
        label: String,
        message_count: usize,
        automatic: bool,
    },
    Fork {
        timestamp_ms: u64,
        source_session_id: String,
        source_message_count: usize,
        source_checkpoint_id: Option<String>,
    },
}

struct LoadedSession {
    summary: SessionSummary,
    workspace: PathBuf,
    messages: Vec<Message>,
    checkpoints: Vec<SessionCheckpoint>,
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
                parent_session_id: None,
                checkpoint_count: 0,
            },
            persisted_messages: Vec::new(),
            checkpoints: Vec::new(),
            workspace: workspace.to_path_buf(),
        })
    }

    pub fn resume(
        state_directory: &Path,
        workspace: &Path,
        selector: Option<&str>,
    ) -> Result<(Self, Conversation)> {
        let sessions = load_sessions(state_directory, workspace)?;
        let loaded = select_session(sessions, selector)?;
        let persisted_messages = loaded.messages.clone();
        let conversation = Conversation::from_messages(loaded.messages);
        Ok((
            Self {
                summary: loaded.summary,
                persisted_messages,
                checkpoints: loaded.checkpoints,
                workspace: loaded.workspace,
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
        let messages = conversation.messages();
        let entries = if messages.starts_with(&self.persisted_messages) {
            messages[self.persisted_messages.len()..]
                .iter()
                .cloned()
                .map(|message| SessionEntry::Message {
                    timestamp_ms: timestamp_ms(),
                    message,
                })
                .collect::<Vec<_>>()
        } else {
            vec![SessionEntry::Snapshot {
                timestamp_ms: timestamp_ms(),
                messages: messages.to_vec(),
            }]
        };
        if entries.is_empty() {
            return Ok(());
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&self.summary.path)
            .with_context(|| format!("failed to append session {}", self.summary.path.display()))?;
        write_entries(file, &entries)?;
        self.persisted_messages = messages.to_vec();
        self.summary.message_count = self.persisted_messages.len();
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

    pub fn checkpoint(
        &mut self,
        label: Option<&str>,
        conversation: &Conversation,
        automatic: bool,
    ) -> Result<SessionCheckpoint> {
        self.save(conversation)?;
        let message_count = conversation.message_count();
        let state_digest = message_digest(conversation.messages());
        if automatic
            && let Some(checkpoint) = self
                .checkpoints
                .last()
                .filter(|checkpoint| checkpoint.state_digest == state_digest)
        {
            return Ok(checkpoint.clone());
        }
        let sequence = self.checkpoints.len().saturating_add(1);
        let checkpoint = SessionCheckpoint {
            id: format!("cp-{sequence:04}"),
            label: normalized_checkpoint_label(label, sequence),
            message_count,
            created_at_ms: timestamp_ms(),
            automatic,
            state_digest,
        };
        let file = OpenOptions::new()
            .append(true)
            .open(&self.summary.path)
            .with_context(|| format!("failed to append session {}", self.summary.path.display()))?;
        write_entries(
            file,
            &[SessionEntry::Checkpoint {
                timestamp_ms: checkpoint.created_at_ms,
                id: checkpoint.id.clone(),
                label: checkpoint.label.clone(),
                message_count,
                automatic,
            }],
        )?;
        self.checkpoints.push(checkpoint.clone());
        self.summary.checkpoint_count = self.checkpoints.len();
        self.summary.updated_at_ms = checkpoint.created_at_ms;
        Ok(checkpoint)
    }

    pub fn checkpoints(&self) -> &[SessionCheckpoint] {
        &self.checkpoints
    }

    pub fn fork(
        &self,
        state_directory: &Path,
        conversation: &Conversation,
        selector: Option<&str>,
    ) -> Result<(Self, Conversation)> {
        let checkpoint = selector
            .map(|selector| select_checkpoint(&self.checkpoints, selector))
            .transpose()?;
        match checkpoint {
            Some(checkpoint) => {
                let messages = load_checkpoint_messages(&self.summary.path, &checkpoint.id)?;
                self.fork_at(state_directory, &messages, Some(&checkpoint))
            }
            None => self.fork_at(state_directory, conversation.messages(), None),
        }
    }

    pub fn rewind(
        &self,
        state_directory: &Path,
        conversation: &Conversation,
        selector: Option<&str>,
    ) -> Result<(Self, Conversation)> {
        let checkpoint = match selector {
            Some(selector) => select_checkpoint(&self.checkpoints, selector)?,
            None => self
                .checkpoints
                .iter()
                .rev()
                .find(|checkpoint| checkpoint.message_count < conversation.message_count())
                .cloned()
                .context("no earlier checkpoint exists in this session")?,
        };
        let messages = load_checkpoint_messages(&self.summary.path, &checkpoint.id)?;
        self.fork_at(state_directory, &messages, Some(&checkpoint))
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

    fn fork_at(
        &self,
        state_directory: &Path,
        messages: &[Message],
        checkpoint: Option<&SessionCheckpoint>,
    ) -> Result<(Self, Conversation)> {
        let forked_conversation = Conversation::from_messages(messages.to_vec());
        let mut forked = Self::create(
            state_directory,
            &self.workspace,
            &self.summary.provider,
            &self.summary.model,
        )?;
        let forked_at = timestamp_ms();
        let file = OpenOptions::new()
            .append(true)
            .open(&forked.summary.path)
            .with_context(|| {
                format!("failed to append session {}", forked.summary.path.display())
            })?;
        write_entries(
            file,
            &[SessionEntry::Fork {
                timestamp_ms: forked_at,
                source_session_id: self.summary.id.clone(),
                source_message_count: messages.len(),
                source_checkpoint_id: checkpoint.map(|checkpoint| checkpoint.id.clone()),
            }],
        )?;
        forked.summary.parent_session_id = Some(self.summary.id.clone());
        if let Some(title) = &self.summary.title {
            forked.rename(&fork_title(title))?;
        }
        forked.save(&forked_conversation)?;
        for source in self
            .checkpoints
            .iter()
            .filter(|source| checkpoint_matches_prefix(source, messages))
        {
            forked.checkpoint(
                Some(&source.label),
                &Conversation::from_messages(messages[..source.message_count].to_vec()),
                source.automatic,
            )?;
        }
        Ok((forked, forked_conversation))
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
    let mut checkpoints: Vec<SessionCheckpoint> = Vec::new();
    let mut parent_session_id = None;
    let mut fork_source_message_count = None;
    let mut fork_source_restored = false;
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
                fork_source_restored |=
                    fork_source_message_count.is_some_and(|count| count <= messages.len());
            }
            SessionEntry::Snapshot {
                timestamp_ms,
                messages: next_messages,
            } => {
                updated_at_ms = updated_at_ms.max(timestamp_ms);
                messages = next_messages;
                fork_source_restored |=
                    fork_source_message_count.is_some_and(|count| count <= messages.len());
            }
            SessionEntry::Rename {
                timestamp_ms,
                title: next_title,
            } => {
                updated_at_ms = updated_at_ms.max(timestamp_ms);
                title = Some(next_title);
            }
            SessionEntry::Checkpoint {
                timestamp_ms,
                id,
                label,
                message_count,
                automatic,
            } => {
                if message_count > messages.len() {
                    bail!(
                        "checkpoint {id:?} points to {message_count} messages, but only {} precede it",
                        messages.len()
                    );
                }
                if checkpoints.iter().any(|checkpoint| checkpoint.id == id) {
                    bail!("duplicate checkpoint ID {id:?}");
                }
                updated_at_ms = updated_at_ms.max(timestamp_ms);
                checkpoints.push(SessionCheckpoint {
                    id,
                    label,
                    message_count,
                    created_at_ms: timestamp_ms,
                    automatic,
                    state_digest: message_digest(&messages[..message_count]),
                });
            }
            SessionEntry::Fork {
                timestamp_ms,
                source_session_id,
                source_message_count,
                ..
            } => {
                if parent_session_id.replace(source_session_id).is_some() {
                    bail!("duplicate session fork record");
                }
                fork_source_message_count = Some(source_message_count);
                fork_source_restored = source_message_count == 0;
                updated_at_ms = updated_at_ms.max(timestamp_ms);
            }
            SessionEntry::Session { .. } => bail!("duplicate session header"),
        }
    }
    if fork_source_message_count.is_some() && !fork_source_restored {
        bail!("session fork record points past the restored conversation");
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
            parent_session_id,
            checkpoint_count: checkpoints.len(),
        },
        workspace,
        messages,
        checkpoints,
    })
}

fn load_checkpoint_messages(path: &Path, checkpoint_id: &str) -> Result<Vec<Message>> {
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
    if !matches!(
        serde_json::from_str::<SessionEntry>(&header).context("invalid session header")?,
        SessionEntry::Session {
            version: SESSION_VERSION,
            ..
        }
    ) {
        bail!("first session record is not a supported header");
    }

    let mut messages = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line.with_context(|| format!("failed to read session line {}", index + 2))?;
        match serde_json::from_str::<SessionEntry>(&line)
            .with_context(|| format!("invalid session record at line {}", index + 2))?
        {
            SessionEntry::Message { message, .. } => messages.push(message),
            SessionEntry::Snapshot {
                messages: next_messages,
                ..
            } => messages = next_messages,
            SessionEntry::Checkpoint {
                id, message_count, ..
            } if id == checkpoint_id => {
                if message_count > messages.len() {
                    bail!("checkpoint {id:?} points past the restored conversation");
                }
                return Ok(messages[..message_count].to_vec());
            }
            _ => {}
        }
    }
    bail!("checkpoint {checkpoint_id:?} no longer exists in this session")
}

fn checkpoint_matches_prefix(checkpoint: &SessionCheckpoint, messages: &[Message]) -> bool {
    checkpoint.message_count <= messages.len()
        && checkpoint.state_digest == message_digest(&messages[..checkpoint.message_count])
}

fn message_digest(messages: &[Message]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(
        u64::try_from(messages.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for message in messages {
        hasher.update([match message.role {
            crate::context::Role::User => 0,
            crate::context::Role::Assistant => 1,
        }]);
        hasher.update(
            u64::try_from(message.content.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(message.content.as_bytes());
        hasher.update(
            u64::try_from(message.images.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        for image in &message.images {
            for value in [&image.media_type, &image.name, &image.data] {
                hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
    }
    hasher.finalize().into()
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

fn normalized_checkpoint_label(label: Option<&str>, sequence: usize) -> String {
    let label = label.map(normalized_title).unwrap_or_default();
    if label.is_empty() {
        format!("checkpoint-{sequence}")
    } else {
        label.chars().take(96).collect()
    }
}

fn select_checkpoint(
    checkpoints: &[SessionCheckpoint],
    selector: &str,
) -> Result<SessionCheckpoint> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("checkpoint selector cannot be empty");
    }
    let selector_lower = selector.to_ascii_lowercase();
    let mut matches = checkpoints
        .iter()
        .filter(|checkpoint| {
            checkpoint.id.starts_with(selector)
                || checkpoint.label.to_ascii_lowercase() == selector_lower
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        0 => bail!("no checkpoint matching {selector:?} exists in this session"),
        1 => Ok(matches.remove(0)),
        _ => bail!("checkpoint selector {selector:?} is ambiguous; use its full ID"),
    }
}

fn fork_title(title: &str) -> String {
    let base = title
        .strip_suffix(" (fork)")
        .unwrap_or(title)
        .chars()
        .take(64)
        .collect::<String>();
    format!("{base} (fork)")
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
    use crate::context::ImageAttachment;

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

    #[test]
    fn image_messages_round_trip_through_resume_checkpoint_and_fork() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();
        let image = ImageAttachment {
            media_type: "image/png".into(),
            data: "YWJj".into(),
            name: "screen.png".into(),
        };
        let conversation = Conversation::from_messages(vec![
            Message::user_with_images("inspect", vec![image]),
            Message::assistant("done"),
        ]);
        session.save(&conversation).unwrap();
        let checkpoint = session
            .checkpoint(Some("with image"), &conversation, false)
            .unwrap();

        let (_, resumed) =
            ChatSession::resume(&state, &workspace, Some(&session.summary().id[..8])).unwrap();
        assert_eq!(resumed, conversation);
        let (_, forked) = session
            .fork(&state, &conversation, Some(&checkpoint.id))
            .unwrap();
        assert_eq!(forked, conversation);
        assert_eq!(forked.messages()[0].images[0].data, "YWJj");
    }

    #[test]
    fn checkpoints_fork_and_rewind_without_rewriting_source_history() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut source = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();
        source.rename("parser work").unwrap();
        let mut conversation = Conversation::default();
        let root = source
            .checkpoint(Some("before first task"), &conversation, true)
            .unwrap();
        conversation = Conversation::from_messages(vec![
            Message::user("Task:\nfirst"),
            Message::assistant(r#"{"action":"finish","summary":"first"}"#),
        ]);
        source.save(&conversation).unwrap();
        let middle = source
            .checkpoint(Some("after first task"), &conversation, false)
            .unwrap();
        conversation = Conversation::from_messages(vec![
            Message::user("Task:\nfirst"),
            Message::assistant(r#"{"action":"finish","summary":"first"}"#),
            Message::user("Follow-up request:\nsecond"),
            Message::assistant(r#"{"action":"finish","summary":"second"}"#),
        ]);
        source.save(&conversation).unwrap();
        let source_before = std::fs::read(&source.summary().path).unwrap();

        let (forked, forked_conversation) = source
            .fork(&state, &conversation, Some(&middle.id))
            .unwrap();
        assert_eq!(forked_conversation.message_count(), 2);
        assert_eq!(
            forked.summary().parent_session_id.as_deref(),
            Some(source.summary().id.as_str())
        );
        assert_eq!(
            forked.summary().title.as_deref(),
            Some("parser work (fork)")
        );

        let (rewound, rewound_conversation) = source
            .rewind(&state, &conversation, Some(&root.id))
            .unwrap();
        assert_eq!(rewound_conversation.message_count(), 0);
        assert_eq!(
            rewound.summary().parent_session_id.as_deref(),
            Some(source.summary().id.as_str())
        );
        assert_eq!(
            std::fs::read(&source.summary().path).unwrap(),
            source_before
        );

        let (resumed, resumed_conversation) =
            ChatSession::resume(&state, &workspace, Some(&forked.summary().id[..8])).unwrap();
        assert_eq!(resumed_conversation, forked_conversation);
        assert_eq!(
            resumed.summary().parent_session_id.as_deref(),
            Some(source.summary().id.as_str())
        );
        assert_eq!(resumed.checkpoints().len(), 2);
    }

    #[test]
    fn rewind_defaults_to_the_latest_earlier_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut source = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();
        let empty = Conversation::default();
        source.checkpoint(Some("start"), &empty, true).unwrap();
        let conversation =
            Conversation::from_messages(vec![Message::user("task"), Message::assistant("done")]);
        source.save(&conversation).unwrap();

        let (_, rewound) = source.rewind(&state, &conversation, None).unwrap();
        assert_eq!(rewound.message_count(), 0);
    }

    #[test]
    fn snapshots_preserve_checkpoints_when_compaction_replaces_messages() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = ChatSession::create(&state, &workspace, "openai", "gpt-test").unwrap();
        let original = Conversation::from_messages(vec![
            Message::user("Task:\nfix parser"),
            Message::assistant(r#"{"action":"read_file","path":"src/parser.rs"}"#),
            Message::user("old parser contents"),
            Message::assistant(r#"{"action":"finish","summary":"inspected"}"#),
        ]);
        session.save(&original).unwrap();
        let before = session
            .checkpoint(Some("before compaction"), &original, false)
            .unwrap();

        let compacted = Conversation::from_messages(vec![
            Message::user("Task:\nfix parser"),
            Message::user("[wecode-context-summary-v1]\nFiles and edits:\n- Inspected parser."),
            Message::assistant(r#"{"action":"finish","summary":"done"}"#),
        ]);
        session.save(&compacted).unwrap();
        session
            .checkpoint(Some("after compaction"), &compacted, true)
            .unwrap();

        let raw = std::fs::read_to_string(&session.summary().path).unwrap();
        assert!(raw.contains(r#""type":"snapshot""#));
        let (resumed, resumed_conversation) =
            ChatSession::resume(&state, &workspace, Some(&session.summary().id[..8])).unwrap();
        assert_eq!(resumed_conversation, compacted);
        assert_eq!(resumed.checkpoints().len(), 2);

        let (mut rewound, rewound_conversation) = resumed
            .rewind(&state, &resumed_conversation, Some(&before.id))
            .unwrap();
        assert_eq!(rewound_conversation, original);
        rewound.save(&compacted).unwrap();
        let (_, resumed_child) =
            ChatSession::resume(&state, &workspace, Some(&rewound.summary().id[..8])).unwrap();
        assert_eq!(resumed_child, compacted);
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
