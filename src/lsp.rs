use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

use crate::config::{LspConfig, LspServerConfig};
use crate::executor::scrub_secret_environment;
use crate::protocol::LspOperation;

const MAX_DIAGNOSTICS_PER_FILE: usize = 20;
const MAX_DIAGNOSTIC_NOTIFICATIONS: usize = 64;
const MAX_SEEN_DIAGNOSTICS: usize = 512;
const MAX_DIAGNOSTIC_FILES: usize = 512;
const MAX_OPEN_DOCUMENTS: usize = 1_024;

type PendingResponse = oneshot::Sender<std::result::Result<Value, String>>;
type SharedWriter = Arc<Mutex<Pin<Box<dyn AsyncWrite + Send>>>>;
type LspEventHandler = Arc<dyn Fn(LspEvent) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspServerStatus {
    Available,
    Starting,
    Ready,
    Failed,
    Stopped,
}

impl LspServerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LspServerSummary {
    pub name: String,
    pub command: String,
    pub status: LspServerStatus,
    pub extensions: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LspEvent {
    pub server: String,
    pub status: LspServerStatus,
    pub detail: String,
}

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<ManagerInner>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LspManager")
            .field("enabled", &self.inner.config.enabled)
            .field("workspace", &self.inner.workspace)
            .finish_non_exhaustive()
    }
}

struct ManagerInner {
    config: LspConfig,
    workspace: PathBuf,
    secret_env: Option<String>,
    state: Mutex<ManagerState>,
    hub: Arc<NotificationHub>,
}

struct ManagerState {
    definitions: Vec<ServerDefinition>,
    clients: HashMap<String, Arc<LspClient>>,
    failures: HashMap<String, String>,
    stopped: bool,
}

#[derive(Clone)]
struct ServerDefinition {
    name: String,
    config: LspServerConfig,
    detected: bool,
}

struct NotificationHub {
    queue: StdMutex<VecDeque<String>>,
    seen: StdMutex<VecDeque<String>>,
    event_handler: StdMutex<Option<LspEventHandler>>,
    max_queue_bytes: usize,
}

struct LspClient {
    name: String,
    writer: SharedWriter,
    child: Mutex<Child>,
    pending: Arc<StdMutex<HashMap<u64, PendingResponse>>>,
    next_id: AtomicU64,
    open_documents: Mutex<HashMap<PathBuf, i32>>,
    diagnostics: Arc<StdMutex<BTreeMap<String, Vec<Value>>>>,
    reader_task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    stderr_task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    request_timeout: Duration,
    max_message_bytes: usize,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Some(task) = self
            .reader_task
            .lock()
            .expect("lsp reader task lock poisoned")
            .take()
        {
            task.abort();
        }
        if let Some(task) = self
            .stderr_task
            .lock()
            .expect("lsp stderr task lock poisoned")
            .take()
        {
            task.abort();
        }
    }
}

