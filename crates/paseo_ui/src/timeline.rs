use chrono::{DateTime, Datelike as _, TimeZone, Utc};
use paseo_client::{TimelineEntry, TimelinePayload};
use serde_json::Value;
use std::borrow::Borrow;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Canceled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub detail: Value,
    pub status: ToolStatus,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoEntry {
    pub text: String,
    pub completed: bool,
    pub in_progress: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamContent {
    User {
        text: String,
        /// The client message ID Paseo rewinds to.
        message_id: Option<String>,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Tool(ToolCall),
    Todo {
        items: Vec<TodoEntry>,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
    Compaction {
        loading: bool,
        pre_tokens: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamItem {
    /// Sequence of the first entry that produced this item; stable while chunks are appended.
    pub key: u64,
    pub timestamp: Option<DateTime<Utc>>,
    pub last_timestamp: Option<DateTime<Utc>>,
    pub content: StreamContent,
}

fn item_value(entry: &TimelineEntry) -> &Value {
    match &entry.payload {
        TimelinePayload::Message(value)
        | TimelinePayload::Tool(value)
        | TimelinePayload::Lifecycle(value)
        | TimelinePayload::Other(value) => value,
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub(crate) fn parse_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn tool_status(value: &Value) -> ToolStatus {
    match value.get("status").and_then(Value::as_str) {
        Some("running") => ToolStatus::Running,
        Some("canceled") => ToolStatus::Canceled,
        Some("failed") => ToolStatus::Failed,
        _ if value.get("error").is_some_and(|error| !error.is_null()) => ToolStatus::Failed,
        _ => ToolStatus::Completed,
    }
}

fn error_text(error: &Value) -> Option<String> {
    match error {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(error.to_string())),
        other => Some(other.to_string()),
    }
}

fn content_of(value: &Value) -> Option<StreamContent> {
    let kind = value.get("type").and_then(Value::as_str)?;
    Some(match kind {
        "user_message" => StreamContent::User {
            text: string_field(value, "text").unwrap_or_default(),
            message_id: string_field(value, "messageId"),
        },
        "assistant_message" => StreamContent::Assistant {
            text: string_field(value, "text").unwrap_or_default(),
        },
        "reasoning" => StreamContent::Reasoning {
            text: string_field(value, "text").unwrap_or_default(),
        },
        "tool_call" => StreamContent::Tool(ToolCall {
            call_id: string_field(value, "callId").unwrap_or_default(),
            name: string_field(value, "name").unwrap_or_else(|| "tool".into()),
            detail: value.get("detail").cloned().unwrap_or(Value::Null),
            status: tool_status(value),
            error: value.get("error").and_then(error_text),
        }),
        "todo" => StreamContent::Todo {
            items: value
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|item| {
                    let status = item.get("status").and_then(Value::as_str);
                    TodoEntry {
                        text: string_field(item, "text").unwrap_or_default(),
                        completed: item.get("completed").and_then(Value::as_bool) == Some(true)
                            || status == Some("completed"),
                        in_progress: status == Some("in_progress"),
                    }
                })
                .collect(),
        },
        "error" => StreamContent::Notice {
            level: NoticeLevel::Error,
            message: string_field(value, "message").unwrap_or_else(|| "Unknown error".into()),
        },
        "notification" => StreamContent::Notice {
            level: match value.get("level").and_then(Value::as_str) {
                Some("warning") => NoticeLevel::Warning,
                Some("error") => NoticeLevel::Error,
                _ => NoticeLevel::Info,
            },
            message: string_field(value, "message").unwrap_or_default(),
        },
        "compaction" => StreamContent::Compaction {
            loading: value.get("status").and_then(Value::as_str) == Some("loading"),
            pre_tokens: value.get("preTokens").and_then(Value::as_u64),
        },
        _ => return None,
    })
}

/// Turns stored timeline entries (projected history pages mixed with raw live chunks) into the
/// items the stream renders. Live chunks arrive one per delta, so consecutive assistant or
/// reasoning text is concatenated and tool calls are collapsed by call ID, matching the daemon's
/// own projection.
pub fn project_items<'a>(entries: impl IntoIterator<Item = &'a TimelineEntry>) -> Vec<StreamItem> {
    let mut projection = TimelineProjection::default();
    for entry in entries {
        projection.push(entry);
    }
    projection
        .items
        .into_iter()
        .map(Rc::unwrap_or_clone)
        .collect()
}

/// How `TimelineProjection::sync` brought its items up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionUpdate {
    Unchanged,
    /// Only entries after the ones already projected arrived, and were folded in.
    Appended,
    /// The timeline changed in a way appending can't follow, so it was projected again.
    Rebuilt,
}

/// `project_items` for one timeline, kept between store changes so streamed chunks are folded
/// into the existing items instead of re-projecting the whole timeline per chunk. Items are
/// shared, so rows holding them clone cheaply and an unchanged item keeps its pointer.
#[derive(Default)]
pub struct TimelineProjection {
    items: Vec<Rc<StreamItem>>,
    tool_indices: HashMap<String, usize>,
    last_message_id: Option<String>,
    last_sequence_end: Option<u64>,
    extends_last: bool,
    /// The epoch and store revision the items were projected from; `None` before the first sync.
    synced: Option<(Option<String>, u64)>,
    /// The store's rewrite mark for the timeline when last synced: when it moves, entries already
    /// folded in were replaced or removed, so appending isn't enough.
    synced_rewrite: u64,
    first_sequence: Option<u64>,
    entry_count: usize,
    /// The newest projected entry, compared on the next sync because the store can replace an
    /// entry in place under the same sequence, such as a merged row that grew.
    last_entry: Option<TimelineEntry>,
}

impl TimelineProjection {
    pub fn items(&self) -> &[Rc<StreamItem>] {
        &self.items
    }

    /// Brings the items up to date with `entries`, the timeline's entries of `epoch` in sequence
    /// order, at the store's `revision` for the timeline. Appends when the only change is new
    /// entries after the projected ones; anything else (another epoch, older pages loaded above,
    /// removed or replaced entries) projects the timeline again.
    pub fn sync<'a, Entries>(
        &mut self,
        epoch: Option<&str>,
        revision: u64,
        rewrite: u64,
        entries: impl Fn() -> Entries,
    ) -> ProjectionUpdate
    where
        Entries: Iterator<Item = &'a TimelineEntry>,
    {
        let synced_epoch = self.synced.as_ref().map(|(epoch, _)| epoch.as_deref());
        if synced_epoch == Some(epoch)
            && self.synced.as_ref().map(|(_, revision)| *revision) == Some(revision)
        {
            return ProjectionUpdate::Unchanged;
        }
        let appended = synced_epoch == Some(epoch)
            && self.synced_rewrite == rewrite
            && self.append_new_entries(entries());
        self.synced = Some((epoch.map(str::to_owned), revision));
        self.synced_rewrite = rewrite;
        if appended {
            return ProjectionUpdate::Appended;
        }
        self.reset();
        for entry in entries() {
            self.push(entry);
        }
        ProjectionUpdate::Rebuilt
    }

    /// Folds in the entries after the projected ones, or returns false without changing anything
    /// when the entries already projected are not all still there, unchanged.
    fn append_new_entries<'a>(&mut self, entries: impl Iterator<Item = &'a TimelineEntry>) -> bool {
        let Some(last_entry) = &self.last_entry else {
            return false;
        };
        let mut entries = entries.peekable();
        if entries.peek().map(|entry| entry.sequence) != self.first_sequence {
            return false;
        }
        let mut kept = 0;
        let mut new_entries = Vec::new();
        for entry in entries {
            if entry.sequence <= last_entry.sequence {
                kept += 1;
                if entry.sequence == last_entry.sequence && entry != last_entry {
                    return false;
                }
            } else {
                new_entries.push(entry);
            }
        }
        // Projected history entries replace the live chunks they cover, which may already be
        // folded into the items.
        if kept != self.entry_count
            || new_entries.is_empty()
            || new_entries
                .iter()
                .any(|entry| entry.extra.get("sourceSeqRanges").is_some())
        {
            return false;
        }
        for entry in new_entries {
            self.push(entry);
        }
        true
    }

    fn reset(&mut self) {
        let synced = self.synced.take();
        let synced_rewrite = self.synced_rewrite;
        *self = Self {
            synced,
            synced_rewrite,
            ..Self::default()
        };
    }

    fn push(&mut self, entry: &TimelineEntry) {
        self.first_sequence.get_or_insert(entry.sequence);
        self.entry_count += 1;
        self.last_entry = Some(entry.clone());
        let value = item_value(entry);
        let Some(content) = content_of(value) else {
            return;
        };
        let timestamp = parse_timestamp(&entry.timestamp);
        let message_id = string_field(value, "messageId");
        let adjacent = self.extends_last
            && self
                .last_sequence_end
                .is_some_and(|end| entry.sequence == end + 1);
        self.last_sequence_end = Some(
            entry
                .extra
                .get("seqEnd")
                .and_then(Value::as_u64)
                .unwrap_or(entry.sequence),
        );
        if let Some(last) = self.items.last_mut() {
            let same_message = adjacent
                && match (&self.last_message_id, &message_id) {
                    (Some(previous), Some(current)) => previous == current,
                    _ => true,
                };
            let appends = same_message
                && matches!(
                    (&last.content, &content),
                    (
                        StreamContent::Assistant { .. },
                        StreamContent::Assistant { .. }
                    ) | (
                        StreamContent::Reasoning { .. },
                        StreamContent::Reasoning { .. }
                    )
                );
            if appends {
                let last = Rc::make_mut(last);
                if let (
                    StreamContent::Assistant { text } | StreamContent::Reasoning { text },
                    StreamContent::Assistant { text: delta }
                    | StreamContent::Reasoning { text: delta },
                ) = (&mut last.content, &content)
                {
                    text.push_str(delta);
                }
                last.last_timestamp = timestamp.or(last.last_timestamp);
                if message_id.is_some() {
                    self.last_message_id = message_id;
                }
                return;
            }
        }
        self.extends_last = matches!(
            content,
            StreamContent::Assistant { .. } | StreamContent::Reasoning { .. }
        );
        if let StreamContent::Tool(call) = &content
            && !call.call_id.is_empty()
            && let Some(&index) = self.tool_indices.get(&call.call_id)
            && let Some(existing) = self.items.get_mut(index)
        {
            *existing = Rc::new(StreamItem {
                key: existing.key,
                timestamp: existing.timestamp,
                last_timestamp: timestamp.or(existing.last_timestamp),
                content,
            });
            return;
        }
        if let StreamContent::Tool(call) = &content
            && !call.call_id.is_empty()
        {
            self.tool_indices
                .insert(call.call_id.clone(), self.items.len());
        }
        self.last_message_id = message_id;
        self.items.push(Rc::new(StreamItem {
            key: entry.sequence,
            timestamp,
            last_timestamp: timestamp,
            content,
        }));
    }
}

/// A user prompt and everything the agent produced in response to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub items: std::ops::Range<usize>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

pub fn group_turns<Item: Borrow<StreamItem>>(items: &[Item]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let item: &StreamItem = item.borrow();
        let starts_turn = matches!(item.content, StreamContent::User { .. })
            && !matches!(
                index
                    .checked_sub(1)
                    .and_then(|previous| items.get(previous))
                    .map(|previous| -> &StreamItem { previous.borrow() }),
                Some(StreamItem {
                    content: StreamContent::User { .. },
                    ..
                })
            );
        match turns.last_mut() {
            Some(turn) if !starts_turn => {
                turn.items.end = index + 1;
                turn.ended_at = item.last_timestamp.or(turn.ended_at);
            }
            _ => turns.push(Turn {
                items: index..index + 1,
                started_at: item.timestamp,
                ended_at: item.last_timestamp,
            }),
        }
    }
    turns
}

