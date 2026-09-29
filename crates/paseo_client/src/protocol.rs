use crate::{
    AgentCommand, AgentSummary, BranchSuggestion, CheckoutDiff, CheckoutStatus, DaemonStatus,
    DaemonUpdate, DiffFile, DiffHunk, DiffLine, DiffLineKind, DiffStat, DirectorySuggestion,
    FileContent, PaseoWorktree, PermissionRequest, ProjectDescriptor, Provider,
    ProviderAvailability, ProviderSubagent, ProviderUsage, RecoveryState, RelayStatus,
    SetupSnapshot, TerminalInfo, TimelineCursor, TimelineEntry, TimelinePage, TimelinePayload,
    UsageBalance, UsageDetail, UsageWindow, WorkspaceDescriptor, WorkspaceLabel, WorkspaceScript,
    subagent_timeline_id,
};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::Value;
use std::path::PathBuf;

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing or invalid {key}"))
}

fn required_array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("missing or invalid {key}"))
}

pub fn parse_providers(payload: &Value) -> Result<Vec<Provider>> {
    required_array(payload, "entries")?
        .iter()
        .map(|entry| {
            Ok(Provider {
                id: required_string(entry, "provider")?.to_owned(),
                label: entry
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: required_string(entry, "status")?.to_owned(),
                extra: entry.clone(),
            })
        })
        .collect()
}

pub fn parse_agent(agent: &Value) -> Result<AgentSummary> {
    let directory = agent.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    if agent
        .get("cwd")
        .and_then(Value::as_str)
        .is_some_and(|path| !is_absolute_workspace_path(path))
    {
        bail!("agent directory is not absolute");
    }
    Ok(AgentSummary {
        id: required_string(agent, "id")?.to_owned(),
        title: agent
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: required_string(agent, "status")?.to_owned(),
        directory,
        project: None,
        extra: agent.clone(),
    })
}

pub fn parse_agent_with_project(agent: &Value, project: Option<&Value>) -> Result<AgentSummary> {
    let mut summary = parse_agent(agent)?;
    summary.project = project.filter(|project| !project.is_null()).cloned();
    Ok(summary)
}

pub fn is_absolute_workspace_path(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || (path.starts_with("\\\\")
            && path[2..]
                .split(['\\', '/'])
                .filter(|part| !part.is_empty())
                .take(2)
                .count()
                == 2)
}

pub fn parse_agents(payload: &Value) -> Result<Vec<AgentSummary>> {
    required_array(payload, "entries")?
        .iter()
        .map(|entry| {
            parse_agent_with_project(
                entry.get("agent").context("missing directory agent")?,
                entry.get("project"),
            )
        })
        .collect()
}

