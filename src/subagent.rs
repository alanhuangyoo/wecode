use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::agent::{Agent, Conversation, RunOptions, RunResult};
use crate::cache::ResponseCache;
use crate::config::{Config, ModelConfig, SubagentRoleConfig, SubagentsConfig};
use crate::control::CancellationToken;
use crate::events::{Event, EventSink};
use crate::input_queue::InputQueue;
use crate::model::{Model, ToolProfile, create_model_with_profile};

const MAX_NOTIFICATIONS: usize = 64;

type ModelFactory = Arc<dyn Fn(&ModelConfig, ToolProfile) -> Result<Box<dyn Model>> + Send + Sync>;
type EventHandler = Arc<dyn Fn(SubagentEvent) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SubagentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct SubagentSummary {
    pub id: u64,
    pub description: String,
    pub agent_type: String,
    pub model: String,
    pub status: SubagentStatus,
    pub turns: usize,
    pub duration_ms: u128,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SubagentEvent {
    pub id: u64,
    pub description: String,
    pub agent_type: String,
    pub status: SubagentStatus,
    pub detail: String,
}

#[derive(Clone)]
pub struct SubagentManager {
    inner: Arc<ManagerInner>,
}

impl std::fmt::Debug for SubagentManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentManager")
            .field("workspace", &self.inner.workspace)
            .field("enabled", &self.inner.settings.enabled)
            .finish_non_exhaustive()
    }
}

struct ManagerInner {
    config: Config,
    settings: SubagentsConfig,
    workspace: PathBuf,
    factory: ModelFactory,
    semaphore: Arc<Semaphore>,
    state: Mutex<ManagerState>,
    changed: Notify,
    hub: Arc<EventHub>,
}

struct ManagerState {
    next_id: u64,
    records: BTreeMap<u64, AgentRecord>,
    stopped: bool,
}

struct AgentRecord {
    id: u64,
    description: String,
    agent_type: String,
    model: String,
    status: SubagentStatus,
    turns: usize,
    started: Instant,
    duration_ms: u128,
    result: Option<String>,
    error: Option<String>,
    background: bool,
    generation: u64,
    cancellation: CancellationToken,
    input_queue: InputQueue,
    runtime: Option<SubagentRuntime>,
    handle: Option<JoinHandle<()>>,
}

struct SubagentRuntime {
    agent: Agent,
    conversation: Conversation,
    role_prompt: String,
}

struct RoleDefinition {
    name: String,
    description: String,
    prompt: String,
    read_only: bool,
    model: Option<String>,
    max_steps: Option<usize>,
}

struct EventHub {
    queue: StdMutex<VecDeque<String>>,
    handler: StdMutex<Option<EventHandler>>,
    max_output_bytes: usize,
}

struct SubagentSink {
    id: u64,
    description: String,
    agent_type: String,
    hub: Arc<EventHub>,
}

enum TaskOutcome {
    Finished {
        runtime: SubagentRuntime,
        result: RunResult,
    },
    Error {
        runtime: Option<SubagentRuntime>,
        message: String,
    },
    TimedOut,
}

impl SubagentManager {
    pub fn new(config: Config, workspace: PathBuf) -> Result<Self> {
        let factory_config = config.clone();
        let factory: ModelFactory = Arc::new(move |model_config, profile| {
            let mut key_config = factory_config.clone();
            key_config.model = model_config.clone();
            let api_key = key_config.api_key()?;
            let cache = ResponseCache::new(factory_config.cache.clone())?;
            create_model_with_profile(model_config, api_key, cache, profile)
        });
        Self::new_with_factory(config, workspace, factory)
    }

    pub fn new_with_model_factory<F>(config: Config, workspace: PathBuf, factory: F) -> Result<Self>
    where
        F: Fn(&ModelConfig, ToolProfile) -> Result<Box<dyn Model>> + Send + Sync + 'static,
    {
        Self::new_with_factory(config, workspace, Arc::new(factory))
    }