/// The index of the latest finished turn. A running agent's last turn is still in progress, so
/// the turn before it is the latest finished one.
pub fn latest_finished_turn(turn_count: usize, running: bool) -> Option<usize> {
    turn_count.checked_sub(if running { 2 } else { 1 })
}

/// Copyable text of a turn's agent output: the assistant messages joined by blank lines.
pub fn turn_text<Item: Borrow<StreamItem>>(items: &[Item]) -> String {
    items
        .iter()
        .filter_map(|item| {
            let item: &StreamItem = item.borrow();
            match &item.content {
                StreamContent::Assistant { text } => Some(text.trim()),
                _ => None,
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        let remainder = seconds % 60;
        if remainder == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {remainder}s")
        }
    } else {
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    }
}

pub fn format_relative(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - timestamp).num_seconds().max(0);
    if seconds < 60 {
        "now".into()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 86_400 * 7 {
        format!("{}d", seconds / 86_400)
    } else if seconds < 86_400 * 365 {
        format!("{}w", seconds / (86_400 * 7))
    } else {
        format!("{}y", seconds / (86_400 * 365))
    }
}

/// A clock time for today's messages, with the date added for older ones.
pub fn format_message_time<Zone: TimeZone>(timestamp: DateTime<Zone>, now: DateTime<Zone>) -> String
where
    Zone::Offset: std::fmt::Display,
{
    if timestamp.date_naive() == now.date_naive() {
        timestamp.format("%H:%M").to_string()
    } else if timestamp.year() == now.year() {
        timestamp.format("%b %-d, %H:%M").to_string()
    } else {
        timestamp.format("%b %-d %Y, %H:%M").to_string()
    }
}

pub fn parse_optional_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value.and_then(Value::as_str).and_then(parse_timestamp)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Shell,
    Read,
    Edit,
    Write,
    Search,
    Fetch,
    SubAgent,
    Plan,
    Thinking,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDisplay {
    pub kind: ToolKind,
    pub label: String,
    pub summary: Option<String>,
}

/// One step of a subagent's log, which Paseo writes as a `[Tool] summary` line.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentAction {
    pub tool_name: String,
    pub summary: Option<String>,
}

