use crate::{
    AgentSummary, PermissionRequest, Provider, TimelineCursor, TimelineEntry, TimelinePage,
    TimelinePayload,
};
use anyhow::{Context as _, Result, anyhow, bail};
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
        extra: agent.clone(),
    })
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
        .map(|entry| parse_agent(entry.get("agent").context("missing directory agent")?))
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