pub fn parse_commands(payload: &Value) -> Result<Vec<AgentCommand>> {
    required_array(payload, "commands")?
        .iter()
        .map(|command| {
            Ok(AgentCommand {
                name: required_string(command, "name")?.to_owned(),
                description: required_string(command, "description")?.to_owned(),
                argument_hint: command
                    .get("argumentHint")
                    .and_then(Value::as_str)
                    .filter(|hint| !hint.is_empty())
                    .map(str::to_owned),
                kind: command
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

pub fn parse_directory_suggestions(payload: &Value) -> Result<Vec<DirectorySuggestion>> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo could not list directories: {error}");
    }
    let entries = payload
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Daemons older than the `entries` field only report directories.
    if entries.is_empty() {
        return required_array(payload, "directories")?
            .iter()
            .map(|directory| {
                Ok(DirectorySuggestion {
                    path: directory
                        .as_str()
                        .context("invalid directory suggestion")?
                        .to_owned(),
                    is_directory: true,
                })
            })
            .collect();
    }
    entries
        .iter()
        .map(|entry| {
            Ok(DirectorySuggestion {
                path: required_string(entry, "path")?.to_owned(),
                is_directory: required_string(entry, "kind")? == "directory",
            })
        })
        .collect()
}

pub fn parse_timeline(payload: &Value) -> Result<Vec<TimelineEntry>> {
    let agent_id = required_string(payload, "agentId")?;
    let epoch = required_string(payload, "epoch")?;
    required_array(payload, "entries")?
        .iter()
        .map(|entry| {
            let sequence = entry
                .get("seqStart")
                .and_then(Value::as_u64)
                .context("missing timeline sequence")?;
            let item = entry.get("item").context("missing timeline item")?.clone();
            let body = timeline_payload(item);
            Ok(TimelineEntry {
                agent_id: agent_id.to_owned(),
                epoch: epoch.to_owned(),
                sequence,
                timestamp: required_string(entry, "timestamp")?.to_owned(),
                payload: body,
                extra: entry.clone(),
            })
        })
        .collect()
}

fn parse_cursor(value: &Value) -> Result<Option<TimelineCursor>> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(TimelineCursor {
        epoch: required_string(value, "epoch")?.to_owned(),
        sequence: value
            .get("seq")
            .and_then(Value::as_u64)
            .context("missing timeline cursor sequence")?,
    }))
}

pub fn parse_timeline_page(payload: &Value) -> Result<TimelinePage> {
    Ok(TimelinePage {
        epoch: required_string(payload, "epoch")?.to_owned(),
        entries: parse_timeline(payload)?,
        start_cursor: parse_cursor(payload.get("startCursor").context("missing start cursor")?)?,
        end_cursor: parse_cursor(payload.get("endCursor").context("missing end cursor")?)?,
        has_older: payload
            .get("hasOlder")
            .and_then(Value::as_bool)
            .context("missing hasOlder")?,
        has_newer: payload
            .get("hasNewer")
            .and_then(Value::as_bool)
            .context("missing hasNewer")?,
    })
}

pub fn parse_subagent(value: &Value) -> Result<ProviderSubagent> {
    Ok(ProviderSubagent {
        id: required_string(value, "id")?.to_owned(),
        parent_agent_id: required_string(value, "parentAgentId")?.to_owned(),
        parent_subagent_id: optional_string(value, "parentSubagentId"),
        provider: required_string(value, "provider")?.to_owned(),
        title: optional_string(value, "title"),
        description: optional_string(value, "description"),
        status: required_string(value, "status")?.to_owned(),
        created_at: required_string(value, "createdAt")?.to_owned(),
        updated_at: required_string(value, "updatedAt")?.to_owned(),
        tool_call_id: optional_string(value, "toolCallId"),
        cwd: optional_string(value, "cwd"),
        subtitle: optional_string(value, "subtitle"),
    })
}

pub fn parse_subagents(payload: &Value) -> Result<Vec<ProviderSubagent>> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo could not list subagents: {error}");
    }
    required_array(payload, "subagents")?
        .iter()
        .map(parse_subagent)
        .collect()
}