impl SubagentAction {
    /// The action as one line: `Read /repo/a.rs`.
    pub fn describe(&self) -> String {
        match &self.summary {
            Some(summary) => format!("{} {summary}", self.tool_label()),
            None => self.tool_label(),
        }
    }

    /// The tool name as words, the way Paseo shows it: `web_fetch` becomes `Web Fetch`.
    pub fn tool_label(&self) -> String {
        self.tool_name
            .split(['.', '_', '-', ' '])
            .filter(|word| !word.is_empty())
            .map(|word| {
                let mut characters = word.chars();
                characters
                    .next()
                    .map(|first| first.to_uppercase().chain(characters).collect::<String>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A subagent log's actions in order, and the lines that are not actions.
pub fn parse_subagent_log(log: &str) -> (Vec<SubagentAction>, String) {
    let mut actions = Vec::new();
    let mut remaining = Vec::new();
    for line in log.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match subagent_action(trimmed) {
            Some(action) => actions.push(action),
            None => remaining.push(line),
        }
    }
    (actions, remaining.join("\n"))
}

fn subagent_action(line: &str) -> Option<SubagentAction> {
    let rest = line.strip_prefix('[')?;
    let (tool_name, summary) = rest.split_once(']')?;
    let tool_name = tool_name.trim();
    // An action line is `[Tool]` alone or followed by whitespace, like Paseo's pattern.
    if tool_name.is_empty() || !(summary.is_empty() || summary.starts_with(char::is_whitespace)) {
        return None;
    }
    let summary = summary.trim();
    Some(SubagentAction {
        tool_name: tool_name.to_owned(),
        summary: (!summary.is_empty()).then(|| summary.to_owned()),
    })
}

pub fn relative_path(path: &str, cwd: Option<&Path>) -> String {
    if let Some(cwd) = cwd
        && let Ok(relative) = Path::new(path).strip_prefix(cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().into_owned();
    }
    path.to_owned()
}

fn humanize(name: &str) -> String {
    if name.contains("__") || name.contains(':') || name.contains('.') || name.contains('/') {
        return name.to_owned();
    }
    let spaced = name.replace(['_', '-'], " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => "Tool".into(),
    }
}

/// What kind of tool a call is, without building its label and summary.
pub fn tool_kind(call: &ToolCall) -> ToolKind {
    match call.detail.get("type").and_then(Value::as_str) {
        Some("shell" | "worktree_setup") => ToolKind::Shell,
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("write") => ToolKind::Write,
        Some("search") => ToolKind::Search,
        Some("fetch") => ToolKind::Fetch,
        Some("sub_agent") => ToolKind::SubAgent,
        Some("plan") => ToolKind::Plan,
        Some("plain_text") => ToolKind::Other,
        _ if call.name == "thinking" => ToolKind::Thinking,
        _ => ToolKind::Other,
    }
}

pub fn tool_display(call: &ToolCall, cwd: Option<&Path>) -> ToolDisplay {
    let detail = &call.detail;
    let field = |key: &str| {
        detail
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty())
    };
    let path = || field("filePath").map(|path| relative_path(&path, cwd));
    let (label, summary) = match detail.get("type").and_then(Value::as_str) {
        Some("shell") => ("Shell".into(), field("command")),
        Some("read") => ("Read".into(), path()),
        Some("edit") => ("Edit".into(), path()),
        Some("write") => ("Write".into(), path()),
        Some("search") => ("Search".into(), field("query")),
        Some("fetch") => ("Fetch".into(), field("url")),
        Some("worktree_setup") => ("Worktree setup".into(), field("branchName")),
        Some("sub_agent") => (
            field("subAgentType").unwrap_or_else(|| "Task".into()),
            field("description"),
        ),
        Some("plan") => ("Plan".into(), None),
        Some("plain_text") => (field("label").unwrap_or_else(|| humanize(&call.name)), None),
        _ if call.name == "thinking" => ("Thinking".into(), None),
        _ => (humanize(&call.name), None),
    };
    ToolDisplay {
        kind: tool_kind(call),
        label,
        summary: summary.map(|summary| summary.lines().next().unwrap_or_default().to_owned()),
    }
}

/// One chat row's worth of a turn: an item on its own, or a run of tool calls shown as one group.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Item(usize),
    ToolRun(Range<usize>),
}

/// Whether a tool call joins a group. Plans and thinking read as prose, so Paseo keeps them out.
fn is_groupable_tool(item: &StreamItem) -> bool {
    match &item.content {
        StreamContent::Tool(call) => {
            !matches!(tool_kind(call), ToolKind::Plan | ToolKind::Thinking)
        }
        _ => false,
    }
}

/// Splits `range` of `items` into segments, grouping every unbroken run of tool calls, as
/// Paseo's overview does. Any other item ends a run.
pub fn tool_runs<Item: Borrow<StreamItem>>(items: &[Item], range: Range<usize>) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut run_start = None;
    for index in range.clone() {
        let groupable = items
            .get(index)
            .is_some_and(|item| is_groupable_tool(item.borrow()));
        match (groupable, run_start) {
            (true, None) => run_start = Some(index),
            (true, Some(_)) => {}
            (false, start) => {
                if let Some(start) = start {
                    segments.push(Segment::ToolRun(start..index));
                    run_start = None;
                }
                segments.push(Segment::Item(index));
            }
        }
    }
    if let Some(start) = run_start {
        segments.push(Segment::ToolRun(start..range.end));
    }
    segments
}

/// A tool group's summary, worded like Paseo's: "Ran 2 commands and used 1 other tool".
pub fn tool_group_label<'a>(calls: impl IntoIterator<Item = &'a ToolCall>) -> String {
    let mut edited = BTreeSet::new();
    let mut read = BTreeSet::new();
    let (mut commands, mut searches, mut other) = (0, 0, 0);
    for call in calls {
        let path = call.detail.get("filePath").and_then(Value::as_str);
        match tool_kind(call) {
            ToolKind::Edit | ToolKind::Write => {
                edited.insert(path.unwrap_or(&call.call_id).to_owned());
            }
            ToolKind::Shell => commands += 1,
            ToolKind::Read => {
                read.insert(path.unwrap_or(&call.call_id).to_owned());
            }
            ToolKind::Search => searches += 1,
            _ => other += 1,
        }
    }
    let count = |count: usize, verb: &str, one: &str, many: &str| {
        (count > 0).then(|| format!("{verb} {count} {}", if count == 1 { one } else { many }))
    };
    let parts = [
        count(edited.len(), "edited", "file", "files"),
        count(commands, "ran", "command", "commands"),
        count(read.len(), "read", "file", "files"),
        count(searches, "searched", "time", "times"),
        count(other, "used", "other tool", "other tools"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let sentence = match parts.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    let mut characters = sentence.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// Counts added and removed lines of a unified diff, ignoring file headers.
pub fn diff_stat(unified_diff: &str) -> (usize, usize) {
    unified_diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++") || line.starts_with("---") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    Hunk,
}

/// Lines to render for an edit, from its unified diff or else from old/new strings.
pub fn edit_diff_lines(detail: &Value) -> Vec<(DiffLineKind, String)> {
    if let Some(diff) = detail.get("unifiedDiff").and_then(Value::as_str) {
        return diff
            .lines()
            .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
            .filter(|line| !line.starts_with("diff ") && !line.starts_with("index "))
            .map(|line| {
                let kind = if line.starts_with("@@") {
                    DiffLineKind::Hunk
                } else if line.starts_with('+') {
                    DiffLineKind::Added
                } else if line.starts_with('-') {
                    DiffLineKind::Removed
                } else {
                    DiffLineKind::Context
                };
                let text = match kind {
                    DiffLineKind::Hunk => line.to_owned(),
                    _ => line.get(1..).unwrap_or_default().to_owned(),
                };
                (kind, text)
            })
            .collect();
    }
    let old = detail
        .get("oldString")
        .and_then(Value::as_str)
        .unwrap_or("");
    let new = detail
        .get("newString")
        .or_else(|| detail.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    old.lines()
        .map(|line| (DiffLineKind::Removed, line.to_owned()))
        .chain(
            new.lines()
                .map(|line| (DiffLineKind::Added, line.to_owned())),
        )
        .collect()
}

/// A file the agent edited during a turn, with the edits' combined diff lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<(DiffLineKind, String)>,
}

/// Files changed by a turn's completed edit and write tool calls, in first-edit order. Paseo keeps
/// no per-turn git checkpoints, so the tool calls are the record of what the turn changed.
pub fn turn_changes<Item: Borrow<StreamItem>>(items: &[Item]) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = Vec::new();
    for item in items {
        let item: &StreamItem = item.borrow();
        let StreamContent::Tool(call) = &item.content else {
            continue;
        };
        if call.status != ToolStatus::Completed
            || !matches!(
                call.detail.get("type").and_then(Value::as_str),
                Some("edit" | "write")
            )
        {
            continue;
        }
        let Some(path) = call
            .detail
            .get("filePath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        let lines = edit_diff_lines(&call.detail);
        let (additions, deletions) =
            lines
                .iter()
                .fold((0, 0), |(added, removed), (kind, _)| match kind {
                    DiffLineKind::Added => (added + 1, removed),
                    DiffLineKind::Removed => (added, removed + 1),
                    DiffLineKind::Context | DiffLineKind::Hunk => (added, removed),
                });
        match changes.iter_mut().find(|change| change.path == path) {
            Some(change) => {
                change.additions += additions;
                change.deletions += deletions;
                change.lines.push((DiffLineKind::Hunk, "⋯".into()));
                change.lines.extend(lines);
            }
            None => changes.push(FileChange {
                path: path.to_owned(),
                additions,
                deletions,
                lines,
            }),
        }
    }
    changes
}

/// One replacement an edit tool call made in a file.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEdit {
    pub old_text: String,
    pub new_text: String,
    /// Zero-based line where `new_text` starts in the file right after the edit, from a unified
    /// diff's hunk header.
    pub line_hint: Option<usize>,
    /// The call wrote the whole file, and its previous content is unknown.
    pub whole_file: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnFileEdits {
    pub path: String,
    pub edits: Vec<FileEdit>,
}

/// A turn's completed edit and write tool calls as replacements, grouped per file in first-edit
/// order and kept in call order within each file.
pub fn turn_edits(items: &[StreamItem]) -> Vec<TurnFileEdits> {
    let mut files: Vec<TurnFileEdits> = Vec::new();
    for item in items {
        let StreamContent::Tool(call) = &item.content else {
            continue;
        };
        if call.status != ToolStatus::Completed {
            continue;
        }
        let Some(path) = call
            .detail
            .get("filePath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        let Some(edits) = tool_call_edits(&call.detail) else {
            continue;
        };
        match files.iter_mut().find(|file| file.path == path) {
            Some(file) => file.edits.extend(edits),
            None => files.push(TurnFileEdits {
                path: path.to_owned(),
                edits,
            }),
        }
    }
    files
}

/// The replacements an edit or write tool call made, or `None` for any other tool.
pub fn tool_call_edits(detail: &Value) -> Option<Vec<FileEdit>> {
    match detail.get("type").and_then(Value::as_str) {
        Some("edit") => Some(detail_edits(detail)),
        Some("write") => Some(
            detail
                .get("content")
                .and_then(Value::as_str)
                .map(|content| vec![whole_file_edit(content)])
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

fn whole_file_edit(content: &str) -> FileEdit {
    FileEdit {
        old_text: String::new(),
        new_text: content.to_owned(),
        line_hint: None,
        whole_file: true,
    }
}

fn detail_edits(detail: &Value) -> Vec<FileEdit> {
    if let Some(diff) = detail.get("unifiedDiff").and_then(Value::as_str) {
        return unified_diff_edits(diff);
    }
    let old_text = detail.get("oldString").and_then(Value::as_str);
    let new_text = detail.get("newString").and_then(Value::as_str);
    match (old_text, new_text) {
        (None, Some(new_text)) => vec![whole_file_edit(new_text)],
        (Some(old_text), new_text) => vec![FileEdit {
            old_text: old_text.to_owned(),
            new_text: new_text.unwrap_or_default().to_owned(),
            line_hint: None,
            whole_file: false,
        }],
        (None, None) => Vec::new(),
    }
}

fn unified_diff_edits(diff: &str) -> Vec<FileEdit> {
    #[derive(Clone, Copy)]
    enum Side {
        Old,
        New,
        Both,
    }
    let mut edits: Vec<FileEdit> = Vec::new();
    let mut previous_side = None;
    for line in diff.lines() {
        if line.starts_with('\\') {
            // `\ No newline at end of file` removes the newline added after the previous line.
            if let (Some(edit), Some(side)) = (edits.last_mut(), previous_side) {
                if matches!(side, Side::Old | Side::Both) && edit.old_text.ends_with('\n') {
                    edit.old_text.pop();
                }
                if matches!(side, Side::New | Side::Both) && edit.new_text.ends_with('\n') {
                    edit.new_text.pop();
                }
            }
            continue;
        }
        if let Some(header) = line.strip_prefix("@@") {
            edits.push(FileEdit {
                old_text: String::new(),
                new_text: String::new(),
                line_hint: hunk_new_start(header),
                whole_file: false,
            });
            continue;
        }
        let Some(edit) = edits.last_mut() else {
            continue;
        };
        if let Some(text) = line.strip_prefix('+') {
            edit.new_text.push_str(text);
            edit.new_text.push('\n');
            previous_side = Some(Side::New);
        } else if let Some(text) = line.strip_prefix('-') {
            edit.old_text.push_str(text);
            edit.old_text.push('\n');
            previous_side = Some(Side::Old);
        } else if let Some(text) = line.strip_prefix(' ').or(line.is_empty().then_some("")) {
            edit.old_text.push_str(text);
            edit.old_text.push('\n');
            edit.new_text.push_str(text);
            edit.new_text.push('\n');
            previous_side = Some(Side::Both);
        }
    }
    edits
}

/// The zero-based line where a hunk's new side starts. An empty new side (`+9,0`) sits after the
/// named line, so its insertion point is that line's index.
fn hunk_new_start(header: &str) -> Option<usize> {
    let new_range = header
        .split_whitespace()
        .find_map(|part| part.strip_prefix('+'))?;
    let (start, count) = match new_range.split_once(',') {
        Some((start, count)) => (start.parse::<usize>().ok()?, count.parse::<usize>().ok()?),
        None => (new_range.parse::<usize>().ok()?, 1),
    };
    Some(if count == 0 {
        start
    } else {
        start.saturating_sub(1)
    })
}

/// Rebuilds a file's text from before its edits by undoing them last first. Returns `None` when
/// an edit can no longer be found: the file changed after the edits, or a deletion left nothing
/// to find it by.
pub fn reverse_edits(current: &str, edits: &[FileEdit]) -> Option<String> {
    let mut text = current.to_owned();
    for edit in edits.iter().rev() {
        if edit.whole_file {
            // Whatever came before a whole-file write is unknown, so earlier edits don't matter.
            return (text == edit.new_text).then(|| edit.old_text.clone());
        }
        let start = locate_edit(&text, edit)?;
        text.replace_range(start..start + edit.new_text.len(), &edit.old_text);
    }
    Some(text)
}

/// Rebuilds the text from before `edits` (oldest first) out of `current`, skipping edits that no
/// longer appear in it, such as ones rejected or rewritten since. Returns that text and, for each
/// edit it undid, the range of `current` holding that edit's text: empty for a deletion.
pub fn reverse_edits_tracking<Key: Clone>(
    current: &str,
    edits: &[(Key, FileEdit)],
) -> (String, Vec<(Key, Range<usize>)>) {
    let mut text = current.to_owned();
    // Each reversal replaced `new_len` bytes at `start` with `old_len` bytes, newest first.
    let mut reversals: Vec<(usize, usize, usize)> = Vec::new();
    let mut ranges = Vec::new();
    for (key, edit) in edits.iter().rev() {
        if edit.whole_file {
            // Whatever came before a whole-file write is unknown, so earlier edits don't matter.
            if text == edit.new_text {
                ranges.push((key.clone(), map_to_current(0..text.len(), &reversals)));
                text = edit.old_text.clone();
            }
            break;
        }
        let Some(start) = locate_edit(&text, edit) else {
            continue;
        };
        let end = start + edit.new_text.len();
        ranges.push((key.clone(), map_to_current(start..end, &reversals)));
        text.replace_range(start..end, &edit.old_text);
        reversals.push((start, edit.old_text.len(), edit.new_text.len()));
    }
    (text, ranges)
}

/// Maps a range in the text after `reversals` back to the text before any of them.
fn map_to_current(range: Range<usize>, reversals: &[(usize, usize, usize)]) -> Range<usize> {
    let mut range = range;
    for &(start, old_len, new_len) in reversals.iter().rev() {
        let map = |offset: usize, is_end: bool| {
            if offset <= start {
                offset
            } else if offset >= start + old_len {
                offset - old_len + new_len
            } else if is_end {
                start + new_len
            } else {
                start
            }
        };
        range = map(range.start, false)..map(range.end, true);
    }
    range
}

fn locate_edit(text: &str, edit: &FileEdit) -> Option<usize> {
    let hinted = edit.line_hint.and_then(|line| line_offset(text, line));
    if let Some(offset) = hinted
        && text
            .get(offset..)
            .is_some_and(|rest| rest.starts_with(edit.new_text.as_str()))
    {
        return Some(offset);
    }
    if edit.new_text.is_empty() {
        return None;
    }
    let mut matches = text.match_indices(edit.new_text.as_str());
    let (offset, _) = matches.next()?;
    matches.next().is_none().then_some(offset)
}

fn line_offset(text: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    text.match_indices('\n')
        .nth(line - 1)
        .map(|(offset, _)| offset + 1)
}

/// Old and new texts of a file's edits on their own, for a file whose edits can no longer be
/// found. Both sides share the separator lines, so the diff shows only the edits.
pub fn snippet_texts(edits: &[FileEdit]) -> (String, String) {
    let join = |side: fn(&FileEdit) -> &str| {
        edits
            .iter()
            .map(|edit| {
                let text = side(edit);
                if text.is_empty() || text.ends_with('\n') {
                    text.to_owned()
                } else {
                    format!("{text}\n")
                }
            })
            .collect::<Vec<_>>()
            .join("⋯\n")
    };
    (join(|edit| &edit.old_text), join(|edit| &edit.new_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace(old_text: &str, new_text: &str) -> FileEdit {
        FileEdit {
            old_text: old_text.into(),
            new_text: new_text.into(),
            line_hint: None,
            whole_file: false,
        }
    }

    #[test]
    fn reverse_edits_tracking_maps_each_edit_into_the_current_text() {
        let current = "let total = sum(values);\nprintln!(\"{total}\");\n";
        let edits = [
            ("first", replace("let x = 1;", "let total = sum(values);")),
            ("second", replace("print(x)", "println!(\"{total}\")")),
        ];
        let (base, ranges) = reverse_edits_tracking(current, &edits);
        assert_eq!(base, "let x = 1;\nprint(x);\n");
        let text_at = |key| {
            let (_, range) = ranges
                .iter()
                .find(|(found, _)| *found == key)
                .expect("edit found");
            &current[range.clone()]
        };
        assert_eq!(text_at("first"), "let total = sum(values);");
        assert_eq!(text_at("second"), "println!(\"{total}\")");
    }

    #[test]
    fn reverse_edits_tracking_places_deletions_and_skips_missing_edits() {
        let current = "keep\nend\n";
        let edits = [
            ("gone", replace("old line\n", "")),
            ("rejected", replace("x", "no longer here")),
        ];
        let (base, ranges) = reverse_edits_tracking(current, &edits);
        assert_eq!(
            base, current,
            "a deletion can't be placed without its surrounding text, and a rejected edit is gone"
        );
        assert!(ranges.is_empty());

        let edits = [
            ("rewritten", replace("one", "two")),
            ("again", replace("two", "three")),
        ];
        let (base, ranges) = reverse_edits_tracking("three", &edits);
        assert_eq!(
            base, "one",
            "a later edit of the agent's own text reverses through both"
        );
        assert_eq!(ranges.len(), 2);
        assert!(ranges.iter().all(|(_, range)| *range == (0..5)));
    }
    use serde_json::json;

    fn entry(sequence: u64, item: Value) -> TimelineEntry {
        TimelineEntry {
            agent_id: "agent".into(),
            epoch: "epoch".into(),
            sequence,
            timestamp: format!("2026-09-26T00:00:{:02}Z", sequence.min(59)),
            payload: paseo_client_payload(item),
            extra: json!({}),
        }
    }

    fn paseo_client_payload(item: Value) -> TimelinePayload {
        match item.get("type").and_then(Value::as_str) {
            Some("user_message" | "assistant_message" | "reasoning") => {
                TimelinePayload::Message(item)
            }
            Some("tool_call") => TimelinePayload::Tool(item),
            _ => TimelinePayload::Lifecycle(item),
        }
    }

    #[test]
    fn live_chunks_merge_and_tools_collapse_by_call_id() {
        let entries = vec![
            entry(1, json!({"type":"user_message","text":"Fix it"})),
            entry(2, json!({"type":"reasoning","text":"Let me "})),
            entry(3, json!({"type":"reasoning","text":"think"})),
            entry(
                4,
                json!({"type":"tool_call","callId":"c1","name":"shell","status":"running","detail":{"type":"shell","command":"ls"},"error":null}),
            ),
            entry(5, json!({"type":"assistant_message","text":"Done "})),
            entry(
                6,
                json!({"type":"tool_call","callId":"c1","name":"shell","status":"completed","detail":{"type":"shell","command":"ls","output":"a"},"error":null}),
            ),
            entry(7, json!({"type":"assistant_message","text":"now."})),
        ];
        let items = project_items(&entries);
        assert_eq!(items.len(), 5);
        assert_eq!(
            items[1].content,
            StreamContent::Reasoning {
                text: "Let me think".into()
            }
        );
        match &items[2].content {
            StreamContent::Tool(call) => {
                assert_eq!(call.status, ToolStatus::Completed);
                assert_eq!(call.detail["output"], "a");
            }
            other => panic!("expected tool, got {other:?}"),
        }
        assert_eq!(
            items[3].content,
            StreamContent::Assistant {
                text: "Done ".into()
            }
        );
        assert_eq!(
            items[4].content,
            StreamContent::Assistant {
                text: "now.".into()
            }
        );
        assert_eq!(items[2].key, 4);
    }

    fn projected(projection: &TimelineProjection) -> Vec<StreamItem> {
        projection
            .items()
            .iter()
            .map(|item| StreamItem::clone(item))
            .collect()
    }

    #[test]
    fn incremental_projection_matches_projecting_everything() {
        let chunks = vec![
            entry(1, json!({"type":"user_message","text":"Fix it"})),
            entry(2, json!({"type":"reasoning","text":"Let me "})),
            entry(3, json!({"type":"reasoning","text":"think"})),
            entry(
                4,
                json!({"type":"tool_call","callId":"c1","name":"shell","status":"running","detail":{"type":"shell","command":"ls"},"error":null}),
            ),
            entry(5, json!({"type":"assistant_message","text":"Done "})),
            entry(
                6,
                json!({"type":"tool_call","callId":"c1","name":"shell","status":"completed","detail":{"type":"shell","command":"ls","output":"a"},"error":null}),
            ),
            entry(7, json!({"type":"assistant_message","text":"now"})),
            entry(8, json!({"type":"assistant_message","text":"."})),
        ];
        let mut projection = TimelineProjection::default();
        let mut entries = Vec::new();
        let mut revision = 0;
        for chunk in chunks {
            entries.push(chunk);
            revision += 1;
            let update = projection.sync(Some("epoch"), revision, 0, || entries.iter());
            assert_eq!(projected(&projection), project_items(&entries));
            let expected = if entries.len() == 1 {
                ProjectionUpdate::Rebuilt
            } else {
                ProjectionUpdate::Appended
            };
            assert_eq!(update, expected, "after {} entries", entries.len());
        }
        assert_eq!(
            projection.sync(Some("epoch"), revision, 0, || entries.iter()),
            ProjectionUpdate::Unchanged
        );

        let shared = projection.items().first().cloned();
        revision += 1;
        entries.push(entry(9, json!({"type":"user_message","text":"Again"})));
        projection.sync(Some("epoch"), revision, 0, || entries.iter());
        assert!(
            shared
                .zip(projection.items().first())
                .is_some_and(|(before, after)| Rc::ptr_eq(&before, after)),
            "appending keeps the items it didn't touch"
        );

        // A refetched tail replaces the newest entry in place, under the same sequence.
        revision += 1;
        if let Some(last) = entries.last_mut() {
            *last = entry(
                9,
                json!({"type":"user_message","text":"Again, differently"}),
            );
        }
        assert_eq!(
            projection.sync(Some("epoch"), revision, 0, || entries.iter()),
            ProjectionUpdate::Rebuilt
        );
        assert_eq!(projected(&projection), project_items(&entries));

        // An older page loads above the projected entries.
        revision += 1;
        entries.insert(
            0,
            entry(0, json!({"type":"assistant_message","text":"Earlier"})),
        );
        entries.push(entry(
            10,
            json!({"type":"assistant_message","text":"Later"}),
        ));
        assert_eq!(
            projection.sync(Some("epoch"), revision, 0, || entries.iter()),
            ProjectionUpdate::Rebuilt
        );
        assert_eq!(projected(&projection), project_items(&entries));

        // A history page covering live chunks replaces them.
        revision += 1;
        let mut page_entry = entry(11, json!({"type":"assistant_message","text":"Paged"}));
        page_entry.extra = json!({"sourceSeqRanges": [[11, 11]]});
        entries.push(page_entry);
        assert_eq!(
            projection.sync(Some("epoch"), revision, 0, || entries.iter()),
            ProjectionUpdate::Rebuilt
        );
        assert_eq!(projected(&projection), project_items(&entries));

        // An older entry replaced in place in the same update as a new tail entry: only the
        // store's rewrite mark tells the two apart from a plain append.
        revision += 1;
        if let Some(first) = entries.first_mut() {
            *first = entry(
                0,
                json!({"type":"assistant_message","text":"Earlier, edited"}),
            );
        }
        entries.push(entry(
            12,
            json!({"type":"assistant_message","text":" More"}),
        ));
        assert_eq!(
            projection.sync(Some("epoch"), revision, 1, || entries.iter()),
            ProjectionUpdate::Rebuilt
        );
        assert_eq!(projected(&projection), project_items(&entries));

        revision += 1;
        assert_eq!(
            projection.sync(Some("next"), revision, 1, || entries.iter()),
            ProjectionUpdate::Rebuilt,
            "a new epoch starts over"
        );
    }

    #[test]
    fn different_message_ids_stay_separate() {
        let entries = vec![
            entry(
                1,
                json!({"type":"assistant_message","text":"One","messageId":"a"}),
            ),
            entry(
                2,
                json!({"type":"assistant_message","text":" more","messageId":"a"}),
            ),
            entry(
                3,
                json!({"type":"assistant_message","text":"Two","messageId":"b"}),
            ),
        ];
        let items = project_items(&entries);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].content,
            StreamContent::Assistant {
                text: "One more".into()
            }
        );
    }

    #[test]
    fn turns_start_at_user_prompts_and_measure_duration() {
        let entries = vec![
            entry(1, json!({"type":"user_message","text":"a"})),
            entry(2, json!({"type":"user_message","text":"b"})),
            entry(10, json!({"type":"assistant_message","text":"x"})),
            entry(20, json!({"type":"user_message","text":"c"})),
            entry(30, json!({"type":"assistant_message","text":"y"})),
        ];
        let items = project_items(&entries);
        let turns = group_turns(&items);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].items, 0..3);
        assert_eq!(
            turns[0]
                .ended_at
                .zip(turns[0].started_at)
                .map(|(end, start)| (end - start).num_seconds()),
            Some(9)
        );
        assert_eq!(turn_text(&items[turns[1].items.clone()]), "y");
    }

    #[test]
    fn tool_display_uses_paseo_labels_and_relative_paths() {
        let call = ToolCall {
            call_id: "c".into(),
            name: "edit".into(),
            detail: json!({"type":"edit","filePath":"/repo/src/main.rs","unifiedDiff":"--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n+more"}),
            status: ToolStatus::Completed,
            error: None,
        };
        let display = tool_display(&call, Some(Path::new("/repo")));
        assert_eq!(display.label, "Edit");
        assert_eq!(display.summary.as_deref(), Some("src/main.rs"));
        assert_eq!(
            diff_stat(call.detail["unifiedDiff"].as_str().unwrap_or("")),
            (2, 1)
        );
        let lines = edit_diff_lines(&call.detail);
        assert_eq!(lines[0].0, DiffLineKind::Hunk);
        assert_eq!(lines[1], (DiffLineKind::Removed, "old".into()));

        let mcp = ToolCall {
            name: "github__create_issue".into(),
            detail: json!({"type":"unknown"}),
            ..call.clone()
        };
        assert_eq!(tool_display(&mcp, None).label, "github__create_issue");
        let plain = ToolCall {
            name: "web_fetch".into(),
            detail: Value::Null,
            ..call
        };
        assert_eq!(tool_display(&plain, None).label, "Web fetch");
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(63), "1m 3s");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(3720), "1h 2m");
    }

    fn tool(key: u64, status: ToolStatus, detail: Value) -> StreamItem {
        StreamItem {
            key,
            timestamp: None,
            last_timestamp: None,
            content: StreamContent::Tool(ToolCall {
                call_id: key.to_string(),
                name: "edit".into(),
                detail,
                status,
                error: None,
            }),
        }
    }

    #[test]
    fn turn_changes_merge_edits_per_file() {
        let items = vec![
            tool(
                1,
                ToolStatus::Completed,
                json!({"type":"edit","filePath":"/p/a.rs","oldString":"one","newString":"two\nthree"}),
            ),
            tool(
                2,
                ToolStatus::Completed,
                json!({"type":"read","filePath":"/p/b.rs"}),
            ),
            tool(
                3,
                ToolStatus::Failed,
                json!({"type":"edit","filePath":"/p/c.rs","oldString":"x","newString":"y"}),
            ),
            tool(
                4,
                ToolStatus::Completed,
                json!({"type":"write","filePath":"/p/d.rs","content":"new file"}),
            ),
            tool(
                5,
                ToolStatus::Completed,
                json!({"type":"edit","filePath":"/p/a.rs","unifiedDiff":"@@ -1 +1 @@\n-three\n+four"}),
            ),
        ];
        let changes = turn_changes(&items);
        let summary = changes
            .iter()
            .map(|change| (change.path.as_str(), change.additions, change.deletions))
            .collect::<Vec<_>>();
        assert_eq!(summary, vec![("/p/a.rs", 3, 2), ("/p/d.rs", 1, 0)]);
        assert!(changes[0].lines.contains(&(DiffLineKind::Hunk, "⋯".into())));
    }

    #[test]
    fn consecutive_tool_calls_form_one_run() {
        let text = |key: u64| StreamItem {
            key,
            timestamp: None,
            last_timestamp: None,
            content: StreamContent::Assistant {
                text: "done".into(),
            },
        };
        let shell = |key: u64| {
            tool(
                key,
                ToolStatus::Completed,
                json!({"type":"shell","command":"ls"}),
            )
        };
        let items = vec![
            text(0),
            shell(1),
            shell(2),
            text(3),
            shell(4),
            tool(5, ToolStatus::Completed, json!({"type":"plan"})),
            shell(6),
        ];
        assert_eq!(
            tool_runs(&items, 0..items.len()),
            vec![
                Segment::Item(0),
                Segment::ToolRun(1..3),
                Segment::Item(3),
                Segment::ToolRun(4..5),
                Segment::Item(5),
                Segment::ToolRun(6..7),
            ]
        );
    }

    #[test]
    fn tool_group_labels_follow_paseo() {
        let call = |detail: Value| ToolCall {
            call_id: "c".into(),
            name: "tool".into(),
            detail,
            status: ToolStatus::Completed,
            error: None,
        };
        let shell = call(json!({"type":"shell","command":"ls"}));
        let other = call(json!({"type":"fetch","url":"https://x"}));
        assert_eq!(tool_group_label([&shell, &shell]), "Ran 2 commands");
        assert_eq!(
            tool_group_label([&shell, &other]),
            "Ran 1 command and used 1 other tool"
        );
        let edit_a = call(json!({"type":"edit","filePath":"/p/a.rs"}));
        let write_a = call(json!({"type":"write","filePath":"/p/a.rs"}));
        let read_b = call(json!({"type":"read","filePath":"/p/b.rs"}));
        let search = call(json!({"type":"search","query":"x"}));
        assert_eq!(
            tool_group_label([&edit_a, &write_a, &shell, &read_b, &search, &search]),
            "Edited 1 file, ran 1 command, read 1 file, and searched 2 times"
        );
    }

    #[test]
    fn parse_subagent_log_splits_actions_from_other_lines() {
        let (actions, remaining) = parse_subagent_log(
            "\n[Bash] ls crates\nThinking about it\n[Read] /repo/a.rs\n[web_fetch]\n[broken]x\n\n",
        );
        assert_eq!(
            actions,
            vec![
                SubagentAction {
                    tool_name: "Bash".into(),
                    summary: Some("ls crates".into()),
                },
                SubagentAction {
                    tool_name: "Read".into(),
                    summary: Some("/repo/a.rs".into()),
                },
                SubagentAction {
                    tool_name: "web_fetch".into(),
                    summary: None,
                },
            ]
        );
        assert_eq!(remaining, "Thinking about it\n[broken]x");
        assert_eq!(actions[2].tool_label(), "Web Fetch");
    }

    #[test]
    fn parse_subagent_log_handles_an_empty_log() {
        assert_eq!(parse_subagent_log(""), (Vec::new(), String::new()));
    }

    #[test]
    fn format_message_time_adds_the_date_for_older_messages() {
        let at = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .map(|time| time.with_timezone(&Utc))
                .expect("valid timestamp")
        };
        let now = at("2026-09-28T18:00:00Z");
        assert_eq!(
            format_message_time(at("2026-09-28T14:32:00Z"), now),
            "14:32"
        );
        assert_eq!(
            format_message_time(at("2026-09-27T09:05:00Z"), now),
            "Sep 27, 09:05"
        );
        assert_eq!(
            format_message_time(at("2025-12-31T23:59:00Z"), now),
            "Dec 31 2025, 23:59"
        );
    }

    fn string_edit(old_text: &str, new_text: &str) -> FileEdit {
        FileEdit {
            old_text: old_text.into(),
            new_text: new_text.into(),
            line_hint: None,
            whole_file: false,
        }
    }

    #[test]
    fn turn_edits_group_completed_edits_per_file() {
        let items = vec![
            tool(
                1,
                ToolStatus::Completed,
                json!({"type":"edit","filePath":"/p/a.rs","oldString":"one","newString":"two"}),
            ),
            tool(
                2,
                ToolStatus::Failed,
                json!({"type":"edit","filePath":"/p/a.rs","oldString":"x","newString":"y"}),
            ),
            tool(
                3,
                ToolStatus::Completed,
                json!({"type":"write","filePath":"/p/b.rs","content":"new file\n"}),
            ),
            tool(
                4,
                ToolStatus::Completed,
                json!({"type":"edit","filePath":"/p/a.rs","unifiedDiff":"@@ -3,2 +3,2 @@\n keep\n-old\n+new\n@@ -9 +9,0 @@\n-gone"}),
            ),
        ];
        let edits = turn_edits(&items);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].path, "/p/a.rs");
        assert_eq!(
            edits[0].edits,
            vec![
                string_edit("one", "two"),
                FileEdit {
                    old_text: "keep\nold\n".into(),
                    new_text: "keep\nnew\n".into(),
                    line_hint: Some(2),
                    whole_file: false,
                },
                FileEdit {
                    old_text: "gone\n".into(),
                    new_text: String::new(),
                    line_hint: Some(9),
                    whole_file: false,
                },
            ]
        );
        assert_eq!(
            edits[1].edits,
            vec![FileEdit {
                old_text: String::new(),
                new_text: "new file\n".into(),
                line_hint: None,
                whole_file: true,
            }]
        );
    }

    #[test]
    fn reverse_edits_undo_string_edits_last_first() {
        let edits = vec![
            string_edit("alpha", "beta"),
            string_edit("beta gamma", "delta"),
        ];
        assert_eq!(
            reverse_edits("start delta end", &edits).as_deref(),
            Some("start alpha gamma end")
        );
    }

    #[test]
    fn reverse_edits_use_line_hints_for_unified_diffs() {
        let edits = vec![
            FileEdit {
                old_text: "a\nold\n".into(),
                new_text: "a\nnew\n".into(),
                line_hint: Some(1),
                whole_file: false,
            },
            FileEdit {
                old_text: "removed\n".into(),
                new_text: String::new(),
                line_hint: Some(4),
                whole_file: false,
            },
        ];
        let current = "top\na\nnew\nx\nlast\n";
        assert_eq!(
            reverse_edits(current, &edits).as_deref(),
            Some("top\na\nold\nx\nremoved\nlast\n")
        );
    }

    #[test]
    fn reverse_edits_prefer_the_hinted_copy_of_repeated_text() {
        let edits = vec![FileEdit {
            old_text: "value = 1\n".into(),
            new_text: "value = 2\n".into(),
            line_hint: Some(2),
            whole_file: false,
        }];
        assert_eq!(
            reverse_edits("value = 2\nother\nvalue = 2\n", &edits).as_deref(),
            Some("value = 2\nother\nvalue = 1\n")
        );
    }

    #[test]
    fn reverse_edits_treat_a_write_as_a_new_file() {
        let write = FileEdit {
            old_text: String::new(),
            new_text: "content\n".into(),
            line_hint: None,
            whole_file: true,
        };
        assert_eq!(
            reverse_edits("content\n", std::slice::from_ref(&write)).as_deref(),
            Some("")
        );
        assert_eq!(reverse_edits("changed later\n", &[write]), None);
    }

    #[test]
    fn reverse_edits_fail_when_an_edit_cannot_be_found() {
        assert_eq!(reverse_edits("rewritten", &[string_edit("a", "b")]), None);
        assert_eq!(reverse_edits("b and b", &[string_edit("a", "b")]), None);
        assert_eq!(reverse_edits("text", &[string_edit("gone", "")]), None);
    }

    #[test]
    fn reverse_edits_stop_at_a_whole_file_write() {
        let write = FileEdit {
            old_text: String::new(),
            new_text: "written\n".into(),
            line_hint: None,
            whole_file: true,
        };
        assert_eq!(
            reverse_edits("written\n", &[string_edit("x", "y"), write]).as_deref(),
            Some("")
        );
    }

    #[test]
    fn unified_diff_edits_honor_missing_final_newlines() {
        let edits = unified_diff_edits(
            "@@ -1,2 +1,2 @@\n keep\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file",
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old_text, "keep\nold");
        assert_eq!(edits[0].new_text, "keep\nnew");
        assert_eq!(
            reverse_edits("keep\nnew", &edits).as_deref(),
            Some("keep\nold")
        );
    }

    #[test]
    fn snippet_texts_join_each_side_with_a_separator() {
        let (base, new) = snippet_texts(&[string_edit("one", "two"), string_edit("three\n", "")]);
        assert_eq!(base, "one\n⋯\nthree\n");
        assert_eq!(new, "two\n⋯\n");
    }
}
