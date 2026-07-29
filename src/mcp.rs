use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::{McpConfig, McpServerConfig};

const PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_TOOLS_PER_SERVER: usize = 256;
const MAX_LIST_PAGES: usize = 32;
const MAX_TOOL_CATALOG_BYTES: usize = 512 * 1_024;
const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1_024;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 2 * 1_024;
const STDERR_LIMIT: usize = 64 * 1_024;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerState {
    Connected,
    Disabled,
    Failed,
}

#[derive(Clone, Debug)]
pub struct McpServerReport {
    pub name: String,
    pub state: McpServerState,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct McpTool {
    pub model_name: String,
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
}

impl McpTool {
    pub fn definition(&self) -> Value {
        json!({
            "name": self.model_name,
            "description": self.description,
            "parameters": self.input_schema,
        })
    }
}

#[derive(Clone, Debug)]
pub struct McpCallOutput {
    pub observation: String,
    pub is_error: bool,
    pub duration_ms: u128,
    pub truncated_bytes: usize,
}

#[derive(Clone)]
pub struct McpManager {
    inner: Arc<McpManagerInner>,
}

struct McpManagerInner {
    servers: BTreeMap<String, McpServerEntry>,
}

struct McpServerEntry {
    report: McpServerReport,
    client: Option<Arc<McpClient>>,
    tools: BTreeMap<String, McpTool>,
}

impl McpManager {
    pub async fn connect(config: &McpConfig, workspace: &Path) -> Self {
        Self::connect_with_secret_env(config, workspace, None).await
    }

    pub async fn connect_with_secret_env(
        config: &McpConfig,
        workspace: &Path,
        extra_secret_env: Option<&str>,
    ) -> Self {
        let extra_secret_env = extra_secret_env
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
        let attempts = config.servers.iter().map(|(name, server)| {
            let name = name.clone();
            let server = server.clone();
            let workspace = workspace.to_path_buf();
            let extra_secret_env = extra_secret_env.clone();
            async move {
                if !server.enabled {
                    return (
                        name.clone(),
                        McpServerEntry {
                            report: McpServerReport {
                                name,
                                state: McpServerState::Disabled,
                                tools: Vec::new(),
                                error: None,
                            },
                            client: None,
                            tools: BTreeMap::new(),
                        },
                    );
                }
                match McpClient::start(&name, &server, &workspace, extra_secret_env.as_deref())
                    .await
                {
                    Ok((client, tools)) => {
                        let tool_names = tools.keys().cloned().collect();
                        (
                            name.clone(),
                            McpServerEntry {
                                report: McpServerReport {
                                    name,
                                    state: McpServerState::Connected,
                                    tools: tool_names,
                                    error: None,
                                },
                                client: Some(Arc::new(client)),
                                tools,
                            },
                        )
                    }
                    Err(error) => (
                        name.clone(),
                        McpServerEntry {
                            report: McpServerReport {
                                name,
                                state: McpServerState::Failed,
                                tools: Vec::new(),
                                error: Some(format!("{error:#}")),
                            },
                            client: None,
                            tools: BTreeMap::new(),
                        },
                    ),
                }
            }
        });
        Self {
            inner: Arc::new(McpManagerInner {
                servers: join_all(attempts).await.into_iter().collect(),
            }),
        }
    }

    pub fn reports(&self) -> Vec<McpServerReport> {
        self.inner
            .servers
            .values()
            .map(|server| server.report.clone())
            .collect()
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.inner
            .servers
            .values()
            .flat_map(|server| server.tools.values().cloned())
            .collect()
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools()
            .into_iter()
            .map(|tool| tool.definition())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.servers.is_empty()
    }

    pub fn tool_is_read_only(&self, server: &str, tool: &str) -> bool {
        self.inner
            .servers
            .get(server)
            .and_then(|entry| entry.tools.get(tool))
            .is_some_and(|tool| tool.read_only)
    }

    pub async fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<McpCallOutput> {
        let entry = self
            .inner
            .servers
            .get(server)
            .with_context(|| format!("MCP server {server:?} is not configured"))?;
        if !entry.tools.contains_key(tool) {
            bail!("MCP server {server:?} does not expose tool {tool:?}");
        }
        let client = entry
            .client
            .as_ref()
            .with_context(|| format!("MCP server {server:?} is not connected"))?;
        client.call(tool, arguments).await
    }