impl LspManager {
    pub fn new(config: LspConfig, workspace: PathBuf, secret_env: Option<String>) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("workspace {} does not exist", workspace.display()))?;
        let definitions = discover_servers(&config);
        let max_queue_bytes = config.max_output_bytes;
        Ok(Self {
            inner: Arc::new(ManagerInner {
                config,
                workspace,
                secret_env,
                state: Mutex::new(ManagerState {
                    definitions,
                    clients: HashMap::new(),
                    failures: HashMap::new(),
                    stopped: false,
                }),
                hub: Arc::new(NotificationHub {
                    queue: StdMutex::new(VecDeque::new()),
                    seen: StdMutex::new(VecDeque::new()),
                    event_handler: StdMutex::new(None),
                    max_queue_bytes,
                }),
            }),
        })
    }

    pub fn set_event_handler<F>(&self, handler: F)
    where
        F: Fn(LspEvent) + Send + Sync + 'static,
    {
        *self
            .inner
            .hub
            .event_handler
            .lock()
            .expect("lsp event handler lock poisoned") = Some(Arc::new(handler));
    }

    pub async fn execute(
        &self,
        operation: LspOperation,
        path: &str,
        line: Option<u32>,
        character: Option<u32>,
        query: Option<&str>,
    ) -> Result<String> {
        if !self.inner.config.enabled {
            bail!("LSP is disabled in config");
        }
        let path = self.resolve_source_file(path)?;
        let (definition, language_id) = self
            .definition_for_path(&path)
            .await
            .with_context(|| format!("no installed or configured LSP server handles {path:?}"))?;
        let client = self.ensure_client(&definition).await?;
        client
            .sync_document(&path, &language_id, self.inner.config.max_file_bytes)
            .await?;

        let result = match operation {
            LspOperation::GoToDefinition => {
                client
                    .request(
                        "textDocument/definition",
                        position_params(&path, required_position(line, character)?),
                    )
                    .await?
            }
            LspOperation::FindReferences => {
                let mut params = position_params(&path, required_position(line, character)?);
                params["context"] = json!({"includeDeclaration": true});
                client.request("textDocument/references", params).await?
            }
            LspOperation::Hover => {
                client
                    .request(
                        "textDocument/hover",
                        position_params(&path, required_position(line, character)?),
                    )
                    .await?
            }
            LspOperation::DocumentSymbols => {
                client
                    .request(
                        "textDocument/documentSymbol",
                        json!({"textDocument": {"uri": file_uri(&path)?}}),
                    )
                    .await?
            }
            LspOperation::WorkspaceSymbols => {
                client
                    .request("workspace/symbol", json!({"query": query.unwrap_or("")}))
                    .await?
            }
            LspOperation::GoToImplementation => {
                client
                    .request(
                        "textDocument/implementation",
                        position_params(&path, required_position(line, character)?),
                    )
                    .await?
            }
            LspOperation::PrepareCallHierarchy => {
                client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        position_params(&path, required_position(line, character)?),
                    )
                    .await?
            }
            LspOperation::IncomingCalls | LspOperation::OutgoingCalls => {
                let prepared = client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        position_params(&path, required_position(line, character)?),
                    )
                    .await?;
                let Some(item) = prepared.as_array().and_then(|items| items.first()).cloned()
                else {
                    return Ok("No call hierarchy item found.".into());
                };
                let method = if operation == LspOperation::IncomingCalls {
                    "callHierarchy/incomingCalls"
                } else {
                    "callHierarchy/outgoingCalls"
                };
                client.request(method, json!({"item": item})).await?
            }
            LspOperation::Diagnostics => {
                tokio::time::sleep(Duration::from_millis(
                    self.inner.config.diagnostic_settle_milliseconds,
                ))
                .await;
                let diagnostics = client.diagnostics_for_path(&path);
                return Ok(bound_output(
                    &format_diagnostics(&path, &diagnostics),
                    self.inner.config.max_output_bytes,
                ));
            }
        };
        Ok(bound_output(
            &format_lsp_result(operation, &result, &self.inner.workspace),
            self.inner.config.max_output_bytes,
        ))
    }

    pub async fn sync_paths(&self, paths: &[PathBuf]) {
        if !self.inner.config.enabled {
            return;
        }
        for path in paths {
            let display = path.to_string_lossy();
            let Ok(path) = self.resolve_source_file(&display) else {
                continue;
            };
            let Some((definition, language_id)) = self.definition_for_path(&path).await else {
                continue;
            };
            match self.ensure_client(&definition).await {
                Ok(client) => {
                    let _ = client
                        .sync_document(&path, &language_id, self.inner.config.max_file_bytes)
                        .await;
                }
                Err(_) => continue,
            }
        }
    }

    pub async fn summaries(&self) -> Vec<LspServerSummary> {
        let state = self.inner.state.lock().await;
        state
            .definitions
            .iter()
            .map(|definition| {
                let status = if state.clients.contains_key(&definition.name) {
                    LspServerStatus::Ready
                } else if state.stopped {
                    LspServerStatus::Stopped
                } else if state.failures.contains_key(&definition.name) {
                    LspServerStatus::Failed
                } else {
                    LspServerStatus::Available
                };
                LspServerSummary {
                    name: definition.name.clone(),
                    command: definition.config.command.clone(),
                    status,
                    extensions: definition.config.extensions.keys().cloned().collect(),
                    error: state.failures.get(&definition.name).cloned(),
                }
            })
            .collect()
    }

    pub fn take_notifications(&self) -> Vec<String> {
        self.inner
            .hub
            .queue
            .lock()
            .expect("lsp notification lock poisoned")
            .drain(..)
            .collect()
    }

    pub async fn restart(&self) {
        self.shutdown_clients().await;
        let mut state = self.inner.state.lock().await;
        state.stopped = false;
        state.failures.clear();
        drop(state);
        self.inner.hub.clear_seen();
    }

    pub async fn shutdown(&self) {
        self.shutdown_clients().await;
        self.inner.state.lock().await.stopped = true;
    }

    async fn shutdown_clients(&self) {
        let clients = {
            let mut state = self.inner.state.lock().await;
            state
                .clients
                .drain()
                .map(|(_, client)| client)
                .collect::<Vec<_>>()
        };
        for client in clients {
            client.shutdown().await;
        }
    }

    async fn definition_for_path(&self, path: &Path) -> Option<(ServerDefinition, String)> {
        let extension = extension_key(path)?;
        let state = self.inner.state.lock().await;
        state.definitions.iter().find_map(|definition| {
            definition
                .config
                .extensions
                .get(&extension)
                .map(|language| (definition.clone(), language.clone()))
        })
    }

    async fn ensure_client(&self, definition: &ServerDefinition) -> Result<Arc<LspClient>> {
        let existing = {
            let state = self.inner.state.lock().await;
            if state.stopped {
                bail!("LSP manager is stopped");
            }
            state.clients.get(&definition.name).cloned()
        };
        if let Some(client) = existing {
            if client.is_alive().await? {
                return Ok(client);
            }
            let mut state = self.inner.state.lock().await;
            if state
                .clients
                .get(&definition.name)
                .is_some_and(|current| Arc::ptr_eq(current, &client))
            {
                state.clients.remove(&definition.name);
            }
            drop(state);
            client.force_stop().await;
        }

        self.inner.hub.event(LspEvent {
            server: definition.name.clone(),
            status: LspServerStatus::Starting,
            detail: format!("starting {}", definition.config.command),
        });
        let client = LspClient::start(
            definition,
            &self.inner.workspace,
            &self.inner.config,
            self.inner.secret_env.as_deref(),
            self.inner.hub.clone(),
        )
        .await;
        let mut state = self.inner.state.lock().await;
        match client {
            Ok(client) => {
                state.failures.remove(&definition.name);
                state
                    .clients
                    .insert(definition.name.clone(), client.clone());
                self.inner.hub.event(LspEvent {
                    server: definition.name.clone(),
                    status: LspServerStatus::Ready,
                    detail: if definition.detected {
                        "ready · auto-detected".into()
                    } else {
                        "ready · configured".into()
                    },
                });
                Ok(client)
            }
            Err(error) => {
                let message = error.to_string();
                state
                    .failures
                    .insert(definition.name.clone(), message.clone());
                self.inner.hub.event(LspEvent {
                    server: definition.name.clone(),
                    status: LspServerStatus::Failed,
                    detail: message,
                });
                Err(error)
            }
        }
    }

    fn resolve_source_file(&self, input: &str) -> Result<PathBuf> {
        let candidate = Path::new(input);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.inner.workspace.join(candidate)
        };
        let path = candidate
            .canonicalize()
            .with_context(|| format!("source file {} does not exist", candidate.display()))?;
        if !path.starts_with(&self.inner.workspace) {
            bail!("LSP path escapes the workspace: {}", candidate.display());
        }
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !metadata.is_file() {
            bail!("LSP path is not a file: {}", path.display());
        }
        if metadata.len() > self.inner.config.max_file_bytes as u64 {
            bail!(
                "LSP source file is {} bytes; limit is {}",
                metadata.len(),
                self.inner.config.max_file_bytes
            );
        }
        Ok(path)
    }
}

