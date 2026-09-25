use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionTarget {
    Direct {
        websocket_url: String,
        editor_ssh: Option<String>,
    },
    Ssh {
        host: String,
        username: Option<String>,
        ssh_port: u16,
        daemon_port: u16,
    },
}

pub struct RuntimePassword(String);

impl RuntimePassword {
    pub fn new(password: String) -> Self {
        Self(password)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub id: String,
    pub label: Option<String>,
    pub status: String,
    pub extra: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub title: Option<String>,
    pub status: String,
    pub directory: Option<PathBuf>,
    pub extra: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TimelinePayload {
    Message(Value),
    Tool(Value),
    Lifecycle(Value),
    Other(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineEntry {
    pub agent_id: String,
    pub epoch: String,
    pub sequence: u64,
    pub timestamp: String,
    pub payload: TimelinePayload,
    pub extra: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineCursor {
    pub epoch: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelinePage {
    pub epoch: String,
    pub entries: Vec<TimelineEntry>,
    pub start_cursor: Option<TimelineCursor>,
    pub end_cursor: Option<TimelineCursor>,
    pub has_older: bool,
    pub has_newer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub agent_id: String,
    pub request_id: String,
    pub title: String,
    pub description: Option<String>,
    pub extra: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaseoEvent {
    Connected,
    Disconnected { reason: String },
    AgentsChanged(Vec<AgentSummary>),
    TimelineEntry(TimelineEntry),
    TimelineReplaced { agent_id: String, epoch: String },
    PermissionRequested(PermissionRequest),
    PermissionResolved { request_id: String },
}

#[derive(Clone, Debug)]
pub struct CreateAgent {
    pub provider: String,
    pub model: Option<String>,
    pub directory: PathBuf,
    pub title: Option<String>,
    pub initial_prompt: Option<String>,
    pub idempotency_key: String,
}