    pub async fn shutdown(&self) {
        let clients = self
            .inner
            .servers
            .values()
            .filter_map(|entry| entry.client.clone())
            .collect::<Vec<_>>();
        for client in clients {
            client.shutdown().await;
        }
    }
}

struct McpClient {
    server_name: String,
    state: Mutex<McpClientState>,
    tool_timeout: Duration,
    max_output_bytes: usize,
}

struct McpClientState {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    stderr: Arc<StdMutex<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<Result<()>>,
    redactions: Vec<String>,
}

impl McpClient {
    async fn start(
        server_name: &str,
        config: &McpServerConfig,
        workspace: &Path,
        extra_secret_env: Option<&str>,
    ) -> Result<(Self, BTreeMap<String, McpTool>)> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in SECRET_ENV_VARS {
            command.env_remove(name);
        }
        if let Some(name) = extra_secret_env {
            command.env_remove(name);
        }
        command.envs(&config.env);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start MCP server {server_name:?}"))?;
        let stdin = child
            .stdin
            .take()
            .context("MCP server child stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP server child stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("MCP server child stderr unavailable")?;
        let captured_stderr = Arc::new(StdMutex::new(Vec::new()));
        let stderr_task = {
            let captured = captured_stderr.clone();
            tokio::spawn(async move { capture_stderr(stderr, captured).await })
        };
        let client = Self {
            server_name: server_name.to_owned(),
            state: Mutex::new(McpClientState {
                child,
                stdin: BufWriter::new(stdin),
                stdout: BufReader::new(stdout),
                next_id: 0,
                stderr: captured_stderr,
                stderr_task,
                redactions: config
                    .env
                    .values()
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .collect(),
            }),
            tool_timeout: Duration::from_secs(config.tool_timeout_seconds),
            max_output_bytes: config.max_output_bytes,
        };
        let startup_timeout = Duration::from_secs(config.startup_timeout_seconds);
        client.initialize(startup_timeout).await?;
        let tools = client.list_tools(startup_timeout).await?;
        Ok((client, tools))
    }

    async fn initialize(&self, timeout: Duration) -> Result<()> {
        let result = self
            .request_with_timeout(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "wecode",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
                timeout,
            )
            .await
            .with_context(|| format!("MCP server {:?} initialization failed", self.server_name))?;
        if !result.is_object() {
            bail!("MCP initialize result must be an object");
        }
        self.notify("notifications/initialized", None).await
    }