impl NotificationHub {
    fn event(&self, event: LspEvent) {
        let handler = self
            .event_handler
            .lock()
            .expect("lsp event handler lock poisoned")
            .clone();
        if let Some(handler) = handler {
            handler(event);
        }
    }

    fn diagnostics(&self, server: &str, uri: &str, diagnostics: &[Value]) {
        let text = format_diagnostic_notification(server, uri, diagnostics);
        let Some(text) = text else {
            return;
        };
        let text = bound_output(&text, self.max_queue_bytes);
        {
            let mut seen = self.seen.lock().expect("lsp diagnostic seen lock poisoned");
            if seen.iter().any(|previous| previous == &text) {
                return;
            }
            if seen.len() >= MAX_SEEN_DIAGNOSTICS {
                seen.pop_front();
            }
            seen.push_back(text.clone());
        }
        let mut queue = self.queue.lock().expect("lsp notification lock poisoned");
        let mut queued_bytes = queue.iter().map(String::len).sum::<usize>();
        while queue.len() >= MAX_DIAGNOSTIC_NOTIFICATIONS
            || queued_bytes.saturating_add(text.len()) > self.max_queue_bytes
        {
            let Some(evicted) = queue.pop_front() else {
                break;
            };
            queued_bytes = queued_bytes.saturating_sub(evicted.len());
        }
        queue.push_back(text.clone());
        drop(queue);
        self.event(LspEvent {
            server: server.into(),
            status: LspServerStatus::Ready,
            detail: one_line(&text, 240),
        });
    }

    fn clear_seen(&self) {
        self.seen
            .lock()
            .expect("lsp diagnostic seen lock poisoned")
            .clear();
        self.queue
            .lock()
            .expect("lsp notification lock poisoned")
            .clear();
    }
}