/// A subagent timeline page, with its rows stored under the subagent's timeline ID.
pub fn parse_subagent_timeline_page(payload: &Value) -> Result<TimelinePage> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo could not load the subagent's conversation: {error}");
    }
    let timeline_id = subagent_timeline_id(
        required_string(payload, "parentAgentId")?,
        required_string(payload, "subagentId")?,
    );
    let epoch = required_string(payload, "epoch")?;
    let entries = required_array(payload, "rows")?
        .iter()
        .map(|row| {
            let sequence = row
                .get("seqStart")
                .or_else(|| row.get("seq"))
                .and_then(Value::as_u64)
                .context("missing subagent timeline sequence")?;
            let item = row
                .get("item")
                .context("missing subagent timeline item")?
                .clone();
            let mut extra = row.clone();
            // A merged row spans `seqStart` to `seq`; the store reads the span from `seqEnd`.
            if extra.get("seqEnd").is_none()
                && let Some(end) = row.get("seq").cloned()
                && let Some(fields) = extra.as_object_mut()
            {
                fields.insert("seqEnd".into(), end);
            }
            Ok(TimelineEntry {
                agent_id: timeline_id.clone(),
                epoch: epoch.to_owned(),
                sequence,
                timestamp: required_string(row, "timestamp")?.to_owned(),
                payload: timeline_payload(item),
                extra,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(TimelinePage {
        epoch: epoch.to_owned(),
        entries,
        start_cursor: payload
            .get("startCursor")
            .map(parse_cursor)
            .transpose()?
            .flatten(),
        end_cursor: payload
            .get("endCursor")
            .map(parse_cursor)
            .transpose()?
            .flatten(),
        has_older: payload
            .get("hasOlder")
            .and_then(Value::as_bool)
            .context("missing hasOlder")?,
        has_newer: payload
            .get("hasNewer")
            .and_then(Value::as_bool)
            .context("missing hasNewer")?,
    })
}

pub(crate) fn timeline_payload(item: Value) -> TimelinePayload {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "user_message" | "assistant_message" | "reasoning" => TimelinePayload::Message(item),
        "tool_call" => TimelinePayload::Tool(item),
        "error" | "notification" | "compaction" => TimelinePayload::Lifecycle(item),
        _ => TimelinePayload::Other(item),
    }
}

pub fn parse_permission(payload: &Value) -> Result<PermissionRequest> {
    let request = payload
        .get("request")
        .context("missing permission request")?;
    Ok(PermissionRequest {
        agent_id: required_string(payload, "agentId")?.to_owned(),
        request_id: required_string(request, "id")?.to_owned(),
        title: request
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| request.get("name").and_then(Value::as_str))
            .context("missing permission title")?
            .to_owned(),
        description: request
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        extra: request.clone(),
    })
}

pub fn response_payload(message: &Value, expected_type: &str) -> Result<Value> {
    if required_string(message, "type")? != expected_type {
        bail!("unexpected Paseo response type");
    }
    let payload = message.get("payload").context("missing response payload")?;
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo request failed: {error}");
    }
    Ok(payload.clone())
}

