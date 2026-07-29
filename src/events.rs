use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::model::Usage;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        session_id: String,
        task_id: Option<String>,
        workspace: String,
        provider: String,
        model: String,
    },
    ModelStarted {
        step: usize,
    },
    ModelDelta {
        step: usize,
        text: String,
        reasoning: bool,
    },
    ModelCompleted {
        step: usize,
        cache_hit: bool,
        usage: Usage,
    },
    AssistantMessage {
        step: usize,
        text: String,
    },
    Action {
        step: usize,
        kind: String,
        description: String,
        detail: String,
    },
    ApprovalRequested {
        id: u64,
        step: usize,
        kind: String,
        risk: String,
        summary: String,
        detail: String,
    },
    ApprovalResolved {
        id: u64,
        step: usize,
        decision: String,
    },
    ToolCompleted {
        step: usize,
        exit_code: Option<i32>,
        duration_ms: u128,
        truncated_bytes: usize,
    },
    ToolOutput {
        step: usize,
        output: String,
    },
    ContextCompacted {
        removed_messages: usize,
    },
    SteeringDelivered {
        step: usize,
        count: usize,
    },
    RunCancelled {
        step: usize,
    },
    Verification {
        passed: bool,
        exit_code: Option<i32>,
    },
    RunCompleted {
        success: bool,
        reason: String,
        steps: usize,
        duration_ms: u128,
        patch_bytes: usize,
        cache_hits: usize,
        usage: Usage,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize)]
struct TimestampedEvent<'a> {
    timestamp_ms: u128,
    #[serde(flatten)]
    event: &'a Event,
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event) -> Result<()>;

    fn wants_model_deltas(&self) -> bool {
        false
    }
}

pub struct JsonlSink {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl JsonlSink {
    pub fn stdout() -> Self {
        Self {
            writer: Mutex::new(Box::new(io::stdout())),
        }
    }

    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }
}

impl EventSink for JsonlSink {
    fn emit(&self, event: &Event) -> Result<()> {
        let record = TimestampedEvent {
            timestamp_ms: timestamp_ms(),
            event,
        };
        let mut writer = self.writer.lock().expect("JSONL writer lock poisoned");
        serde_json::to_writer(&mut *writer, &record)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

pub fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