impl LspClient {
    async fn start(
        definition: &ServerDefinition,
        workspace: &Path,
        manager_config: &LspConfig,
        secret_env: Option<&str>,
        hub: Arc<NotificationHub>,
    ) -> Result<Arc<Self>> {
        let mut command = Command::new(&definition.config.command);
        command
            .args(&definition.config.args)
            .current_dir(workspace)
            .envs(&definition.config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        scrub_secret_environment(&mut command, secret_env);
        crate::process_tree::configure(&mut command);
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start LSP server {:?} with command {:?}",
                definition.name, definition.config.command
            )
        })?;
        let stdin = child.stdin.take().context("LSP server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("LSP server stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("LSP server stderr unavailable")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            Box::pin(stdin) as Pin<Box<dyn AsyncWrite + Send>>
        ));
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let diagnostics = Arc::new(StdMutex::new(BTreeMap::new()));
        let client = Arc::new(Self {
            name: definition.name.clone(),
            writer: writer.clone(),
            child: Mutex::new(child),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            open_documents: Mutex::new(HashMap::new()),
            diagnostics: diagnostics.clone(),
            reader_task: StdMutex::new(None),
            stderr_task: StdMutex::new(None),
            request_timeout: Duration::from_secs(manager_config.request_timeout_seconds),
            max_message_bytes: manager_config.max_message_bytes,
        });
        let reader_server = definition.name.clone();
        let max_message_bytes = manager_config.max_message_bytes;
        let reader_task = tokio::spawn(read_messages(
            stdout,
            writer,
            pending,
            diagnostics,
            hub,
            reader_server,
            max_message_bytes,
            workspace.to_path_buf(),
        ));
        *client
            .reader_task
            .lock()
            .expect("lsp reader task lock poisoned") = Some(reader_task);
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buffer = [0_u8; 4_096];
            while let Ok(read) = stderr.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
            }
        });
        *client
            .stderr_task
            .lock()
            .expect("lsp stderr task lock poisoned") = Some(stderr_task);

        let root_uri = file_uri(workspace)?;
        let initialize = client.request_with_timeout(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "WeCode", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")}],
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "symbol": {"dynamicRegistration": false}
                    },
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false, "didSave": true},
                        "definition": {"dynamicRegistration": false, "linkSupport": true},
                        "references": {"dynamicRegistration": false},
                        "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
                        "documentSymbol": {"dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true},
                        "implementation": {"dynamicRegistration": false, "linkSupport": true},
                        "callHierarchy": {"dynamicRegistration": false},
                        "publishDiagnostics": {"relatedInformation": true, "versionSupport": true}
                    }
                },
                "initializationOptions": definition.config.initialization_options
            }),
            Duration::from_secs(definition.config.startup_timeout_seconds),
        );
        if let Err(error) = initialize.await {
            client.force_stop().await;
            return Err(error).context("LSP initialize failed");
        }
        client.notify("initialized", json!({})).await?;
        if let Some(settings) = &definition.config.settings {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({"settings": settings}),
                )
                .await?;
        }
        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.request_timeout)
            .await
    }

    async fn is_alive(&self) -> Result<bool> {
        Ok(self.child.lock().await.try_wait()?.is_none())
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        {
            let mut child = self.child.lock().await;
            if child.try_wait()?.is_some() {
                bail!("LSP server {:?} has exited", self.name);
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("lsp pending response lock poisoned")
            .insert(id, sender);
        if let Err(error) = write_json(
            &self.writer,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            self.max_message_bytes,
        )
        .await
        {
            self.pending
                .lock()
                .expect("lsp pending response lock poisoned")
                .remove(&id);
            return Err(error);
        }
        let response = tokio::time::timeout(timeout, receiver)
            .await
            .with_context(|| format!("LSP request {method:?} timed out after {timeout:?}"))?
            .context("LSP response channel closed")?;
        response.map_err(anyhow::Error::msg)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_json(
            &self.writer,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
            self.max_message_bytes,
        )
        .await
    }

    async fn sync_document(
        &self,
        path: &Path,
        language_id: &str,
        max_file_bytes: usize,
    ) -> Result<()> {
        let metadata = tokio::fs::metadata(path).await?;
        if metadata.len() > max_file_bytes as u64 {
            bail!(
                "LSP source file is {} bytes; limit is {max_file_bytes}",
                metadata.len()
            );
        }
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("LSP source file {} is not UTF-8", path.display()))?;
        let uri = file_uri(path)?;
        let mut documents = self.open_documents.lock().await;
        if let Some(version) = documents.get_mut(path) {
            *version = version.saturating_add(1);
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": *version},
                    "contentChanges": [{"text": content}]
                }),
            )
            .await?;
        } else {
            if documents.len() >= MAX_OPEN_DOCUMENTS {
                bail!("LSP open-document limit of {MAX_OPEN_DOCUMENTS} reached");
            }
            documents.insert(path.to_path_buf(), 1);
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": content
                    }
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn diagnostics_for_path(&self, path: &Path) -> Vec<Value> {
        let Ok(uri) = file_uri(path) else {
            return Vec::new();
        };
        self.diagnostics
            .lock()
            .expect("lsp diagnostics lock poisoned")
            .get(&uri)
            .cloned()
            .unwrap_or_default()
    }

    async fn shutdown(&self) {
        let _ = self
            .request_with_timeout("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        self.force_stop().await;
    }

    async fn force_stop(&self) {
        let mut child = self.child.lock().await;
        crate::process_tree::terminate(&mut child).await;
        if let Some(task) = self
            .reader_task
            .lock()
            .expect("lsp reader task lock poisoned")
            .take()
        {
            task.abort();
        }
        if let Some(task) = self
            .stderr_task
            .lock()
            .expect("lsp stderr task lock poisoned")
            .take()
        {
            task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_messages(
    stdout: impl AsyncRead + Unpin,
    writer: SharedWriter,
    pending: Arc<StdMutex<HashMap<u64, PendingResponse>>>,
    diagnostics: Arc<StdMutex<BTreeMap<String, Vec<Value>>>>,
    hub: Arc<NotificationHub>,
    server: String,
    max_message_bytes: usize,
    workspace: PathBuf,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_message(&mut reader, max_message_bytes).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                fail_pending(&pending, error.to_string());
                break;
            }
        };
        if let Some(id) = message.get("id").and_then(Value::as_u64)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            let response = if let Some(error) = message.get("error") {
                Err(format_json_rpc_error(error))
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            if let Some(sender) = pending
                .lock()
                .expect("lsp pending response lock poisoned")
                .remove(&id)
            {
                let _ = sender.send(response);
            }
            continue;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        if method == "textDocument/publishDiagnostics" {
            let uri = message
                .pointer("/params/uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let values = message
                .pointer("/params/diagnostics")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let values = prioritized_diagnostics(values);
            let mut stored = diagnostics.lock().expect("lsp diagnostics lock poisoned");
            if stored.contains_key(&uri) || stored.len() < MAX_DIAGNOSTIC_FILES {
                stored.insert(uri.clone(), values.clone());
            }
            drop(stored);
            hub.diagnostics(&server, &display_uri(&uri, &workspace), &values);
            continue;
        }
        if let Some(id) = message.get("id").cloned() {
            let result = match method {
                "workspace/configuration" => {
                    let count = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    Value::Array((0..count).map(|_| Value::Null).collect())
                }
                "workspace/workspaceFolders" => Value::Array(vec![json!({
                    "uri": file_uri(&workspace).unwrap_or_default(),
                    "name": workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
                })]),
                "workspace/applyEdit" => {
                    json!({"applied": false, "failureReason": "WeCode does not allow server-initiated edits"})
                }
                _ => Value::Null,
            };
            let _ = write_json(
                &writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                max_message_bytes,
            )
            .await;
        }
    }
    fail_pending(&pending, format!("LSP server {server:?} closed stdout"));
}

async fn read_message(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_message_bytes: usize,
) -> Result<Option<Value>> {
    let mut content_length = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return if saw_header {
                Err(anyhow::anyhow!(
                    "LSP server closed in the middle of a header"
                ))
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            bail!("malformed LSP header");
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid LSP Content-Length")?,
            );
        }
    }
    let length = content_length.context("LSP message has no Content-Length")?;
    if length == 0 || length > max_message_bytes {
        bail!("LSP message length {length} exceeds limit {max_message_bytes}");
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid LSP JSON message")
}

async fn write_json(writer: &SharedWriter, value: &Value, max_message_bytes: usize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > max_message_bytes {
        bail!(
            "outgoing LSP message is {} bytes; limit is {max_message_bytes}",
            body.len()
        );
    }
    let mut writer = writer.lock().await;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

fn fail_pending(pending: &StdMutex<HashMap<u64, PendingResponse>>, reason: String) {
    let pending = std::mem::take(&mut *pending.lock().expect("lsp pending response lock poisoned"));
    for (_, sender) in pending {
        let _ = sender.send(Err(reason.clone()));
    }
}

fn discover_servers(config: &LspConfig) -> Vec<ServerDefinition> {
    if !config.enabled {
        return Vec::new();
    }
    let mut definitions = config
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, server)| ServerDefinition {
            name: name.clone(),
            config: server.clone(),
            detected: false,
        })
        .collect::<Vec<_>>();
    if !config.auto_detect {
        return definitions;
    }
    let mut claimed = definitions
        .iter()
        .flat_map(|definition| definition.config.extensions.keys().cloned())
        .collect::<HashSet<_>>();
    for mut definition in builtin_servers() {
        if !command_exists(&definition.config.command) {
            continue;
        }
        definition
            .config
            .extensions
            .retain(|extension, _| claimed.insert(extension.clone()));
        if !definition.config.extensions.is_empty() {
            definitions.push(definition);
        }
    }
    definitions
}