    async fn list_tools(&self, timeout: Duration) -> Result<BTreeMap<String, McpTool>> {
        let mut cursor: Option<String> = None;
        let mut tools = BTreeMap::new();
        let mut seen_cursors = HashSet::new();
        let mut catalog_bytes = 0_usize;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let result = self
                .request_with_timeout("tools/list", params, timeout)
                .await?;
            let listed = result
                .get("tools")
                .and_then(Value::as_array)
                .context("MCP tools/list result did not contain a tools array")?;
            for value in listed {
                if tools.len() >= MAX_TOOLS_PER_SERVER {
                    bail!(
                        "MCP server {:?} exposed more than {MAX_TOOLS_PER_SERVER} tools",
                        self.server_name
                    );
                }
                catalog_bytes = catalog_bytes.saturating_add(serde_json::to_vec(value)?.len());
                if catalog_bytes > MAX_TOOL_CATALOG_BYTES {
                    bail!(
                        "MCP server {:?} tool catalog exceeded {MAX_TOOL_CATALOG_BYTES} bytes",
                        self.server_name
                    );
                }
                let tool = parse_tool(&self.server_name, value)?;
                if tools.insert(tool.name.clone(), tool).is_some() {
                    bail!(
                        "MCP server {:?} exposed a duplicate tool name",
                        self.server_name
                    );
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .filter(|cursor| !cursor.is_empty());
            let Some(next) = &cursor else {
                return Ok(tools);
            };
            if next.len() > 1_024 {
                bail!(
                    "MCP server {:?} returned an oversized cursor",
                    self.server_name
                );
            }
            if !seen_cursors.insert(next.clone()) {
                bail!("MCP server {:?} repeated a list cursor", self.server_name);
            }
        }
        bail!(
            "MCP server {:?} exceeded the {MAX_LIST_PAGES}-page tool listing limit",
            self.server_name
        )
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<McpCallOutput> {
        let started = Instant::now();
        let result = self
            .request_with_timeout(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                self.tool_timeout,
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let raw = normalize_call_result(&result, is_error);
        let (observation, truncated_bytes) = truncate_middle(&raw, self.max_output_bytes);
        Ok(McpCallOutput {
            observation,
            is_error,
            duration_ms: started.elapsed().as_millis(),
            truncated_bytes,
        })
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let mut state = self.state.lock().await;
        match tokio::time::timeout(
            timeout,
            state.request(method, params, self.max_output_bytes),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let _ = state.child.kill().await;
                let stderr = state.stderr_text();
                bail!(
                    "MCP server {:?} timed out during {method}{}",
                    self.server_name,
                    diagnostic_suffix(&stderr)
                )
            }
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let mut state = self.state.lock().await;
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        state.write_message(&message).await
    }

    async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        let _ = state.child.kill().await;
        let _ = state.child.wait().await;
        state.stderr_task.abort();
    }
}

impl McpClientState {
    async fn request(&mut self, method: &str, params: Value, max_line: usize) -> Result<Value> {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        for _ in 0..128 {
            let value = self.read_message(max_line).await?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    bail!("MCP JSON-RPC error: {}", compact_json(error, 2_048));
                }
                return value
                    .get("result")
                    .cloned()
                    .context("MCP response did not contain result");
            }
            if value.get("method").is_some() && value.get("id").is_some() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": value["id"],
                    "error": {"code": -32601, "message": "client method not supported"}
                });
                self.write_message(&response).await?;
            }
        }
        bail!("MCP server sent too many unrelated messages");
    }

    async fn write_message(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_message(&mut self, max_line: usize) -> Result<Value> {
        let mut bytes = Vec::new();
        let read = (&mut self.stdout)
            .take(max_line.saturating_add(1) as u64)
            .read_until(b'\n', &mut bytes)
            .await?;
        if read == 0 {
            let stderr = self.stderr_text();
            bail!("MCP server closed stdout{}", diagnostic_suffix(&stderr));
        }
        if bytes.len() > max_line {
            bail!("MCP server response exceeded {max_line} bytes");
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        serde_json::from_slice(&bytes).context("MCP server emitted invalid JSON")
    }

    fn stderr_text(&self) -> String {
        let bytes = self.stderr.lock().expect("MCP stderr lock poisoned");
        let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
        for value in &self.redactions {
            text = text.replace(value, "[redacted]");
        }
        text
    }
}

fn parse_tool(server: &str, value: &Value) -> Result<McpTool> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .context("MCP tool is missing its name")?;
    validate_tool_name(name)?;
    let model_name = format!("mcp__{server}__{name}");
    if model_name.len() > 64 {
        bail!("namespaced MCP tool name {model_name:?} exceeds 64 bytes");
    }
    let input_schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    if !input_schema.is_object() {
        bail!("MCP tool {name:?} inputSchema must be an object");
    }
    if serde_json::to_vec(&input_schema)?.len() > MAX_TOOL_SCHEMA_BYTES {
        bail!("MCP tool {name:?} inputSchema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes");
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("Tool provided by an MCP server.")
        .trim();
    let description = if description.is_empty() {
        format!("Tool {name} provided by MCP server {server}.")
    } else {
        format!("[MCP: {server}] {description}")
    };
    let description = truncate_middle(&description, MAX_TOOL_DESCRIPTION_BYTES).0;
    let read_only = value
        .get("annotations")
        .and_then(|annotations| annotations.get("readOnlyHint"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(McpTool {
        model_name,
        server: server.to_owned(),
        name: name.to_owned(),
        description,
        input_schema,
        read_only,
    })
}

fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 48
        || name.contains("__")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!(
            "MCP tool name {name:?} must be 1-48 ASCII letters, digits, underscores, or hyphens and cannot contain \"__\""
        );
    }
    Ok(())
}

fn normalize_call_result(result: &Value, is_error: bool) -> String {
    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                    }
                }
                Some("resource") => {
                    if let Some(resource) = block.get("resource") {
                        let uri = resource
                            .get("uri")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown resource");
                        if let Some(text) = resource.get("text").and_then(Value::as_str) {
                            parts.push(format!("RESOURCE {uri}\n{text}"));
                        } else {
                            parts.push(format!("RESOURCE {uri} (binary content omitted)"));
                        }
                    }
                }
                Some("resource_link") => {
                    let uri = block
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown resource");
                    parts.push(format!("RESOURCE LINK {uri}"));
                }
                Some("image") | Some("audio") => {
                    let kind = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("media")
                        .to_ascii_uppercase();
                    let mime = block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown MIME type");
                    let encoded_bytes = block
                        .get("data")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    parts.push(format!(
                        "{kind} {mime} ({encoded_bytes} encoded bytes omitted)"
                    ));
                }
                Some(kind) => parts.push(format!("[unsupported MCP content type: {kind}]")),
                None => {}
            }
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        parts.push(format!(
            "STRUCTURED CONTENT\n{}",
            compact_json(structured, 32_768)
        ));
    }
    if parts.is_empty() {
        parts.push("(MCP tool returned no content)".into());
    }
    let body = parts.join("\n\n");
    if is_error {
        format!("MCP TOOL ERROR\n{body}")
    } else {
        format!("MCP TOOL RESULT\n{body}")
    }
}