    fn new_with_factory(config: Config, workspace: PathBuf, factory: ModelFactory) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("workspace {} does not exist", workspace.display()))?;
        let settings = config.subagents.clone();
        Ok(Self {
            inner: Arc::new(ManagerInner {
                semaphore: Arc::new(Semaphore::new(settings.max_concurrent)),
                hub: Arc::new(EventHub {
                    queue: StdMutex::new(VecDeque::new()),
                    handler: StdMutex::new(None),
                    max_output_bytes: settings.max_output_bytes,
                }),
                config,
                settings,
                workspace,
                factory,
                state: Mutex::new(ManagerState {
                    next_id: 0,
                    records: BTreeMap::new(),
                    stopped: false,
                }),
                changed: Notify::new(),
            }),
        })
    }

    pub fn set_event_handler<F>(&self, handler: F)
    where
        F: Fn(SubagentEvent) + Send + Sync + 'static,
    {
        *self
            .inner
            .hub
            .handler
            .lock()
            .expect("subagent event handler lock poisoned") = Some(Arc::new(handler));
    }

    pub fn role_summaries(&self) -> Vec<(String, String, bool)> {
        effective_roles(&self.inner.settings)
            .into_values()
            .map(|role| (role.name, role.description, role.read_only))
            .collect()
    }

    pub async fn spawn(
        &self,
        description: String,
        prompt: String,
        agent_type: String,
        background: bool,
        model_override: Option<String>,
    ) -> Result<String> {
        self.spawn_cancellable(
            description,
            prompt,
            agent_type,
            background,
            model_override,
            None,
        )
        .await
    }

    pub async fn spawn_cancellable(
        &self,
        description: String,
        prompt: String,
        agent_type: String,
        background: bool,
        model_override: Option<String>,
        parent_cancellation: Option<CancellationToken>,
    ) -> Result<String> {
        if !self.inner.settings.enabled {
            bail!("subagents are disabled in config");
        }
        let role = resolve_role(&self.inner.settings, &agent_type)?;
        let model = model_override
            .or_else(|| role.model.clone())
            .unwrap_or_else(|| self.inner.config.model.model.clone());
        let id = {
            let mut state = self.inner.state.lock().await;
            if state.stopped {
                bail!("subagent manager is stopped");
            }
            prune_records(&mut state.records, self.inner.settings.max_agents);
            if state.records.len() >= self.inner.settings.max_agents {
                bail!(
                    "subagent limit of {} reached; stop or start a new session",
                    self.inner.settings.max_agents
                );
            }
            state.next_id = state.next_id.saturating_add(1);
            let id = state.next_id;
            state.records.insert(
                id,
                AgentRecord {
                    id,
                    description: description.clone(),
                    agent_type: role.name.clone(),
                    model: model.clone(),
                    status: SubagentStatus::Queued,
                    turns: 0,
                    started: Instant::now(),
                    duration_ms: 0,
                    result: None,
                    error: None,
                    background,
                    generation: 1,
                    cancellation: CancellationToken::new(),
                    input_queue: InputQueue::new(),
                    runtime: None,
                    handle: None,
                },
            );
            id
        };
        self.emit(id, SubagentStatus::Queued, "waiting for an execution slot")
            .await;
        self.launch(id, role, prompt, model, None).await?;
        if background {
            return Ok(format!(
                "Subagent #{id} started in the background ({agent_type} · {description}). \
Completion will be delivered automatically; continue non-overlapping work."
            ));
        }
        let foreground_ids = [id];
        let wait = self.wait_for(
            &foreground_ids,
            self.inner.settings.max_runtime_seconds.saturating_add(1),
        );
        let completed = if let Some(parent_cancellation) = parent_cancellation {
            tokio::select! {
                biased;
                _ = parent_cancellation.cancelled() => {
                    self.stop(id).await?;
                    bail!("subagent #{id} cancelled with its parent task");
                }
                completed = wait => completed?,
            }
        } else {
            wait.await?
        };
        if !completed {
            self.stop(id).await?;
            bail!(
                "subagent #{id} did not stop within {} seconds",
                self.inner.settings.max_runtime_seconds.saturating_add(1)
            );
        }
        self.status(Some(id)).await
    }

    pub async fn send(&self, id: u64, message: String) -> Result<String> {
        let (role, model, runtime) = {
            let mut state = self.inner.state.lock().await;
            let record = state
                .records
                .get_mut(&id)
                .with_context(|| format!("unknown subagent #{id}"))?;
            match record.status {
                SubagentStatus::Queued | SubagentStatus::Running => {
                    record.input_queue.steer(message);
                    return Ok(format!(
                        "Queued additional context for subagent #{id}; it will receive it at the next safe model boundary."
                    ));
                }
                SubagentStatus::Completed | SubagentStatus::Cancelled | SubagentStatus::Failed => {}
            }
            let runtime = record.runtime.take().with_context(|| {
                format!("subagent #{id} cannot be resumed after its terminal error")
            })?;
            record.status = SubagentStatus::Queued;
            record.result = None;
            record.error = None;
            record.background = true;
            record.generation = record.generation.saturating_add(1);
            record.cancellation = CancellationToken::new();
            record.input_queue = InputQueue::new();
            record.started = Instant::now();
            (
                resolve_role(&self.inner.settings, &record.agent_type)?,
                record.model.clone(),
                runtime,
            )
        };
        self.emit(id, SubagentStatus::Queued, "queued follow-up task")
            .await;
        self.launch(id, role, message, model, Some(runtime)).await?;
        Ok(format!(
            "Subagent #{id} resumed with its preserved conversation. Completion will be delivered automatically."
        ))
    }

    pub async fn wait(&self, ids: &[u64], timeout_seconds: Option<u64>) -> Result<String> {
        let timeout = timeout_seconds
            .unwrap_or(self.inner.settings.wait_timeout_seconds)
            .min(60);
        let completed = self.wait_for(ids, timeout).await?;
        let mut outputs = Vec::new();
        for id in ids {
            outputs.push(self.status(Some(*id)).await?);
        }
        let output = outputs.join("\n\n");
        if completed {
            Ok(output)
        } else {
            Ok(format!(
                "Wait timed out after {timeout} seconds; unfinished subagents continue in the background.\n\n{output}"
            ))
        }
    }

    pub async fn stop(&self, id: u64) -> Result<String> {
        let cancellation = {
            let state = self.inner.state.lock().await;
            let record = state
                .records
                .get(&id)
                .with_context(|| format!("unknown subagent #{id}"))?;
            if record.status.is_terminal() {
                return Ok(format!(
                    "Subagent #{id} is already {}.",
                    record.status.as_str()
                ));
            }
            record.cancellation.clone()
        };
        cancellation.cancel();
        self.inner.changed.notify_waiters();
        Ok(format!("Cancellation requested for subagent #{id}."))
    }

    pub async fn status(&self, id: Option<u64>) -> Result<String> {
        let summaries = self.summaries().await;
        if let Some(id) = id {
            let summary = summaries
                .into_iter()
                .find(|summary| summary.id == id)
                .with_context(|| format!("unknown subagent #{id}"))?;
            return Ok(format_summary(
                &summary,
                self.inner.settings.max_output_bytes,
            ));
        }
        if summaries.is_empty() {
            return Ok("No subagents have been started in this session.".into());
        }
        Ok(summaries
            .iter()
            .map(|summary| {
                format!(
                    "#{:<3} {:10} {:16} {} · {} turn{} · {} ms",
                    summary.id,
                    summary.status.as_str(),
                    summary.agent_type,
                    one_line(&summary.description, 80),
                    summary.turns,
                    if summary.turns == 1 { "" } else { "s" },
                    summary.duration_ms
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub async fn summaries(&self) -> Vec<SubagentSummary> {
        self.inner
            .state
            .lock()
            .await
            .records
            .values()
            .map(record_summary)
            .collect()
    }

    pub fn take_notifications(&self) -> Vec<String> {
        self.inner
            .hub
            .queue
            .lock()
            .expect("subagent notification lock poisoned")
            .drain(..)
            .collect()
    }

    pub async fn shutdown(&self) {
        let handles = {
            let mut state = self.inner.state.lock().await;
            state.stopped = true;
            for record in state.records.values() {
                if !record.status.is_terminal() {
                    record.cancellation.cancel();
                }
            }
            state
                .records
                .values_mut()
                .filter_map(|record| record.handle.take())
                .collect::<Vec<_>>()
        };
        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
    }

    async fn launch(
        &self,
        id: u64,
        role: RoleDefinition,
        prompt: String,
        model: String,
        runtime: Option<SubagentRuntime>,
    ) -> Result<()> {
        let generation = self
            .inner
            .state
            .lock()
            .await
            .records
            .get(&id)
            .context("subagent disappeared before launch")?
            .generation;
        let handle = tokio::spawn(
            self.clone()
                .run_task(id, generation, role, prompt, model, runtime),
        );
        let mut state = self.inner.state.lock().await;
        let record = state
            .records
            .get_mut(&id)
            .context("subagent disappeared while storing task handle")?;
        record.handle = Some(handle);
        Ok(())
    }

    fn run_task(
        self,
        id: u64,
        generation: u64,
        role: RoleDefinition,
        prompt: String,
        model: String,
        runtime: Option<SubagentRuntime>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            let cancellation = {
                let state = self.inner.state.lock().await;
                let Some(record) = state.records.get(&id) else {
                    return;
                };
                record.cancellation.clone()
            };
            let permit = tokio::select! {
                _ = cancellation.cancelled() => {
                    self.finish_cancelled(id, generation, None).await;
                    return;
                }
                permit = self.inner.semaphore.clone().acquire_owned() => {
                    match permit {
                        Ok(permit) => permit,
                        Err(_) => {
                            self.finish_error(id, generation, None, "subagent executor closed".into()).await;
                            return;
                        }
                    }
                }
            };
            {
                let mut state = self.inner.state.lock().await;
                let Some(record) = state.records.get_mut(&id) else {
                    return;
                };
                if record.generation != generation {
                    return;
                }
                record.status = SubagentStatus::Running;
            }
            self.emit(id, SubagentStatus::Running, "working").await;

            let runtime = match runtime {
                Some(runtime) => Ok(runtime),
                None => self.build_runtime(id, &role, &model),
            };
            let mut runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.finish_error(id, generation, None, format!("{error:#}"))
                        .await;
                    return;
                }
            };
            let input_queue = {
                let state = self.inner.state.lock().await;
                let Some(record) = state.records.get(&id) else {
                    return;
                };
                record.input_queue.clone()
            };
            let timeout = Duration::from_secs(self.inner.settings.max_runtime_seconds);
            let task = runtime.agent.run_in_conversation(
                &prompt,
                RunOptions {
                    task_id: Some(format!("subagent-{id}-{generation}")),
                    cancellation: Some(cancellation.clone()),
                    input_queue: Some(input_queue),
                    additional_system_prompt: Some(runtime.role_prompt.clone()),
                    ..Default::default()
                },
                &mut runtime.conversation,
            );
            let outcome = match tokio::time::timeout(timeout, task).await {
                Ok(Ok(result)) => TaskOutcome::Finished { runtime, result },
                Ok(Err(error)) => TaskOutcome::Error {
                    runtime: Some(runtime),
                    message: format!("{error:#}"),
                },
                Err(_) => TaskOutcome::TimedOut,
            };
            drop(permit);
            self.finish(id, generation, outcome).await;
        })
    }

    fn build_runtime(
        &self,
        id: u64,
        role: &RoleDefinition,
        model: &str,
    ) -> Result<SubagentRuntime> {
        let mut config = self.inner.config.clone();
        config.model.model = model.to_owned();
        config.agent.max_steps = role
            .max_steps
            .unwrap_or(self.inner.settings.max_steps)
            .min(self.inner.settings.max_steps);
        config.agent.wall_time_limit_seconds = config
            .agent
            .wall_time_limit_seconds
            .min(self.inner.settings.max_runtime_seconds);
        let profile = if role.read_only {
            ToolProfile::ReadOnlySubagent
        } else {
            ToolProfile::Coding
        };
        let model = (self.inner.factory)(&config.model, profile)?;
        let sink = Box::new(SubagentSink {
            id,
            description: role.description.clone(),
            agent_type: role.name.clone(),
            hub: self.inner.hub.clone(),
        });
        Ok(SubagentRuntime {
            agent: Agent::new_with_profile(
                config,
                model,
                sink,
                self.inner.workspace.clone(),
                profile,
            ),
            conversation: Conversation::default(),
            role_prompt: role_system_prompt(role),
        })
    }

    async fn finish(&self, id: u64, generation: u64, outcome: TaskOutcome) {
        match outcome {
            TaskOutcome::Finished { runtime, result } => {
                let status = if result.success {
                    SubagentStatus::Completed
                } else if result.reason == "cancelled" {
                    SubagentStatus::Cancelled
                } else {
                    SubagentStatus::Failed
                };
                let error = (status == SubagentStatus::Failed)
                    .then(|| format!("subagent ended with reason {}", result.reason));
                self.finish_record(
                    id,
                    generation,
                    status,
                    Some(runtime),
                    result.steps,
                    Some(result.summary),
                    error,
                )
                .await;
            }
            TaskOutcome::Error { runtime, message } => {
                self.finish_error(id, generation, runtime, message).await;
            }
            TaskOutcome::TimedOut => {
                self.finish_error(
                    id,
                    generation,
                    None,
                    format!(
                        "subagent exceeded {} seconds",
                        self.inner.settings.max_runtime_seconds
                    ),
                )
                .await;
            }
        }
    }

    async fn finish_cancelled(&self, id: u64, generation: u64, runtime: Option<SubagentRuntime>) {
        self.finish_record(
            id,
            generation,
            SubagentStatus::Cancelled,
            runtime,
            0,
            None,
            Some("cancelled".into()),
        )
        .await;
    }

    async fn finish_error(
        &self,
        id: u64,
        generation: u64,
        runtime: Option<SubagentRuntime>,
        message: String,
    ) {
        self.finish_record(
            id,
            generation,
            SubagentStatus::Failed,
            runtime,
            0,
            None,
            Some(message),
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_record(
        &self,
        id: u64,
        generation: u64,
        status: SubagentStatus,
        runtime: Option<SubagentRuntime>,
        turns: usize,
        result: Option<String>,
        error: Option<String>,
    ) {
        let (background, description, agent_type, detail) = {
            let mut state = self.inner.state.lock().await;
            let Some(record) = state.records.get_mut(&id) else {
                return;
            };
            if record.generation != generation {
                return;
            }
            record.status = status;
            record.turns = turns;
            record.duration_ms = record.started.elapsed().as_millis();
            record.result =
                result.map(|result| bound_output(&result, self.inner.settings.max_output_bytes));
            record.error =
                error.map(|error| bound_output(&error, self.inner.settings.max_output_bytes));
            record.runtime = runtime;
            record.handle = None;
            (
                record.background,
                record.description.clone(),
                record.agent_type.clone(),
                record
                    .result
                    .clone()
                    .or_else(|| record.error.clone())
                    .unwrap_or_else(|| status.as_str().into()),
            )
        };
        if background {
            self.inner.hub.notification(format!(
                "SUBAGENT #{id} {} · {} · {}\n{}",
                status.as_str(),
                agent_type,
                description,
                detail
            ));
        }
        self.emit(id, status, &detail).await;
        self.inner.changed.notify_waiters();
    }

    async fn wait_for(&self, ids: &[u64], timeout_seconds: u64) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
        loop {
            let notified = self.inner.changed.notified();
            {
                let state = self.inner.state.lock().await;
                for id in ids {
                    if !state.records.contains_key(id) {
                        bail!("unknown subagent #{id}");
                    }
                }
                if ids.iter().all(|id| {
                    state
                        .records
                        .get(id)
                        .is_some_and(|record| record.status.is_terminal())
                }) {
                    return Ok(true);
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(false);
            }
        }
    }

    async fn emit(&self, id: u64, status: SubagentStatus, detail: &str) {
        let summary = {
            let state = self.inner.state.lock().await;
            state.records.get(&id).map(|record| {
                (
                    record.description.clone(),
                    record.agent_type.clone(),
                    one_line(detail, 240),
                )
            })
        };
        if let Some((description, agent_type, detail)) = summary {
            self.inner.hub.event(SubagentEvent {
                id,
                description,
                agent_type,
                status,
                detail,
            });
        }
    }
}

impl EventHub {
    fn event(&self, event: SubagentEvent) {
        let handler = self
            .handler
            .lock()
            .expect("subagent event handler lock poisoned")
            .clone();
        if let Some(handler) = handler {
            handler(event);
        }
    }

    fn notification(&self, text: String) {
        let text = bound_output(&text, self.max_output_bytes);
        let mut queue = self
            .queue
            .lock()
            .expect("subagent notification lock poisoned");
        let mut total = queue.iter().map(String::len).sum::<usize>();
        while queue.len() >= MAX_NOTIFICATIONS
            || total.saturating_add(text.len()) > self.max_output_bytes
        {
            let Some(evicted) = queue.pop_front() else {
                break;
            };
            total = total.saturating_sub(evicted.len());
        }
        queue.push_back(text);
    }
}

impl EventSink for SubagentSink {
    fn emit(&self, event: &Event) -> Result<()> {
        let detail = match event {
            Event::Action {
                kind, description, ..
            } => Some(format!("{kind} · {}", one_line(description, 160))),
            Event::ContextCompacted { removed_messages } => {
                Some(format!("compacted {removed_messages} context messages"))
            }
            _ => None,
        };
        if let Some(detail) = detail {
            self.hub.event(SubagentEvent {
                id: self.id,
                description: self.description.clone(),
                agent_type: self.agent_type.clone(),
                status: SubagentStatus::Running,
                detail,
            });
        }
        Ok(())
    }
}

fn effective_roles(settings: &SubagentsConfig) -> BTreeMap<String, RoleDefinition> {
    let mut roles = builtin_roles()
        .into_iter()
        .map(|role| (role.name.clone(), role))
        .collect::<BTreeMap<_, _>>();
    for (name, role) in &settings.roles {
        roles.insert(name.clone(), configured_role(name, role));
    }
    roles
}

fn resolve_role(settings: &SubagentsConfig, name: &str) -> Result<RoleDefinition> {
    let roles = effective_roles(settings);
    roles.get(name).cloned().with_context(|| {
        format!(
            "unknown subagent type {name:?}; available types: {}",
            roles.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })
}

impl Clone for RoleDefinition {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
            prompt: self.prompt.clone(),
            read_only: self.read_only,
            model: self.model.clone(),
            max_steps: self.max_steps,
        }
    }
}

fn configured_role(name: &str, role: &SubagentRoleConfig) -> RoleDefinition {
    RoleDefinition {
        name: name.into(),
        description: role.description.clone(),
        prompt: role.prompt.clone(),
        read_only: role.read_only,
        model: role.model.clone(),
        max_steps: role.max_steps,
    }
}

fn builtin_roles() -> Vec<RoleDefinition> {
    vec![
        RoleDefinition {
            name: "general-purpose".into(),
            description: "Autonomous implementation, debugging, and verification".into(),
            prompt: "Work autonomously on the delegated task. You may inspect, edit, and verify the shared workspace. Keep changes tightly scoped, preserve unrelated work, and report concrete files and validation. Do not spawn other agents.".into(),
            read_only: false,
            model: None,
            max_steps: None,
        },
        RoleDefinition {
            name: "explore".into(),
            description: "Fast read-only repository reconnaissance".into(),
            prompt: "Investigate the delegated question using only repository read tools. Trace symbols and relevant files, distinguish evidence from inference, and return a compact answer with exact paths. Do not edit files or run shell commands.".into(),
            read_only: true,
            model: None,
            max_steps: Some(12),
        },
        RoleDefinition {
            name: "plan".into(),
            description: "Read-only implementation planning and risk analysis".into(),
            prompt: "Inspect the repository deeply enough to produce an executable implementation plan. Identify exact files, dependencies, invariants, tests, risks, and sequencing. Do not edit files or run shell commands.".into(),
            read_only: true,
            model: None,
            max_steps: Some(16),
        },
        RoleDefinition {
            name: "review".into(),
            description: "Read-only correctness and regression review".into(),
            prompt: "Review the requested code or change for concrete correctness, security, portability, and test gaps. Prioritize findings by impact and cite exact files and lines. Return no finding when evidence does not support one. Do not edit files or run shell commands.".into(),
            read_only: true,
            model: None,
            max_steps: Some(16),
        },
    ]
}

fn role_system_prompt(role: &RoleDefinition) -> String {
    format!(
        "You are WeCode subagent type `{}`. Your context is isolated from the parent agent. \
Complete only the delegated task and return the useful result through finish. \
The workspace is shared with the parent, so preserve unrelated changes and do not duplicate work. \
You cannot ask the user questions or spawn another agent.\n\nRole instructions:\n{}",
        role.name, role.prompt
    )
}

fn prune_records(records: &mut BTreeMap<u64, AgentRecord>, limit: usize) {
    while records.len() >= limit {
        let Some(id) = records
            .iter()
            .find_map(|(id, record)| record.status.is_terminal().then_some(*id))
        else {
            break;
        };
        records.remove(&id);
    }
}

fn record_summary(record: &AgentRecord) -> SubagentSummary {
    SubagentSummary {
        id: record.id,
        description: record.description.clone(),
        agent_type: record.agent_type.clone(),
        model: record.model.clone(),
        status: record.status,
        turns: record.turns,
        duration_ms: if record.status.is_terminal() {
            record.duration_ms
        } else {
            record.started.elapsed().as_millis()
        },
        result: record.result.clone(),
        error: record.error.clone(),
    }
}

fn format_summary(summary: &SubagentSummary, limit: usize) -> String {
    let mut output = format!(
        "Subagent #{} · {} · {}\nrole: {}\nmodel: {}\nturns: {}\nduration_ms: {}",
        summary.id,
        summary.status.as_str(),
        summary.description,
        summary.agent_type,
        summary.model,
        summary.turns,
        summary.duration_ms
    );
    if let Some(result) = &summary.result {
        output.push_str("\n\nresult:\n");
        output.push_str(result);
    }
    if let Some(error) = &summary.error {
        output.push_str("\n\nerror:\n");
        output.push_str(error);
    }
    bound_output(&output, limit)
}

fn bound_output(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let notice = format!(
        "\n\n[Subagent output truncated: {} bytes omitted]",
        value.len().saturating_sub(limit)
    );
    let budget = limit.saturating_sub(notice.len());
    let head = previous_boundary(value, budget / 2);
    let tail = next_boundary(value, value.len().saturating_sub(budget - budget / 2));
    format!("{}{}{}", &value[..head], notice, &value[tail..])
}

fn previous_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_boundary(value: &str, mut index: usize) -> usize {
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
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;
    use crate::model::{CompletionRequest, ModelResponse, ModelStream, Usage};
    use crate::protocol::Action;

    struct FakeModel {
        actions: StdMutex<VecDeque<Action>>,
    }

    struct BlockingModel {
        started: Arc<Notify>,
    }

    #[async_trait]
    impl Model for FakeModel {
        async fn complete(
            &self,
            _request: CompletionRequest,
            _stream: Option<&dyn ModelStream>,
        ) -> Result<ModelResponse> {
            Ok(ModelResponse {
                text: String::new(),
                action: self.actions.lock().unwrap().pop_front(),
                additional_actions: Vec::new(),
                usage: Usage::default(),
                cache_hit: false,
            })
        }
    }

    #[async_trait]
    impl Model for BlockingModel {
        async fn complete(
            &self,
            _request: CompletionRequest,
            _stream: Option<&dyn ModelStream>,
        ) -> Result<ModelResponse> {
            self.started.notify_one();
            std::future::pending().await
        }
    }

    fn fixture_manager() -> (tempfile::TempDir, SubagentManager) {
        let temp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .status()
            .unwrap();
        let mut config = Config::default();
        config.agent.trajectory_directory = temp.path().join("trajectories");
        config.cache.directory = temp.path().join("cache");
        let manager = SubagentManager::new_with_model_factory(
            config,
            temp.path().to_path_buf(),
            |_model, _profile| {
                Ok(Box::new(FakeModel {
                    actions: StdMutex::new(VecDeque::from([
                        Action::Finish {
                            summary: "delegated result".into(),
                        },
                        Action::Finish {
                            summary: "delegated result".into(),
                        },
                    ])),
                }))
            },
        )
        .unwrap();
        (temp, manager)
    }

    #[test]
    fn manager_creation_is_lazy() {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = SubagentManager::new_with_model_factory(
            Config::default(),
            temp.path().to_path_buf(),
            {
                let calls = calls.clone();
                move |_model, _profile| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(FakeModel {
                        actions: StdMutex::new(VecDeque::new()),
                    }))
                }
            },
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(manager);
    }

    #[tokio::test]
    async fn foreground_and_resumed_agents_preserve_lifecycle() {
        let (_temp, manager) = fixture_manager();
        let output = manager
            .spawn(
                "inspect fixture".into(),
                "Report what you find.".into(),
                "explore".into(),
                false,
                None,
            )
            .await
            .unwrap();
        assert!(output.contains("delegated result"));
        assert_eq!(
            manager.summaries().await[0].status,
            SubagentStatus::Completed
        );

        manager.send(1, "Check once more.".into()).await.unwrap();
        manager.wait(&[1], Some(5)).await.unwrap();
        let summary = manager.summaries().await.remove(0);
        assert_eq!(summary.status, SubagentStatus::Completed);
        assert_eq!(summary.turns, 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn background_completion_is_bounded_and_notified() {
        let (_temp, manager) = fixture_manager();
        manager
            .spawn(
                "background fixture".into(),
                "Report what you find.".into(),
                "explore".into(),
                true,
                None,
            )
            .await
            .unwrap();
        manager.wait(&[1], Some(5)).await.unwrap();
        let notifications = manager.take_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("SUBAGENT #1 completed"));
        assert!(
            notifications.iter().map(String::len).sum::<usize>()
                <= manager.inner.settings.max_output_bytes
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_parent_cancels_foreground_subagent() {
        let temp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .status()
            .unwrap();
        let started = Arc::new(Notify::new());
        let mut config = Config::default();
        config.agent.trajectory_directory = temp.path().join("trajectories");
        config.cache.directory = temp.path().join("cache");
        let manager = SubagentManager::new_with_model_factory(config, temp.path().to_path_buf(), {
            let started = started.clone();
            move |_model, _profile| {
                Ok(Box::new(BlockingModel {
                    started: started.clone(),
                }))
            }
        })
        .unwrap();
        let parent_cancellation = CancellationToken::new();
        let spawn = tokio::spawn({
            let manager = manager.clone();
            let parent_cancellation = parent_cancellation.clone();
            async move {
                manager
                    .spawn_cancellable(
                        "blocking fixture".into(),
                        "Wait forever.".into(),
                        "explore".into(),
                        false,
                        None,
                        Some(parent_cancellation),
                    )
                    .await
            }
        });

        started.notified().await;
        parent_cancellation.cancel();
        let error = spawn.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled with its parent"));
        manager.wait(&[1], Some(5)).await.unwrap();
        assert_eq!(
            manager.summaries().await[0].status,
            SubagentStatus::Cancelled
        );
        manager.shutdown().await;
    }

    #[test]
    fn custom_roles_override_builtins_and_outputs_are_utf8_safe() {
        let mut settings = SubagentsConfig::default();
        settings.roles.insert(
            "explore".into(),
            SubagentRoleConfig {
                description: "custom".into(),
                prompt: "custom prompt".into(),
                read_only: false,
                ..Default::default()
            },
        );
        let role = resolve_role(&settings, "explore").unwrap();
        assert!(!role.read_only);
        assert_eq!(role.prompt, "custom prompt");
        let bounded = bound_output(&"界".repeat(10_000), 4_096);
        assert!(bounded.len() <= 4_096);
        assert!(bounded.contains("truncated"));
    }
}