fn builtin_servers() -> Vec<ServerDefinition> {
    [
        (
            "rust-analyzer",
            "rust-analyzer",
            &[][..],
            &[(".rs", "rust")][..],
        ),
        (
            "typescript",
            "typescript-language-server",
            &["--stdio"][..],
            &[
                (".ts", "typescript"),
                (".tsx", "typescriptreact"),
                (".js", "javascript"),
                (".jsx", "javascriptreact"),
                (".mts", "typescript"),
                (".cts", "typescript"),
                (".mjs", "javascript"),
                (".cjs", "javascript"),
            ][..],
        ),
        (
            "basedpyright",
            "basedpyright-langserver",
            &["--stdio"][..],
            &[(".py", "python"), (".pyi", "python")][..],
        ),
        (
            "pyright",
            "pyright-langserver",
            &["--stdio"][..],
            &[(".py", "python"), (".pyi", "python")][..],
        ),
        ("gopls", "gopls", &[][..], &[(".go", "go")][..]),
        (
            "clangd",
            "clangd",
            &[][..],
            &[
                (".c", "c"),
                (".h", "c"),
                (".cc", "cpp"),
                (".cpp", "cpp"),
                (".cxx", "cpp"),
                (".hpp", "cpp"),
            ][..],
        ),
        (
            "sourcekit-lsp",
            "sourcekit-lsp",
            &[][..],
            &[
                (".swift", "swift"),
                (".m", "objective-c"),
                (".mm", "objective-cpp"),
            ][..],
        ),
        (
            "lua",
            "lua-language-server",
            &[][..],
            &[(".lua", "lua")][..],
        ),
        (
            "zls",
            "zls",
            &[][..],
            &[(".zig", "zig"), (".zon", "zig")][..],
        ),
        ("nil", "nil", &[][..], &[(".nix", "nix")][..]),
    ]
    .into_iter()
    .map(|(name, command, args, extensions)| ServerDefinition {
        name: name.into(),
        config: LspServerConfig {
            command: command.into(),
            args: args.iter().map(|argument| (*argument).into()).collect(),
            extensions: extensions
                .iter()
                .map(|(extension, language)| ((*extension).into(), (*language).into()))
                .collect::<BTreeMap<_, _>>(),
            ..Default::default()
        },
        detected: true,
    })
    .collect()
}