async fn capture_stderr<R: tokio::io::AsyncRead + Unpin>(
    mut stderr: R,
    captured: Arc<StdMutex<Vec<u8>>>,
) -> Result<()> {
    let mut buffer = [0_u8; 4_096];
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let mut output = captured.lock().expect("MCP stderr lock poisoned");
        let remaining = STDERR_LIMIT.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

fn diagnostic_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; stderr: {}", stderr.replace(['\r', '\n'], " "))
    }
}

fn compact_json(value: &Value, max_bytes: usize) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "<invalid JSON>".into());
    truncate_middle(&raw, max_bytes).0
}

fn truncate_middle(value: &str, max_bytes: usize) -> (String, usize) {
    if value.len() <= max_bytes {
        return (value.to_owned(), 0);
    }
    let marker = "\n... MCP output truncated ...\n";
    let available = max_bytes.saturating_sub(marker.len());
    let mut head = available / 2;
    while head > 0 && !value.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = value.len().saturating_sub(available.saturating_sub(head));
    while tail_start < value.len() && !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start.saturating_sub(head);
    (
        format!("{}{marker}{}", &value[..head], &value[tail_start..]),
        omitted,
    )
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn parses_and_namespaces_valid_tool() {
        let tool = parse_tool(
            "files",
            &json!({
                "name": "read_text",
                "description": "Read text",
                "inputSchema": {"type": "object"},
                "annotations": {"readOnlyHint": true}
            }),
        )
        .unwrap();
        assert_eq!(tool.model_name, "mcp__files__read_text");
        assert!(tool.read_only);
    }

    #[test]
    fn invalid_or_oversized_tools_are_rejected() {
        assert!(parse_tool("files", &json!({"name": "bad.name"})).is_err());
        assert!(
            parse_tool(
                "server-name-that-is-already-quite-long",
                &json!({"name": "tool-name-that-is-also-much-too-long"})
            )
            .is_err()
        );
    }

    #[test]
    fn media_payloads_are_not_forwarded_to_the_model() {
        let result = json!({
            "content": [{
                "type": "image",
                "mimeType": "image/png",
                "data": "SECRET_BASE64_PAYLOAD"
            }]
        });
        let observation = normalize_call_result(&result, false);
        assert!(observation.contains("image/png"));
        assert!(!observation.contains("SECRET_BASE64_PAYLOAD"));
    }

    #[test]
    fn truncation_preserves_both_ends_and_utf8_boundaries() {
        let value = format!("start-{}-end", "世界".repeat(100));
        let (truncated, omitted) = truncate_middle(&value, 64);
        assert!(truncated.starts_with("start-"));
        assert!(truncated.ends_with("-end"));
        assert!(omitted > 0);
        assert!(truncated.len() <= 64);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_handshake_discovery_and_call_work_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fixture.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}}'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":true}}]}}'
      ;;
    *'"method":"tools/call"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"fixture-result"}],"isError":false}}'
      ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = McpConfig::default();
        config.servers.insert(
            "fixture".into(),
            McpServerConfig {
                command: script.display().to_string(),
                ..Default::default()
            },
        );
        let manager = McpManager::connect(&config, temp.path()).await;
        let reports = manager.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, McpServerState::Connected);
        assert_eq!(reports[0].tools, vec!["echo"]);
        assert_eq!(manager.definitions()[0]["name"], "mcp__fixture__echo");
        assert!(manager.tool_is_read_only("fixture", "echo"));

        let output = manager
            .call("fixture", "echo", json!({"text": "hello"}))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.observation.contains("fixture-result"));
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_tool_calls_have_a_hard_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("slow.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}'
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"slow","inputSchema":{"type":"object"}}]}}'
      ;;
    *'"method":"tools/call"'*)
      sleep 5
      ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = McpConfig::default();
        config.servers.insert(
            "slow".into(),
            McpServerConfig {
                command: script.display().to_string(),
                tool_timeout_seconds: 1,
                ..Default::default()
            },
        );
        let manager = McpManager::connect(&config, temp.path()).await;
        let started = Instant::now();
        let error = manager.call("slow", "slow", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
        manager.shutdown().await;
    }
}
