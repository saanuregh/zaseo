use serde_json::Value;
use std::collections::BTreeMap;
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

/// Why the daemon refused this client's credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRejection {
    PasswordRequired,
    IncorrectPassword,
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::PasswordRequired => "Password required",
            Self::IncorrectPassword => "Incorrect password",
        })
    }
}

impl std::error::Error for AuthRejection {}

/// What the connected daemon supports, from its `server_info` hello.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    /// The daemon's own ID, which tells two profiles that reach the same daemon apart from two
    /// daemons.
    pub server_id: Option<String>,
    pub features: Value,
    pub capabilities: Value,
    /// Paseo Desktop runs this daemon, so only Paseo Desktop can update it.
    pub desktop_managed: bool,
}

impl ServerInfo {
    pub fn has_feature(&self, feature: &str) -> bool {
        self.features[feature] == true
    }

    pub fn dictation_enabled(&self) -> bool {
        self.capabilities["voice"]["dictation"]["enabled"] == true
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
    /// The daemon's project placement for the agent, when it sent one.
    pub project: Option<Value>,
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

/// A Paseo workspace: a directory or Paseo worktree inside a project, holding agents.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceDescriptor {
    pub id: String,
    pub project_id: String,
    pub project_display_name: String,
    pub project_root_path: PathBuf,
    /// Where the workspace's agents run: the project root, or a checkout or worktree.
    pub directory: PathBuf,
    /// `directory`, `local_checkout`, `checkout`, or `worktree`.
    pub kind: String,
    pub worktree_slug: Option<String>,
    /// The resolved display name.
    pub name: String,
    /// The user's title override, if any.
    pub title: Option<String>,
    pub pinned_at: Option<String>,
    pub labels: Vec<String>,
    /// `needs_input`, `failed`, `running`, `attention`, or `done`.
    pub status: String,
    pub activity_at: Option<String>,
    pub diff_stat: Option<DiffStat>,
    pub scripts: Vec<WorkspaceScript>,
    pub current_branch: Option<String>,
    pub is_paseo_worktree: bool,
    pub extra: Value,
}

impl WorkspaceDescriptor {
    /// Whether the workspace lives in a git worktree rather than the project's own checkout.
    pub fn is_worktree(&self) -> bool {
        self.kind == "worktree" || self.is_paseo_worktree
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffStat {
    pub additions: u64,
    pub deletions: u64,
}

/// A Paseo project: a repository or directory that workspaces belong to.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectDescriptor {
    pub id: String,
    pub display_name: String,
    pub custom_name: Option<String>,
    /// Changes whenever the project's icon changes, so a cached icon can be refetched.
    pub icon_revision: Option<String>,
    pub root_path: PathBuf,
    /// `git`, `non_git`, or `directory`.
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceLabel {
    pub name: String,
    /// One of Paseo's label colors, such as `violet` or `emerald`.
    pub color: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceScript {
    pub name: String,
    /// `script` or `service`.
    pub kind: String,
    pub hostname: String,
    pub port: Option<u16>,
    pub proxy_url: Option<String>,
    pub running: bool,
    /// `healthy` or `unhealthy`, when the script reports health.
    pub health: Option<String>,
    pub exit_code: Option<i64>,
    pub terminal_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SetupSnapshot {
    /// `running`, `completed`, `failed`, or `blocked`.
    pub status: String,
    pub log: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryState {
    Recoverable {
        workspace_name: String,
        action: String,
        branch: Option<String>,
    },
    Unavailable {
        reason: String,
        message: String,
    },
}

/// Where a new workspace comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceSource {
    Directory {
        path: String,
        project_id: Option<String>,
    },
    /// A new Paseo worktree branched off `base_ref`.
    Worktree {
        cwd: String,
        project_id: Option<String>,
        base_ref: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderAvailability {
    pub provider: String,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelayStatus {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub public_endpoint: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonStatus {
    pub server_id: String,
    pub version: Option<String>,
    pub pid: Option<u64>,
    pub node_path: Option<String>,
    pub started_at: Option<String>,
    pub listen: Option<String>,
    pub relay: Option<RelayStatus>,
    pub providers: Vec<ProviderAvailability>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DaemonUpdate {
    pub previous_version: Option<String>,
    pub new_version: Option<String>,
}

/// A git worktree Paseo created for an agent.
#[derive(Clone, Debug, PartialEq)]
pub struct PaseoWorktree {
    pub path: PathBuf,
    pub created_at: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// A subagent a provider ran for an agent, such as a Claude Task.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSubagent {
    pub id: String,
    pub parent_agent_id: String,
    pub parent_subagent_id: Option<String>,
    pub provider: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `running`, `completed`, `failed`, or `canceled`.
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// The parent's tool call that started the subagent.
    pub tool_call_id: Option<String>,
    pub cwd: Option<String>,
    pub subtitle: Option<String>,
}

/// The timeline ID a subagent's entries are stored under. Subagents have no agent ID of their own,
/// and this keeps their entries apart from every agent's.
pub fn subagent_timeline_id(parent_agent_id: &str, subagent_id: &str) -> String {
    format!("subagent:{parent_agent_id}:{subagent_id}")
}

/// The parent agent and subagent IDs of a `subagent_timeline_id`, or `None` for an agent's own
/// timeline ID.
pub fn parse_subagent_timeline_id(timeline_id: &str) -> Option<(&str, &str)> {
    timeline_id.strip_prefix("subagent:")?.split_once(':')
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
    /// The daemon's advertised features, sent before every `Connected`.
    ServerInfo(ServerInfo),
    Disconnected {
        reason: String,
    },
    /// The session stopped reconnecting, so no further events follow.
    ConnectionFailed {
        reason: String,
    },
    /// The whole agent directory, sent once its last page arrives. It replaces every agent the
    /// receiver knew.
    AgentsChanged(Vec<AgentSummary>),
    /// One agent was added or changed.
    AgentUpserted(AgentSummary),
    /// One agent left the directory.
    AgentRemoved {
        agent_id: String,
    },
    /// An agent or subagent timeline item; subagent entries use `subagent_timeline_id`.
    TimelineEntry(TimelineEntry),
    SubagentUpserted(ProviderSubagent),
    SubagentRemoved {
        parent_agent_id: String,
        subagent_id: String,
    },
    TimelineReplaced {
        agent_id: String,
        epoch: String,
    },
    PermissionRequested(PermissionRequest),
    PermissionResolved {
        request_id: String,
    },
    /// The daemon's global provider snapshot changed.
    ProvidersChanged(Vec<Provider>),
    /// Bytes for a subscribed terminal. `restore` output replaces the whole screen.
    TerminalOutput {
        terminal_id: String,
        bytes: Vec<u8>,
        restore: bool,
    },
    TerminalExited {
        terminal_id: String,
        error: Option<String>,
    },
    /// The terminals open in a directory watched with `watch_terminals`.
    TerminalsChanged {
        cwd: String,
        /// Set on the first snapshot; releasing it stops the updates.
        subscription_id: Option<String>,
        terminals: Vec<TerminalInfo>,
    },
    DictationPartial {
        dictation_id: String,
        text: String,
    },
    DictationFinal {
        dictation_id: String,
        text: String,
    },
    DictationFailed {
        dictation_id: String,
        error: String,
    },
    /// The first page of workspaces after connecting, with projects that have none.
    WorkspacesSnapshot {
        workspaces: Vec<WorkspaceDescriptor>,
        empty_projects: Vec<ProjectDescriptor>,
        /// Set when more pages exist; fetch them with `workspaces_page`.
        next_cursor: Option<String>,
    },
    WorkspaceUpserted(WorkspaceDescriptor),
    WorkspaceRemoved {
        workspace_id: String,
        /// Set when removing the workspace removed its project too.
        removed_project_id: Option<String>,
    },
    ProjectUpserted(ProjectDescriptor),
    ProjectRemoved {
        project_id: String,
    },
    LabelsSnapshot(Vec<WorkspaceLabel>),
    LabelUpserted {
        label: WorkspaceLabel,
        previous_name: Option<String>,
    },
    LabelRemoved {
        name: String,
    },
    ScriptsChanged {
        workspace_id: String,
        scripts: Vec<WorkspaceScript>,
    },
    SetupProgress {
        workspace_id: String,
        snapshot: SetupSnapshot,
    },
    /// A `daemon.update` phase: `starting`, `downloading`, `installing`, or `complete`.
    DaemonUpdateProgress {
        phase: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: String,
    pub name: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
}

/// Which part of an agent a rewind restores to the chosen user message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewindMode {
    Conversation,
    Files,
    Both,
}

impl RewindMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Files => "files",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub display_name: String,
    /// `available`, `unavailable`, or `error`.
    pub status: String,
    pub plan_label: Option<String>,
    pub source_label: Option<String>,
    pub fetched_at: Option<String>,
    pub error: Option<String>,
    pub windows: Vec<UsageWindow>,
    pub balances: Vec<UsageBalance>,
    pub details: Vec<UsageDetail>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub id: String,
    pub label: String,
    /// A few characters naming the window where space is tight, such as `5h`. An empty string
    /// means the percent alone; `None` means use `label`.
    pub short_label: Option<String>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<String>,
    pub runs_out_at: Option<String>,
    pub tone: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageBalance {
    pub label: String,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub limit: Option<f64>,
    pub unit: String,
    pub tone: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageDetail {
    pub label: String,
    pub value: String,
}

/// Why an account's usage can't be read.
#[derive(Clone, Debug, PartialEq)]
pub enum UsageProblem {
    /// The login expired; `refreshed_by` is the command that refreshes it, such as `claude login`.
    Expired {
        expires_at: String,
        refreshed_by: Option<String>,
    },
    /// The provider rejected the login with this HTTP status.
    Rejected {
        status: i64,
        refreshed_by: Option<String>,
    },
    NoQuota {
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum UsageReport {
    Available {
        plan_label: Option<String>,
        windows: Vec<UsageWindow>,
        balances: Vec<UsageBalance>,
        details: Vec<UsageDetail>,
    },
    Unavailable(UsageProblem),
    Error(String),
}

/// One login that failed while reading an account that has several.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageLoginError {
    pub harness: String,
    pub report: UsageReport,
}

/// The usage of one account of one usage source, such as a Claude login.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageReportEntry {
    /// `<source_id>:<account key>` on hosts with usage sources, the provider ID on older hosts.
    pub id: String,
    pub account_label: Option<String>,
    pub fetched_at: String,
    pub source_id: String,
    pub source_label: String,
    /// SVG markup for the source's icon.
    pub icon: Option<String>,
    pub report: UsageReport,
    pub login_errors: Vec<UsageLoginError>,
}

/// Which usage reports to read. `agent_id` asks for the account that agent runs under and can't
/// be combined with `report_ids`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReportsRequest {
    pub agent_id: Option<String>,
    /// The agent's provider. Hosts without usage sources can't name the agent's account, so an
    /// agent request there answers with this provider's entry from their per-provider list.
    pub provider: Option<String>,
    pub report_ids: Option<Vec<String>>,
    pub force_refresh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// A message from the provider about an accepted change, such as a mode change that applies only
/// after the current turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderNotice {
    pub kind: NoticeKind,
    pub message: String,
}

/// Creates the agent in a new git worktree branched off `base`, or the repository's default
/// branch when `base` is `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeTarget {
    pub new_branch: String,
    pub base: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CreateAgent {
    pub provider: String,
    pub model: Option<String>,
    pub directory: PathBuf,
    pub title: Option<String>,
    pub initial_prompt: Option<String>,
    /// The initial prompt's message ID, which its timeline item echoes back.
    pub client_message_id: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub images: Vec<ImageAttachment>,
    /// Daemon attachment objects, such as the chat history returned by `fork_context`.
    pub attachments: Vec<Value>,
    pub worktree: Option<WorktreeTarget>,
    pub idempotency_key: String,
    /// The Paseo workspace the agent joins. Without one the daemon starts a new workspace, and a
    /// new worktree always gets its own.
    pub workspace_id: Option<String>,
    /// Chosen feature values by feature ID, such as Codex's `fast_mode`.
    pub feature_values: BTreeMap<String, Value>,
}

/// How a message sent while the agent is running treats the active turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveTurnBehavior {
    Interrupt,
    Steer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachment {
    pub data_base64: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendMessage {
    pub agent_id: String,
    pub text: String,
    pub message_id: String,
    pub behavior: Option<ActiveTurnBehavior>,
    pub images: Vec<ImageAttachment>,
    /// Daemon attachment objects, such as [`UploadedFile::attachment`].
    pub attachments: Vec<Value>,
}

/// A file to upload to the daemon's host, as Paseo's attach button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileUpload {
    pub file_name: String,
    pub mime_type: String,
    /// RFC 3339.
    pub modified_at: String,
    pub bytes: Vec<u8>,
}

/// A file the daemon stored for this session; agents read it at `path` on the daemon's host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadedFile {
    pub id: String,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
    pub path: String,
}

impl UploadedFile {
    /// The attachment a message or new agent carries to hand the agent this file.
    pub fn attachment(&self) -> Value {
        serde_json::json!({
            "type": "uploaded_file",
            "id": self.id,
            "fileName": self.file_name,
            "mimeType": self.mime_type,
            "size": self.size,
            "path": self.path,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PermissionResponse {
    Allow {
        selected_action_id: Option<String>,
        /// Must be a JSON object when present.
        updated_input: Option<Value>,
    },
    Deny {
        selected_action_id: Option<String>,
        message: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectorySuggestion {
    pub path: String,
    pub is_directory: bool,
}

/// Agent settings used to list commands and features before the agent exists.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftConfig {
    pub provider: String,
    pub cwd: PathBuf,
    pub mode_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    /// Chosen feature values by feature ID, such as Codex's `fast_mode`.
    pub feature_values: BTreeMap<String, Value>,
}

/// A provider-defined agent option the composer shows beside the model, such as Codex's Fast
/// and Plan toggles. Providers name and describe their own features.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentFeature {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub tooltip: Option<String>,
    /// A Lucide icon name from the provider, such as `zap`.
    pub icon: Option<String>,
    /// Whether the desktop toolbar shows only the icon, as for Codex's Speed menu.
    pub icon_only: bool,
    pub kind: AgentFeatureKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentFeatureKind {
    Toggle(bool),
    Select {
        value: Option<String>,
        options: Vec<AgentFeatureOption>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentFeatureOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
}

/// A daemon-side git checkout, as shown in the changes panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckoutStatus {
    pub is_git: bool,
    pub repo_root: Option<String>,
    pub current_branch: Option<String>,
    /// The ref the current branch tracks, e.g. `refs/remotes/origin/main`.
    pub upstream_ref: Option<String>,
    pub is_dirty: bool,
    pub base_ref: Option<String>,
    /// Commits ahead of and behind the base branch.
    pub ahead_of_base: u64,
    pub behind_base: u64,
    pub ahead_of_origin: Option<u64>,
    pub behind_origin: Option<u64>,
    pub has_remote: bool,
    pub is_paseo_worktree: bool,
}

/// A file read from the daemon's host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// A branch the daemon suggests as a worktree base. Provenance and divergence are absent on
/// daemons that predate them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchSuggestion {
    pub name: String,
    /// Unix seconds of the branch tip's commit.
    pub committer_date: Option<i64>,
    pub has_local: Option<bool>,
    pub has_remote: Option<bool>,
    pub local_ahead: Option<u64>,
    pub local_behind: Option<u64>,
}

/// What a checkout diff compares the working tree against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffCompare {
    #[default]
    Uncommitted,
    Base,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub is_new: bool,
    pub is_deleted: bool,
    pub additions: u64,
    pub deletions: u64,
    pub hunks: Vec<DiffHunk>,
    /// `too_large` or `binary` when the daemon sent no hunks for the file.
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffHunk {
    pub old_start: u64,
    pub new_start: u64,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
    Header,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckoutDiff {
    pub files: Vec<DiffFile>,
    pub too_large: bool,
}