fn command_exists(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    #[cfg(windows)]
    let extensions = env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![".EXE".into(), ".CMD".into(), ".BAT".into()]);
    env::split_paths(&paths).any(|directory| {
        let candidate = directory.join(command);
        if candidate.is_file() {
            return true;
        }
        #[cfg(windows)]
        {
            extensions
                .iter()
                .any(|extension| directory.join(format!("{command}{extension}")).is_file())
        }
        #[cfg(not(windows))]
        false
    })
}

fn extension_key(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
}

fn required_position(line: Option<u32>, character: Option<u32>) -> Result<(u32, u32)> {
    let line = line
        .filter(|line| *line > 0)
        .context("LSP line is required")?;
    let character = character
        .filter(|character| *character > 0)
        .context("LSP character is required")?;
    Ok((line - 1, character - 1))
}

fn position_params(path: &Path, (line, character): (u32, u32)) -> Value {
    json!({
        "textDocument": {"uri": file_uri(path).unwrap_or_default()},
        "position": {"line": line, "character": character}
    })
}

fn file_uri(path: &Path) -> Result<String> {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow::anyhow!("cannot convert {} to a file URI", path.display()))
}

fn display_uri(uri: &str, workspace: &Path) -> String {
    reqwest::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map(|path| {
            path.strip_prefix(workspace)
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/")
        })
        .unwrap_or_else(|| uri.to_owned())
}

fn prioritized_diagnostics(mut diagnostics: Vec<Value>) -> Vec<Value> {
    diagnostics.sort_by_key(|diagnostic| {
        diagnostic
            .get("severity")
            .and_then(Value::as_u64)
            .unwrap_or(4)
    });
    diagnostics.truncate(MAX_DIAGNOSTICS_PER_FILE);
    diagnostics
}

fn format_diagnostic_notification(
    server: &str,
    display_path: &str,
    diagnostics: &[Value],
) -> Option<String> {
    let rendered = render_diagnostics(diagnostics);
    if rendered.is_empty() {
        return None;
    }
    Some(format!(
        "LSP DIAGNOSTICS · {server}\nfile: {display_path}\n{}",
        rendered.join("\n")
    ))
}

fn format_diagnostics(path: &Path, diagnostics: &[Value]) -> String {
    let rendered = render_diagnostics(diagnostics);
    if rendered.is_empty() {
        format!("No error or warning diagnostics for {}.", path.display())
    } else {
        format!(
            "LSP diagnostics for {}:\n{}",
            path.display(),
            rendered.join("\n")
        )
    }
}

fn render_diagnostics(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let severity = diagnostic
                .get("severity")
                .and_then(Value::as_u64)
                .unwrap_or(3);
            let label = match severity {
                1 => "error",
                2 => "warning",
                _ => return None,
            };
            let line = diagnostic
                .pointer("/range/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            let character = diagnostic
                .pointer("/range/start/character")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            let source = diagnostic
                .get("source")
                .and_then(Value::as_str)
                .map(|source| format!("[{source}] "))
                .unwrap_or_default();
            let code = diagnostic
                .get("code")
                .filter(|code| code.is_string() || code.is_number())
                .map(|code| format!(" ({code})"))
                .unwrap_or_default();
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown diagnostic")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!(
                "  {label} {line}:{character} · {source}{message}{code}"
            ))
        })
        .collect()
}

fn format_lsp_result(operation: LspOperation, result: &Value, workspace: &Path) -> String {
    if result.is_null() || result.as_array().is_some_and(Vec::is_empty) {
        return format!("No results found for {}.", operation.as_str());
    }
    match operation {
        LspOperation::GoToDefinition
        | LspOperation::FindReferences
        | LspOperation::GoToImplementation => {
            let locations = collect_locations(result, workspace);
            if locations.is_empty() {
                pretty_json(result)
            } else {
                locations.join("\n")
            }
        }
        LspOperation::Hover => format_hover(result).unwrap_or_else(|| pretty_json(result)),
        LspOperation::DocumentSymbols | LspOperation::WorkspaceSymbols => {
            let mut symbols = Vec::new();
            collect_symbols(result, workspace, 0, &mut symbols);
            if symbols.is_empty() {
                pretty_json(result)
            } else {
                symbols.join("\n")
            }
        }
        LspOperation::PrepareCallHierarchy
        | LspOperation::IncomingCalls
        | LspOperation::OutgoingCalls => format_call_items(result, workspace),
        LspOperation::Diagnostics => unreachable!("diagnostics are formatted before this function"),
    }
}