/// A response's error, sent either as text or, by newer requests, as `{code, message}`.
pub fn error_text(error: Option<&Value>) -> Option<String> {
    match error? {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object
            .get("message")
            .or_else(|| object.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

fn optional_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

pub fn parse_workspace(value: &Value) -> Result<WorkspaceDescriptor> {
    let project_root_path = PathBuf::from(required_string(value, "projectRootPath")?);
    let git = value.get("gitRuntime");
    Ok(WorkspaceDescriptor {
        id: required_string(value, "id")?.to_owned(),
        project_id: required_string(value, "projectId")?.to_owned(),
        project_display_name: optional_string(value, "projectDisplayName").unwrap_or_default(),
        directory: optional_string(value, "workspaceDirectory")
            .map(PathBuf::from)
            .unwrap_or_else(|| project_root_path.clone()),
        project_root_path,
        kind: optional_string(value, "workspaceKind").unwrap_or_else(|| "directory".into()),
        worktree_slug: optional_string(value, "worktreeSlug"),
        name: required_string(value, "name")?.to_owned(),
        title: optional_string(value, "title"),
        pinned_at: optional_string(value, "pinnedAt"),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect(),
        status: optional_string(value, "status").unwrap_or_else(|| "done".into()),
        activity_at: optional_string(value, "activityAt"),
        diff_stat: value.get("diffStat").and_then(|stat| {
            Some(DiffStat {
                additions: optional_u64(stat, "additions")?,
                deletions: optional_u64(stat, "deletions")?,
            })
        }),
        scripts: objects(value, "scripts")
            .filter_map(|script| parse_workspace_script(script).ok())
            .collect(),
        current_branch: git.and_then(|git| optional_string(git, "currentBranch")),
        is_paseo_worktree: git
            .and_then(|git| git.get("isPaseoOwnedWorktree"))
            .and_then(Value::as_bool)
            == Some(true),
        extra: value.clone(),
    })
}

pub fn parse_project(value: &Value) -> Result<ProjectDescriptor> {
    Ok(ProjectDescriptor {
        id: required_string(value, "projectId")?.to_owned(),
        display_name: required_string(value, "projectDisplayName")?.to_owned(),
        custom_name: optional_string(value, "projectCustomName"),
        icon_revision: optional_string(value, "projectCustomIconRevision")
            .or_else(|| optional_string(value, "projectIconRevision")),
        root_path: PathBuf::from(required_string(value, "projectRootPath")?),
        kind: optional_string(value, "projectKind").unwrap_or_else(|| "directory".into()),
    })
}

pub fn parse_projects(payload: &Value) -> Result<Vec<ProjectDescriptor>> {
    required_array(payload, "projects")?
        .iter()
        .map(parse_project)
        .collect()
}

/// A `fetch_workspaces_response` page: its workspaces, projects without workspaces, and the
/// cursor of the next page when there is one.
pub fn parse_workspace_page(
    payload: &Value,
) -> Result<(
    Vec<WorkspaceDescriptor>,
    Vec<ProjectDescriptor>,
    Option<String>,
)> {
    let workspaces = required_array(payload, "entries")?
        .iter()
        .map(parse_workspace)
        .collect::<Result<Vec<_>>>()?;
    let empty_projects = objects(payload, "emptyProjects")
        .map(parse_project)
        .collect::<Result<Vec<_>>>()?;
    let page_info = payload.get("pageInfo");
    let next_cursor = page_info
        .filter(|info| info.get("hasMore").and_then(Value::as_bool) == Some(true))
        .and_then(|info| optional_string(info, "nextCursor"));
    Ok((workspaces, empty_projects, next_cursor))
}

pub fn parse_label(value: &Value) -> Result<WorkspaceLabel> {
    Ok(WorkspaceLabel {
        name: required_string(value, "name")?.to_owned(),
        color: required_string(value, "color")?.to_owned(),
    })
}

pub fn parse_labels(payload: &Value) -> Result<Vec<WorkspaceLabel>> {
    required_array(payload, "labels")?
        .iter()
        .map(parse_label)
        .collect()
}

pub fn parse_workspace_script(value: &Value) -> Result<WorkspaceScript> {
    Ok(WorkspaceScript {
        name: required_string(value, "scriptName")?.to_owned(),
        kind: optional_string(value, "type").unwrap_or_else(|| "service".into()),
        hostname: optional_string(value, "hostname").unwrap_or_default(),
        port: optional_u64(value, "port").and_then(|port| u16::try_from(port).ok()),
        proxy_url: optional_string(value, "proxyUrl")
            .or_else(|| optional_string(value, "localProxyUrl")),
        running: value.get("lifecycle").and_then(Value::as_str) == Some("running"),
        health: optional_string(value, "health"),
        exit_code: value.get("exitCode").and_then(Value::as_i64),
        terminal_id: optional_string(value, "terminalId"),
    })
}

pub fn parse_workspace_scripts(payload: &Value) -> Result<Vec<WorkspaceScript>> {
    required_array(payload, "scripts")?
        .iter()
        .map(parse_workspace_script)
        .collect()
}

/// A setup snapshot from `workspace_setup_status_response` or a `workspace_setup_progress` event,
/// which carry the same fields.
pub fn parse_setup_snapshot(value: &Value) -> Result<SetupSnapshot> {
    Ok(SetupSnapshot {
        status: required_string(value, "status")?.to_owned(),
        log: value
            .get("detail")
            .and_then(|detail| optional_string(detail, "log")),
        error: optional_string(value, "error"),
    })
}

pub fn parse_recovery_state(payload: &Value) -> Result<RecoveryState> {
    let state = payload.get("state").context("missing recovery state")?;
    Ok(match required_string(state, "kind")? {
        "recoverable" => RecoveryState::Recoverable {
            workspace_name: optional_string(state, "workspaceName").unwrap_or_default(),
            action: optional_string(state, "action").unwrap_or_default(),
            branch: optional_string(state, "branch"),
        },
        _ => RecoveryState::Unavailable {
            reason: optional_string(state, "reason").unwrap_or_default(),
            message: optional_string(state, "message").unwrap_or_default(),
        },
    })
}

pub fn parse_provider_availability(payload: &Value) -> Result<Vec<ProviderAvailability>> {
    required_array(payload, "providers")?
        .iter()
        .map(|provider| {
            Ok(ProviderAvailability {
                provider: required_string(provider, "provider")?.to_owned(),
                available: provider.get("available").and_then(Value::as_bool) == Some(true),
                error: optional_string(provider, "error"),
            })
        })
        .collect()
}

pub fn parse_daemon_status(payload: &Value) -> Result<DaemonStatus> {
    Ok(DaemonStatus {
        server_id: required_string(payload, "serverId")?.to_owned(),
        version: optional_string(payload, "version"),
        pid: optional_u64(payload, "pid"),
        node_path: optional_string(payload, "nodePath"),
        started_at: optional_string(payload, "startedAt"),
        listen: optional_string(payload, "listen"),
        relay: payload
            .get("relay")
            .filter(|relay| relay.is_object())
            .map(|relay| RelayStatus {
                enabled: relay.get("enabled").and_then(Value::as_bool) == Some(true),
                endpoint: optional_string(relay, "endpoint"),
                public_endpoint: optional_string(relay, "publicEndpoint"),
            }),
        providers: parse_provider_availability(payload).unwrap_or_default(),
    })
}

pub fn parse_daemon_update(payload: &Value) -> Result<DaemonUpdate> {
    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        bail!(
            "Paseo could not update the daemon: {}",
            error_text(payload.get("error")).unwrap_or_else(|| "unknown reason".into())
        );
    }
    Ok(DaemonUpdate {
        previous_version: optional_string(payload, "previousVersion"),
        new_version: optional_string(payload, "newVersion"),
    })
}

pub fn parse_paseo_worktrees(payload: &Value) -> Result<Vec<PaseoWorktree>> {
    if let Some(error) = error_text(payload.get("error")) {
        bail!("Paseo could not list worktrees: {error}");
    }
    required_array(payload, "worktrees")?
        .iter()
        .map(|worktree| {
            Ok(PaseoWorktree {
                path: PathBuf::from(required_string(worktree, "worktreePath")?),
                created_at: optional_string(worktree, "createdAt").unwrap_or_default(),
                branch: optional_string(worktree, "branchName"),
                head: optional_string(worktree, "head"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_snapshot_preserves_unknown_optional_fields() {
        let providers = parse_providers(&json!({
            "entries": [{"provider": "codex", "status": "ready", "label": "Codex", "newField": {"future": true}}]
        }))
        .expect("valid provider snapshot");
        assert_eq!(providers[0].extra["newField"]["future"], true);
    }

    #[test]
    fn required_fields_are_validated() {
        assert!(parse_providers(&json!({"entries": [{"provider": "codex"}]})).is_err());
        assert!(parse_agent(&json!({"id": "a", "status": "idle", "cwd": "relative"})).is_err());
        assert!(parse_timeline(&json!({"agentId": "a", "epoch": "e", "entries": [{"timestamp": "now", "item": {}}]})).is_err());
    }

    #[test]
    fn windows_daemon_directory_is_preserved_on_linux() {
        let agent = parse_agent(&json!({
            "id": "a", "status": "idle", "cwd": "C:\\Users\\agent\\project"
        }))
        .expect("Windows absolute directory");
        assert_eq!(
            agent.directory.as_deref(),
            Some(std::path::Path::new("C:\\Users\\agent\\project"))
        );
    }

    #[test]
    fn directory_suggestion_errors_are_reported() {
        let error = parse_directory_suggestions(
            &json!({"entries": [], "directories": [], "error": "cwd not found"}),
        )
        .expect_err("daemon error");
        assert!(error.to_string().contains("cwd not found"));
    }

    #[test]
    fn empty_timeline_page_preserves_epoch() {
        let page = parse_timeline_page(&json!({
            "agentId":"agent-1", "epoch":"replacement-epoch", "entries":[],
            "startCursor":null, "endCursor":null, "hasOlder":false, "hasNewer":false
        }))
        .expect("empty page");
        assert_eq!(page.epoch, "replacement-epoch");
        assert!(page.entries.is_empty());
    }
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn objects<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.is_object())
}

pub fn parse_provider_usage(payload: &Value) -> Result<Vec<ProviderUsage>> {
    required_array(payload, "providers")?
        .iter()
        .map(|provider| {
            Ok(ProviderUsage {
                provider_id: required_string(provider, "providerId")?.to_owned(),
                display_name: required_string(provider, "displayName")?.to_owned(),
                status: required_string(provider, "status")?.to_owned(),
                plan_label: optional_string(provider, "planLabel"),
                source_label: optional_string(provider, "sourceLabel"),
                fetched_at: optional_string(provider, "fetchedAt")
                    .or_else(|| optional_string(payload, "fetchedAt")),
                error: optional_string(provider, "error"),
                windows: objects(provider, "windows")
                    .map(|window| UsageWindow {
                        label: optional_string(window, "label").unwrap_or_default(),
                        used_percent: window.get("usedPct").and_then(Value::as_f64).or_else(|| {
                            window
                                .get("remainingPct")
                                .and_then(Value::as_f64)
                                .map(|remaining| 100.0 - remaining)
                        }),
                        resets_at: optional_string(window, "resetsAt"),
                        runs_out_at: optional_string(window, "runsOutAt")
                            .filter(|_| window.get("shortfallPct").is_some_and(Value::is_number)),
                        tone: optional_string(window, "tone"),
                    })
                    .collect(),
                balances: objects(provider, "balances")
                    .map(|balance| UsageBalance {
                        label: optional_string(balance, "label").unwrap_or_default(),
                        used: balance.get("used").and_then(Value::as_f64),
                        remaining: balance.get("remaining").and_then(Value::as_f64),
                        limit: balance.get("limit").and_then(Value::as_f64),
                        unit: optional_string(balance, "unit").unwrap_or_default(),
                        tone: optional_string(balance, "tone"),
                    })
                    .collect(),
                details: objects(provider, "details")
                    .map(|detail| UsageDetail {
                        label: optional_string(detail, "label").unwrap_or_default(),
                        value: optional_string(detail, "value").unwrap_or_default(),
                    })
                    .collect(),
            })
        })
        .collect()
}

pub fn parse_terminal(terminal: &Value) -> Result<TerminalInfo> {
    Ok(TerminalInfo {
        id: required_string(terminal, "id")?.to_owned(),
        name: optional_string(terminal, "name").unwrap_or_default(),
        title: optional_string(terminal, "title").filter(|title| !title.is_empty()),
        cwd: optional_string(terminal, "cwd"),
    })
}

pub fn parse_terminals(payload: &Value) -> Result<Vec<TerminalInfo>> {
    required_array(payload, "terminals")?
        .iter()
        .map(parse_terminal)
        .collect()
}

/// Fails with the daemon's message when a checkout response carries an error.
pub fn checkout_error(payload: &Value, action: &str) -> Result<()> {
    match payload.get("error").filter(|error| !error.is_null()) {
        Some(error) => bail!(
            "Paseo could not {action}: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
                .unwrap_or("unknown git error")
        ),
        None => Ok(()),
    }
}

pub fn parse_checkout_status(payload: &Value) -> Result<CheckoutStatus> {
    checkout_error(payload, "read the git status")?;
    let count = |key: &str| payload.get(key).and_then(Value::as_u64);
    Ok(CheckoutStatus {
        is_git: payload.get("isGit").and_then(Value::as_bool) == Some(true),
        repo_root: optional_string(payload, "repoRoot"),
        current_branch: optional_string(payload, "currentBranch"),
        upstream_ref: optional_string(payload, "upstreamRef"),
        is_dirty: payload.get("isDirty").and_then(Value::as_bool) == Some(true),
        base_ref: optional_string(payload, "baseRef"),
        ahead_of_base: payload["aheadBehind"]["ahead"].as_u64().unwrap_or(0),
        behind_base: payload["aheadBehind"]["behind"].as_u64().unwrap_or(0),
        ahead_of_origin: count("aheadOfOrigin"),
        behind_origin: count("behindOfOrigin"),
        has_remote: payload.get("hasRemote").and_then(Value::as_bool) == Some(true),
        is_paseo_worktree: payload.get("isPaseoOwnedWorktree").and_then(Value::as_bool)
            == Some(true),
    })
}

pub fn parse_file_content(payload: &Value) -> Result<FileContent> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo could not read the file: {error}");
    }
    let file = payload
        .get("file")
        .filter(|file| !file.is_null())
        .context("Paseo returned no file")?;
    let content = file.get("content").and_then(Value::as_str).unwrap_or("");
    let bytes = match file.get("encoding").and_then(Value::as_str) {
        Some("base64") => base64::engine::general_purpose::STANDARD
            .decode(content)
            .context("Paseo sent an invalid base64 file")?,
        _ => content.as_bytes().to_vec(),
    };
    Ok(FileContent {
        bytes,
        mime_type: optional_string(file, "mimeType")
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
    })
}

pub fn parse_branch_suggestions(payload: &Value) -> Result<Vec<BranchSuggestion>> {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        bail!("Paseo could not list branches: {error}");
    }
    let details = objects(payload, "branchDetails")
        .map(|detail| {
            Ok(BranchSuggestion {
                name: required_string(detail, "name")?.to_owned(),
                committer_date: detail.get("committerDate").and_then(Value::as_i64),
                has_local: detail.get("hasLocal").and_then(Value::as_bool),
                has_remote: detail.get("hasRemote").and_then(Value::as_bool),
                local_ahead: detail.get("localAhead").and_then(Value::as_u64),
                local_behind: detail.get("localBehind").and_then(Value::as_u64),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !details.is_empty() {
        return Ok(details);
    }
    // Daemons older than `branchDetails` only send names.
    Ok(payload
        .get("branches")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(|name| BranchSuggestion {
            name: name.to_owned(),
            committer_date: None,
            has_local: None,
            has_remote: None,
            local_ahead: None,
            local_behind: None,
        })
        .collect())
}

pub fn parse_diff_file(file: &Value) -> Result<DiffFile> {
    let flag = |key: &str| file.get(key).and_then(Value::as_bool) == Some(true);
    Ok(DiffFile {
        path: required_string(file, "path")?.to_owned(),
        old_path: optional_string(file, "oldPath"),
        is_new: flag("isNew"),
        is_deleted: flag("isDeleted"),
        additions: file.get("additions").and_then(Value::as_u64).unwrap_or(0),
        deletions: file.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        hunks: objects(file, "hunks")
            .map(|hunk| DiffHunk {
                old_start: hunk.get("oldStart").and_then(Value::as_u64).unwrap_or(0),
                new_start: hunk.get("newStart").and_then(Value::as_u64).unwrap_or(0),
                lines: objects(hunk, "lines")
                    .map(|line| DiffLine {
                        kind: match line.get("type").and_then(Value::as_str) {
                            Some("add") => DiffLineKind::Added,
                            Some("remove") => DiffLineKind::Removed,
                            Some("header") => DiffLineKind::Header,
                            _ => DiffLineKind::Context,
                        },
                        content: optional_string(line, "content").unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect(),
        status: optional_string(file, "status").filter(|status| status != "ok"),
    })
}

pub fn parse_checkout_diff(payload: &Value) -> Result<CheckoutDiff> {
    checkout_error(payload, "read the diff")?;
    Ok(CheckoutDiff {
        files: required_array(payload, "files")?
            .iter()
            .map(parse_diff_file)
            .collect::<Result<_>>()?,
        too_large: payload.get("diffTooLarge").and_then(Value::as_bool) == Some(true),
    })
}