fn collect_locations(value: &Value, workspace: &Path) -> Vec<String> {
    let values = value
        .as_array()
        .map_or_else(|| vec![value], |values| values.iter().collect());
    values
        .into_iter()
        .filter_map(|location| {
            let uri = location
                .get("uri")
                .or_else(|| location.get("targetUri"))
                .and_then(Value::as_str)?;
            let range = location
                .get("range")
                .or_else(|| location.get("targetSelectionRange"))
                .or_else(|| location.get("targetRange"))?;
            let line = range.pointer("/start/line").and_then(Value::as_u64)? + 1;
            let character = range.pointer("/start/character").and_then(Value::as_u64)? + 1;
            Some(format!(
                "{}:{line}:{character}",
                display_uri(uri, workspace)
            ))
        })
        .collect()
}

fn format_hover(value: &Value) -> Option<String> {
    let contents = value.get("contents")?;
    if let Some(text) = contents.as_str() {
        return Some(text.to_owned());
    }
    if let Some(text) = contents.get("value").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    contents.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| {
                item.as_str().map(ToOwned::to_owned).or_else(|| {
                    item.get("value")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn collect_symbols(value: &Value, workspace: &Path, depth: usize, output: &mut Vec<String>) {
    let Some(symbols) = value.as_array() else {
        return;
    };
    for symbol in symbols {
        let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol.get("kind").and_then(Value::as_u64).unwrap_or(0);
        let uri = symbol
            .pointer("/location/uri")
            .or_else(|| symbol.get("uri"))
            .and_then(Value::as_str);
        let range = symbol
            .pointer("/location/range")
            .or_else(|| symbol.get("selectionRange"))
            .or_else(|| symbol.get("range"));
        let location = match (uri, range) {
            (Some(uri), Some(range)) => {
                let line = range
                    .pointer("/start/line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    + 1;
                format!(" · {}:{line}", display_uri(uri, workspace))
            }
            _ => String::new(),
        };
        output.push(format!(
            "{}kind {kind} · {name}{location}",
            "  ".repeat(depth.min(8))
        ));
        if let Some(children) = symbol.get("children") {
            collect_symbols(children, workspace, depth + 1, output);
        }
    }
}

fn format_call_items(value: &Value, workspace: &Path) -> String {
    let Some(items) = value.as_array() else {
        return pretty_json(value);
    };
    let lines = items
        .iter()
        .filter_map(|wrapper| {
            let item = wrapper
                .get("from")
                .or_else(|| wrapper.get("to"))
                .unwrap_or(wrapper);
            let name = item.get("name").and_then(Value::as_str)?;
            let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
            let line = item
                .pointer("/selectionRange/start/line")
                .or_else(|| item.pointer("/range/start/line"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            Some(format!("{name} · {}:{line}", display_uri(uri, workspace)))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        pretty_json(value)
    } else {
        lines.join("\n")
    }
}

fn format_json_rpc_error(error: &Value) -> String {
    let code = error
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown LSP error");
    format!("LSP error {code}: {message}")
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn bound_output(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let notice = format!(
        "\n\n[LSP output truncated: {} bytes omitted]",
        value.len().saturating_sub(limit)
    );
    let budget = limit.saturating_sub(notice.len());
    let head_budget = budget / 2;
    let tail_budget = budget.saturating_sub(head_budget);
    let head_end = previous_char_boundary(value, head_budget);
    let tail_start = next_char_boundary(value, value.len().saturating_sub(tail_budget));
    format!("{}{}{}", &value[..head_end], notice, &value[tail_start..])
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn one_line(value: &str, limit: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= limit {
        value
    } else {
        let mut output = value
            .chars()
            .take(limit.saturating_sub(1))
            .collect::<String>();
        output.push('…');
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn reads_framed_messages_and_enforces_limits() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(body);
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(
            read_message(&mut reader, 1024).await.unwrap().unwrap(),
            json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})
        );

        let mut reader = BufReader::new(b"Content-Length: 5000\r\n\r\n".as_slice());
        assert!(read_message(&mut reader, 1024).await.is_err());
    }

    #[tokio::test]
    async fn routes_responses_and_deduplicates_bounded_diagnostics() {
        let (client_stream, mut server_stream) = tokio::io::duplex(64 * 1_024);
        let (client_reader, client_writer) = tokio::io::split(client_stream);
        let writer: SharedWriter = Arc::new(Mutex::new(
            Box::pin(client_writer) as Pin<Box<dyn AsyncWrite + Send>>
        ));
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let diagnostics = Arc::new(StdMutex::new(BTreeMap::new()));
        let hub = Arc::new(NotificationHub {
            queue: StdMutex::new(VecDeque::new()),
            seen: StdMutex::new(VecDeque::new()),
            event_handler: StdMutex::new(None),
            max_queue_bytes: 4_096,
        });
        let (response_tx, response_rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, response_tx);
        let task = tokio::spawn(read_messages(
            client_reader,
            writer,
            pending,
            diagnostics.clone(),
            hub.clone(),
            "fixture".into(),
            64 * 1_024,
            PathBuf::from("/workspace"),
        ));
        for message in [
            json!({"jsonrpc":"2.0","id":7,"result":{"value":"ok"}}),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{
                    "uri":"file:///workspace/src/main.rs",
                    "diagnostics":[{
                        "severity":1,
                        "range":{"start":{"line":2,"character":4}},
                        "message":"broken"
                    }]
                }
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{
                    "uri":"file:///workspace/src/main.rs",
                    "diagnostics":[{
                        "severity":1,
                        "range":{"start":{"line":2,"character":4}},
                        "message":"broken"
                    }]
                }
            }),
        ] {
            let body = serde_json::to_vec(&message).unwrap();
            server_stream
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            server_stream.write_all(&body).await.unwrap();
        }
        server_stream.shutdown().await.unwrap();
        assert_eq!(response_rx.await.unwrap().unwrap(), json!({"value":"ok"}));
        task.await.unwrap();
        assert_eq!(hub.queue.lock().unwrap().len(), 1);
        assert_eq!(
            diagnostics
                .lock()
                .unwrap()
                .get("file:///workspace/src/main.rs")
                .unwrap()
                .len(),
            1
        );
        for index in 0..10 {
            hub.diagnostics(
                "fixture",
                &format!("src/file-{index}.rs"),
                &[json!({
                    "severity": 1,
                    "range": {"start": {"line": 0, "character": 0}},
                    "message": format!("{index}:{}", "x".repeat(2_000))
                })],
            );
        }
        assert!(
            hub.queue
                .lock()
                .unwrap()
                .iter()
                .map(String::len)
                .sum::<usize>()
                <= hub.max_queue_bytes
        );
    }

    #[test]
    fn prioritizes_and_formats_diagnostics_with_one_based_positions() {
        let diagnostics = prioritized_diagnostics(vec![
            json!({"severity": 2, "range": {"start": {"line": 4, "character": 2}}, "message": "warn"}),
            json!({"severity": 1, "range": {"start": {"line": 1, "character": 0}}, "message": "bad", "source": "rustc"}),
            json!({"severity": 3, "range": {"start": {"line": 0, "character": 0}}, "message": "info"}),
        ]);
        assert_eq!(
            render_diagnostics(&diagnostics),
            vec!["  error 2:1 · [rustc] bad", "  warning 5:3 · warn",]
        );
    }

    #[test]
    fn configured_servers_override_auto_detected_extensions() {
        let config = LspConfig {
            auto_detect: false,
            servers: BTreeMap::from([(
                "custom".into(),
                LspServerConfig {
                    command: "custom-lsp".into(),
                    extensions: BTreeMap::from([(".rs".into(), "rust".into())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let servers = discover_servers(&config);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "custom");
        assert!(!servers[0].detected);
    }

    #[test]
    fn result_formatters_make_locations_and_hover_compact() {
        let workspace = if cfg!(windows) {
            Path::new("C:\\repo")
        } else {
            Path::new("/repo")
        };
        let uri = if cfg!(windows) {
            "file:///C:/repo/src/main.rs"
        } else {
            "file:///repo/src/main.rs"
        };
        assert!(
            format_lsp_result(
                LspOperation::GoToDefinition,
                &json!([{"uri": uri, "range": {"start": {"line": 8, "character": 3}}}]),
                workspace,
            )
            .ends_with("src/main.rs:9:4")
        );
        assert_eq!(
            format_lsp_result(
                LspOperation::Hover,
                &json!({"contents": {"kind": "markdown", "value": "`fn main()`"}}),
                workspace,
            ),
            "`fn main()`"
        );
    }

    #[tokio::test]
    async fn queries_a_real_clangd_when_available() {
        if !command_exists("clangd") {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("sample.c"),
            "static int add(int a, int b) { return a + b; }\nint main(void) { return add(1, 2); }\n",
        )
        .unwrap();
        let config = LspConfig {
            auto_detect: false,
            servers: BTreeMap::from([(
                "clangd".into(),
                LspServerConfig {
                    command: "clangd".into(),
                    extensions: BTreeMap::from([(".c".into(), "c".into())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let manager = LspManager::new(config, workspace.path().to_path_buf(), None).unwrap();
        let output = manager
            .execute(LspOperation::DocumentSymbols, "sample.c", None, None, None)
            .await
            .unwrap();
        assert!(output.contains("main"));
        assert!(output.contains("add"));
        manager.shutdown().await;
    }
}
