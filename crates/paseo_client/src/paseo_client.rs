mod model;
mod protocol;
mod transport;

pub use model::*;
pub use protocol::is_absolute_workspace_path;
pub use transport::parse_ssh_uri;

use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::{Receiver, Sender};
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use transport::Socket;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
const TIMELINE_PAGE_SIZE: usize = 100;
const PING_INTERVAL: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

enum Command {
    Request {
        message: Value,
        response_type: &'static str,
        retry_creation: bool,
        reply: oneshot::Sender<Result<Value>>,
    },
    Close(oneshot::Sender<Result<()>>),
}

struct Pending {
    message: Value,
    response_type: &'static str,
    retry_creation: bool,
    reply: oneshot::Sender<Result<Value>>,
}

pub struct PaseoSession {
    commands: mpsc::Sender<Command>,
}

impl PaseoSession {
    async fn request(
        &self,
        message: Value,
        response_type: &'static str,
        retry_creation: bool,
    ) -> Result<Value> {
        self.request_with_timeout(message, response_type, retry_creation, REQUEST_TIMEOUT)
            .await
    }

    async fn request_with_timeout(
        &self,
        message: Value,
        response_type: &'static str,
        retry_creation: bool,
        timeout: Duration,
    ) -> Result<Value> {
        let request_type = message["type"].as_str().unwrap_or_default().to_owned();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Request {
                message,
                response_type,
                retry_creation,
                reply,
            })
            .await
            .context("Paseo connection closed")?;
        match tokio::time::timeout(timeout, response).await {
            Ok(result) => result.context("Paseo connection closed")?,
            Err(_) if request_type == "send_agent_message_request" => {
                bail!("Paseo request timed out; message outcome unknown")
            }
            Err(_) if retry_creation => {
                bail!(
                    "Paseo request timed out; creation outcome unknown; retry with the same idempotency key"
                )
            }
            Err(_) => bail!("Paseo request timed out"),
        }
    }

    pub async fn providers(&self, cwd: Option<&Path>) -> Result<Vec<Provider>> {
        let mut message =
            json!({"type":"get_providers_snapshot_request", "requestId":next_request_id()});
        if let Some(cwd) = cwd {
            message["cwd"] = json!(cwd.to_str().context("provider directory is not UTF-8")?);
        }
        let payload = self
            .request(message, "get_providers_snapshot_response", false)
            .await?;
        protocol::parse_providers(&payload)
    }

    pub async fn agents(&self) -> Result<Vec<AgentSummary>> {
        let mut agents = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut message = json!({"type":"fetch_agents_request", "requestId":next_request_id(), "scope":"active"});
            if let Some(cursor) = cursor.as_deref() {
                message["page"] = json!({"limit":200,"cursor":cursor});
            }
            let payload = self
                .request(message, "fetch_agents_response", false)
                .await?;
            agents.extend(protocol::parse_agents(&payload)?);
            cursor = next_agents_cursor(&payload)?;
            if cursor.is_none() {
                return Ok(agents);
            }
            if !seen_cursors.insert(cursor.clone().unwrap_or_default()) {
                bail!("Paseo directory returned a repeated page cursor");
            }
        }
    }

    pub async fn select_agent(&self, id: &str) -> Result<Vec<TimelineEntry>> {
        Ok(self.select_agent_page(id).await?.entries)
    }

    pub async fn select_agent_page(&self, id: &str) -> Result<TimelinePage> {
        self.request(json!({"type":"agent.timeline.set_subscription.request", "requestId":next_request_id(), "agentIds":[id]}), "agent.timeline.set_subscription.response", false).await?;
        let payload = self.request(json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE, "projection":"projected"}), "fetch_agent_timeline_response", false).await?;
        protocol::parse_timeline_page(&payload)
    }

    pub async fn timeline_before(&self, id: &str, cursor: &TimelineCursor) -> Result<TimelinePage> {
        let payload = self.request(json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":id, "direction":"before", "cursor":{"epoch":cursor.epoch,"seq":cursor.sequence}, "limit":TIMELINE_PAGE_SIZE, "projection":"projected", "mergeWindow":true}), "fetch_agent_timeline_response", false).await?;
        protocol::parse_timeline_page(&payload)
    }

    pub async fn create(&self, request: CreateAgent) -> Result<AgentSummary> {
        if request.idempotency_key.is_empty() || request.idempotency_key.len() > 512 {
            bail!("invalid creation idempotency key");
        }
        let cwd = request
            .directory
            .to_str()
            .context("agent directory is not UTF-8")?;
        if !protocol::is_absolute_workspace_path(cwd) {
            bail!("agent directory must be absolute");
        }
        let mut config = json!({"provider":request.provider, "cwd":cwd});
        if let Some(model) = request.model {
            config["model"] = json!(model);
        }
        if let Some(title) = request.title {
            config["title"] = json!(title);
        }
        let mut message = json!({
            "type":"agent.create.request",
            "requestId":next_request_id(),
            "idempotencyKey":request.idempotency_key,
            "config":config
        });
        if let Some(initial_prompt) = request.initial_prompt {
            message["initialPrompt"] = json!(initial_prompt);
        }
        let payload = self.request(message, "agent.create.response", true).await?;
        protocol::parse_agent(
            payload
                .get("agent")
                .filter(|agent| !agent.is_null())
                .context("creation did not return an agent")?,
        )
    }

    pub async fn send(&self, id: &str, text: &str, message_id: &str) -> Result<()> {
        if message_id.is_empty() {
            bail!("message ID must not be empty");
        }
        let payload = self.request(json!({"type":"send_agent_message_request", "requestId":next_request_id(), "agentId":id, "text":text, "messageId":message_id}), "send_agent_message_response", false).await?;
        if payload.get("accepted").and_then(Value::as_bool) != Some(true) {
            bail!(
                "Paseo did not accept the message: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown reason")
            );
        }
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<()> {
        self.request(
            json!({"type":"cancel_agent_request", "requestId":next_request_id(), "agentId":id}),
            "cancel_agent_response",
            false,
        )
        .await?;
        Ok(())
    }

    pub async fn answer_permission(&self, request_id: &str, allow: bool) -> Result<()> {
        let payload = self.request(json!({"type":"agent_permission_response", "requestId":request_id, "response":{"behavior":if allow {"allow"} else {"deny"}}}), "agent_permission_resolved", false).await?;
        if payload.get("requestId").and_then(Value::as_str) != Some(request_id) {
            bail!("permission acknowledgement mismatch");
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Close(reply))
            .await
            .context("Paseo connection closed")?;
        response.await.context("Paseo connection closed")?
    }
}

fn deliver<T>(sender: oneshot::Sender<T>, value: T) {
    if sender.send(value).is_err() {
        log::debug!("Paseo request receiver closed");
    }
}

fn emit_event(events: &Sender<PaseoEvent>, event: PaseoEvent) {
    if events.try_send(event).is_err() {
        log::debug!("Paseo event receiver closed");
    }
}

fn next_request_id() -> String {
    format!("zaseo-{}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
}

fn next_agents_cursor(payload: &Value) -> Result<Option<String>> {
    let page_info = payload.get("pageInfo").context("missing agent pageInfo")?;
    let has_more = page_info
        .get("hasMore")
        .and_then(Value::as_bool)
        .context("missing agent hasMore")?;
    let cursor = page_info
        .get("nextCursor")
        .context("missing agent nextCursor")?;
    if has_more {
        let cursor = cursor
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .context("missing next agent page cursor")?;
        Ok(Some(cursor.to_owned()))
    } else {
        Ok(None)
    }
}

pub async fn connect(
    target: ConnectionTarget,
    password: Option<RuntimePassword>,
    client_id: String,
) -> Result<(PaseoSession, Receiver<PaseoEvent>)> {
    connect_with_ssh_executable(target, password, client_id, PathBuf::from("ssh")).await
}

async fn connect_with_ssh_executable(
    target: ConnectionTarget,
    password: Option<RuntimePassword>,
    client_id: String,
    ssh_executable: PathBuf,
) -> Result<(PaseoSession, Receiver<PaseoEvent>)> {
    if client_id.trim().is_empty() {
        bail!("client ID must not be empty");
    }
    let mut socket = open_socket(&target, password.as_ref(), &client_id, &ssh_executable).await?;
    subscribe(&mut socket, None, None)
        .await
        .context("Paseo initial subscription failed")?;
    let (commands, command_receiver) = mpsc::channel(64);
    let (events, event_receiver) = async_channel::unbounded();
    events
        .send(PaseoEvent::Connected)
        .await
        .context("event receiver closed")?;
    tokio::spawn(run(
        socket,
        target,
        password,
        client_id,
        ssh_executable,
        command_receiver,
        events,
    ));
    Ok((PaseoSession { commands }, event_receiver))
}

async fn open_socket(
    target: &ConnectionTarget,
    password: Option<&RuntimePassword>,
    client_id: &str,
    ssh_executable: &Path,
) -> Result<Socket> {
    let websocket_url = transport::websocket_url(target)?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .context("invalid Paseo WebSocket request")?;
    if let Some(password) = password {
        let protocol = HeaderValue::from_str(&format!("paseo.bearer.{}", password.as_str()))
            .map_err(|_| anyhow!("password cannot be used in WebSocket subprotocol"))?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocol);
    }
    let (mut socket, response) = transport::connect_socket(target, request, ssh_executable).await?;
    if let Some(password) = password {
        let expected = format!("paseo.bearer.{}", password.as_str());
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            != Some(expected.as_str())
        {
            bail!("Paseo daemon did not accept password authentication");
        }
    }
    socket.send(Message::Text(json!({"type":"hello", "clientId":client_id, "clientType":"cli", "protocolVersion":1, "capabilities":{"owned_subscriptions":true, "explicit_event_subscriptions":true, "selective_agent_timeline":true, "all_providers":true, "timeline_replacement_invalidation":true}}).to_string().into())).await.context("Paseo hello failed")?;
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .context("Paseo hello timed out")?
        .context("Paseo closed during hello")?
        .context("Paseo hello failed")?;
    let value: Value =
        serde_json::from_slice(&message.into_data()).context("invalid Paseo hello response")?;
    let info = value
        .get("message")
        .filter(|_| value["type"] == "session")
        .context("missing Paseo session hello")?;
    if info["type"] != "status" || info["payload"]["status"] != "server_info" {
        bail!("Paseo daemon did not send server_info");
    }
    let features = &info["payload"]["features"];
    for feature in [
        "ownedSubscriptions",
        "providersSnapshot",
        "creationLifecycle",
    ] {
        if features[feature] != true {
            bail!("incompatible Paseo daemon: missing {feature}");
        }
    }
    Ok(socket)
}

async fn send_message(socket: &mut Socket, message: Value) -> Result<()> {
    socket
        .send(Message::Text(
            json!({"type":"session", "message":message})
                .to_string()
                .into(),
        ))
        .await
        .context("Paseo WebSocket send failed")
}

async fn subscribe(
    socket: &mut Socket,
    selected_agent: Option<&str>,
    cursor: Option<(&str, u64)>,
) -> Result<()> {
    send_message(socket, json!({"type":"session.events.set_subscription.request", "requestId":next_request_id(), "events":["agent_permission_request", "agent_permission_resolved"]})).await?;
    send_message(socket, json!({"type":"fetch_agents_request", "requestId":next_request_id(), "scope":"active", "subscribe":{}})).await?;
    if let Some(id) = selected_agent {
        send_message(socket, json!({"type":"agent.timeline.set_subscription.request", "requestId":next_request_id(), "agentIds":[id]})).await?;
        let mut history = json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE, "projection":"projected"});
        if let Some((epoch, sequence)) = cursor {
            history["direction"] = json!("after");
            history["cursor"] = json!({"epoch":epoch, "seq":sequence});
        }
        send_message(socket, history).await?;
    }
    Ok(())
}

async fn run(
    mut socket: Socket,
    target: ConnectionTarget,
    password: Option<RuntimePassword>,
    client_id: String,
    ssh_executable: PathBuf,
    mut commands: mpsc::Receiver<Command>,
    events: Sender<PaseoEvent>,
) {
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut agents: HashMap<String, AgentSummary> = HashMap::new();
    let mut permissions: HashMap<String, String> = HashMap::new();
    let mut subscriptions: HashMap<&'static str, String> = HashMap::new();
    let mut selected_agent: Option<String> = None;
    let mut cursor: Option<(String, u64)> = None;
    let mut ping_pending = false;
    let mut ping_timer =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    loop {
        tokio::select! {
            _ = ping_timer.tick() => {
                pending.retain(|_, request| !request.reply.is_closed());
                if ping_pending || socket.send(Message::Text(json!({"type":"ping"}).to_string().into())).await.is_err() {
                    if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                } else {
                    ping_pending = true;
                }
            },
            command = commands.recv() => match command {
                Some(Command::Request { message, response_type, retry_creation, reply }) => {
                    let Some(request_id) = message.get("requestId").and_then(Value::as_str).map(str::to_owned) else {
                        deliver(reply, Err(anyhow!("request ID missing")));
                        continue;
                    };
                    if message["type"] == "agent.timeline.set_subscription.request" {
                        selected_agent = message["agentIds"][0].as_str().map(str::to_owned);
                        cursor = None;
                    }
                    if message["type"] == "agent_permission_response" {
                        if let Some(agent_id) = permissions.get(&request_id) {
                            let mut message = message;
                            message["agentId"] = json!(agent_id);
                            if send_message(&mut socket, message.clone()).await.is_err() {
                                deliver(reply, Err(anyhow!("Paseo connection lost; permission outcome unknown")));
                                if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                            } else {
                                pending.insert(request_id, Pending { message, response_type, retry_creation, reply });
                            }
                            continue;
                        }
                        deliver(reply, Err(anyhow!("permission request is no longer pending")));
                        continue;
                    }
                    if send_message(&mut socket, message.clone()).await.is_err() {
                        if retry_creation { pending.insert(request_id, Pending { message, response_type, retry_creation, reply }); }
                        else { deliver(reply, Err(anyhow!("Paseo connection lost; request outcome unknown"))); }
                        if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                    } else {
                        pending.insert(request_id, Pending { message, response_type, retry_creation, reply });
                    }
                }
                Some(Command::Close(reply)) => {
                    let result = socket.close(None).await.context("Paseo close failed");
                    deliver(reply, result);
                    break;
                }
                None => break,
            },
            frame = socket.next() => {
                let result = frame.and_then(Result::ok);
                if let Some(Message::Text(text)) = result {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value["type"] == "session" {
                            let message = &value["message"];
                            let replacement_for_selected = message["type"] == "agent.timeline.replacement" && message["payload"]["agentId"].as_str() == selected_agent.as_deref();
                            let refresh = replacement_for_selected || (message["type"] == "fetch_agent_timeline_response" && (message["payload"]["staleCursor"] == true || message["payload"]["reset"] == true));
                            let directory_page = message["type"] == "fetch_agents_response"
                                && message["payload"]["requestId"].as_str().is_some_and(|request_id| !pending.contains_key(request_id));
                            let next_directory_cursor = if directory_page { next_agents_cursor(&message["payload"]).ok().flatten() } else { None };
                            let previous_cursor = cursor.clone();
                            let old_subscription = update_subscription(message, &mut subscriptions);
                            handle_message(message, &mut pending, &mut agents, &mut permissions, &events, selected_agent.as_deref(), &mut cursor);
                            if let Some(page_cursor) = next_directory_cursor {
                                if send_message(&mut socket, json!({"type":"fetch_agents_request", "requestId":next_request_id(), "scope":"active", "page":{"limit":200,"cursor":page_cursor}})).await.is_err() {
                                    if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                                }
                            }
                            if let Some(subscription_id) = old_subscription {
                                if send_message(&mut socket, json!({"type":"subscription.release.request", "requestId":next_request_id(), "subscriptionId":subscription_id})).await.is_err() {
                                    if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                                }
                            }
                            if refresh {
                                cursor = None;
                                if let Some(agent_id) = selected_agent.as_deref() {
                                    if send_message(&mut socket, json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":agent_id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE, "projection":"projected"})).await.is_err() {
                                        if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                                    }
                                }
                            } else if message["type"] == "fetch_agent_timeline_response"
                                && message["payload"]["direction"] == "after"
                                && message["payload"]["hasNewer"] == true
                                && cursor != previous_cursor
                            {
                                if let (Some(agent_id), Some((epoch, sequence))) = (selected_agent.as_deref(), cursor.as_ref()) {
                                    if send_message(&mut socket, json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":agent_id, "direction":"after", "cursor":{"epoch":epoch,"seq":sequence}, "limit":TIMELINE_PAGE_SIZE, "projection":"projected"})).await.is_err() {
                                        if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                                    }
                                }
                            }
                        } else if value["type"] == "pong" {
                            ping_pending = false;
                        } else if value["type"] == "ping" {
                            if socket.send(Message::Text(json!({"type":"pong"}).to_string().into())).await.is_err() {
                                if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                            }
                        }
                    }
                } else if result.is_none() {
                    if !reconnect(&mut socket, &target, &ssh_executable, password.as_ref(), &client_id, &events, &mut pending, &mut commands, selected_agent.as_deref(), cursor.as_ref(), &mut ping_pending).await { break; }
                }
            }
        }
    }
    pending.into_values().for_each(|pending| {
        deliver(pending.reply, Err(anyhow!("Paseo connection closed")));
    });
}

fn update_subscription(
    message: &Value,
    subscriptions: &mut HashMap<&'static str, String>,
) -> Option<String> {
    let kind = match message["type"].as_str() {
        Some("session.events.set_subscription.response") => "events",
        Some("fetch_agents_response") => "agents",
        Some("agent.timeline.set_subscription.response") => "timeline",
        _ => return None,
    };
    let new_id = message["payload"]["subscriptionId"].as_str()?;
    let previous = subscriptions.insert(kind, new_id.to_owned());
    previous.filter(|old_id| old_id != new_id)
}

fn handle_message(
    message: &Value,
    pending: &mut HashMap<String, Pending>,
    agents: &mut HashMap<String, AgentSummary>,
    permissions: &mut HashMap<String, String>,
    events: &Sender<PaseoEvent>,
    selected_agent: Option<&str>,
    cursor: &mut Option<(String, u64)>,
) {
    let message_type = message["type"].as_str().unwrap_or("");
    let payload = &message["payload"];
    if let Some(request_id) = payload.get("requestId").and_then(Value::as_str) {
        if pending.get(request_id).is_some_and(|request| {
            request.response_type == message_type || message_type == "rpc_error"
        }) {
            if let Some(request) = pending.remove(request_id) {
                let result = if message_type == "rpc_error" {
                    Err(anyhow!(
                        "Paseo request failed: {}",
                        payload["error"].as_str().unwrap_or("unknown error")
                    ))
                } else {
                    protocol::response_payload(message, request.response_type)
                };
                if let Ok(payload) = &result {
                    if message_type == "fetch_agent_timeline_response" {
                        emit_timeline(payload, events, cursor);
                    }
                    if message_type == "agent_permission_resolved" {
                        permissions.remove(request_id);
                        emit_event(
                            events,
                            PaseoEvent::PermissionResolved {
                                request_id: request_id.to_owned(),
                            },
                        );
                    }
                }
                deliver(request.reply, result);
            }
            return;
        }
    }
    match message_type {
        "fetch_agents_response" => update_agents(payload, agents, permissions, events),
        "agent.timeline.replacement" => {
            if payload["agentId"].as_str() == selected_agent
                && let (Some(agent_id), Some(epoch)) =
                    (payload["agentId"].as_str(), payload["epoch"].as_str())
            {
                emit_event(
                    events,
                    PaseoEvent::TimelineReplaced {
                        agent_id: agent_id.to_owned(),
                        epoch: epoch.to_owned(),
                    },
                );
            }
        }
        "agent_update" => {
            match payload["kind"].as_str() {
                Some("upsert") => {
                    if let Ok(agent) = protocol::parse_agent(&payload["agent"]) {
                        agents.insert(agent.id.clone(), agent);
                    }
                }
                Some("remove") => {
                    if let Some(id) = payload["agentId"].as_str() {
                        agents.remove(id);
                    }
                }
                _ => return,
            }
            emit_event(
                events,
                PaseoEvent::AgentsChanged(agents.values().cloned().collect()),
            );
        }
        "fetch_agent_timeline_response" => emit_timeline(payload, events, cursor),
        "agent_stream" => {
            if payload["agentId"].as_str() != selected_agent {
                return;
            }
            if let (Some(epoch), Some(sequence), Some(timestamp), Some(item)) = (
                payload["epoch"].as_str(),
                payload["seq"].as_u64(),
                payload["timestamp"].as_str(),
                payload["event"]["item"].as_object(),
            ) {
                let entry = TimelineEntry {
                    agent_id: selected_agent.unwrap_or_default().to_owned(),
                    epoch: epoch.to_owned(),
                    sequence,
                    timestamp: timestamp.to_owned(),
                    payload: protocol::timeline_payload(Value::Object(item.clone())),
                    extra: payload.clone(),
                };
                *cursor = Some((epoch.to_owned(), sequence));
                emit_event(events, PaseoEvent::TimelineEntry(entry));
            }
        }
        "agent_permission_request" => {
            if let Ok(permission) = protocol::parse_permission(payload) {
                permissions.insert(permission.request_id.clone(), permission.agent_id.clone());
                emit_event(events, PaseoEvent::PermissionRequested(permission));
            }
        }
        "agent_permission_resolved" => {
            if let Some(request_id) = payload["requestId"].as_str() {
                permissions.remove(request_id);
                emit_event(
                    events,
                    PaseoEvent::PermissionResolved {
                        request_id: request_id.to_owned(),
                    },
                );
            }
        }
        _ => {}
    }
}

fn update_agents(
    payload: &Value,
    agents: &mut HashMap<String, AgentSummary>,
    permissions: &mut HashMap<String, String>,
    events: &Sender<PaseoEvent>,
) {
    if let Ok(parsed) = protocol::parse_agents(payload) {
        if payload["subscriptionId"].as_str().is_some() {
            agents.clear();
        }
        for agent in parsed {
            if let Some(pending) = agent.extra["pendingPermissions"].as_array() {
                for request in pending {
                    if let Some(id) = request["id"].as_str() {
                        permissions.insert(id.to_owned(), agent.id.clone());
                    }
                }
            }
            agents.insert(agent.id.clone(), agent);
        }
        emit_event(
            events,
            PaseoEvent::AgentsChanged(agents.values().cloned().collect()),
        );
    }
}

fn emit_timeline(payload: &Value, events: &Sender<PaseoEvent>, cursor: &mut Option<(String, u64)>) {
    if let Ok(entries) = protocol::parse_timeline(payload) {
        let advance_cursor = payload["direction"] != "before";
        for entry in entries {
            if advance_cursor {
                *cursor = Some((
                    entry.epoch.clone(),
                    entry.extra["seqEnd"].as_u64().unwrap_or(entry.sequence),
                ));
            }
            emit_event(events, PaseoEvent::TimelineEntry(entry));
        }
        if advance_cursor
            && let (Some(epoch), Some(sequence)) = (
                payload["endCursor"]["epoch"].as_str(),
                payload["endCursor"]["seq"].as_u64(),
            )
        {
            *cursor = Some((epoch.to_owned(), sequence));
        }
    }
}

async fn reconnect(
    socket: &mut Socket,
    target: &ConnectionTarget,
    ssh_executable: &Path,
    password: Option<&RuntimePassword>,
    client_id: &str,
    events: &Sender<PaseoEvent>,
    pending: &mut HashMap<String, Pending>,
    commands: &mut mpsc::Receiver<Command>,
    selected_agent: Option<&str>,
    cursor: Option<&(String, u64)>,
    ping_pending: &mut bool,
) -> bool {
    emit_event(
        events,
        PaseoEvent::Disconnected {
            reason: "Paseo connection lost; reconnecting".into(),
        },
    );
    let mut retry = HashMap::new();
    for (request_id, request) in pending.drain() {
        if request.reply.is_closed() {
            continue;
        }
        if request.retry_creation {
            retry.insert(request_id, request);
        } else {
            let message = if request.message["type"] == "send_agent_message_request" {
                "Paseo connection lost; message outcome unknown"
            } else {
                "Paseo connection lost; request outcome unknown"
            };
            deliver(request.reply, Err(anyhow!(message)));
        }
    }
    *pending = retry;
    loop {
        if events.is_closed() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            command = commands.recv() => match command {
                Some(Command::Request { reply, .. }) => {
                    deliver(reply, Err(anyhow!("Paseo is reconnecting")));
                    continue;
                }
                Some(Command::Close(reply)) => {
                    deliver(reply, Ok(()));
                    return false;
                }
                None => return false,
            }
        }
        if let Ok(mut replacement) = open_socket(target, password, client_id, ssh_executable).await
        {
            let cursor = cursor.map(|(epoch, sequence)| (epoch.as_str(), *sequence));
            if subscribe(&mut replacement, selected_agent, cursor)
                .await
                .is_err()
            {
                continue;
            }
            let mut replay_failed = false;
            for request in pending.values() {
                if send_message(&mut replacement, request.message.clone())
                    .await
                    .is_err()
                {
                    replay_failed = true;
                    break;
                }
            }
            if replay_failed {
                continue;
            }
            *socket = replacement;
            *ping_pending = false;
            emit_event(events, PaseoEvent::Connected);
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::{WebSocketStream, tokio::accept_async};
    use futures::StreamExt;
    use tokio::net::{TcpListener, TcpStream};

    async fn server_socket(
        stream: TcpStream,
    ) -> async_tungstenite::WebSocketStream<async_tungstenite::tokio::TokioAdapter<TcpStream>> {
        let mut socket = accept_async(stream).await.expect("accept mock socket");
        let hello = next_json(&mut socket).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["protocolVersion"], 1);
        assert_eq!(hello["capabilities"]["selective_agent_timeline"], true);
        send_json(&mut socket, json!({"type":"session", "message":{"type":"status", "payload":{"status":"server_info", "serverId":"mock", "features":{"ownedSubscriptions":true,"providersSnapshot":true,"creationLifecycle":true}}}})).await;
        socket
    }

    async fn next_json<S>(socket: &mut WebSocketStream<S>) -> Value
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin,
    {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("message timed out")
            .expect("socket closed")
            .expect("read failed");
        serde_json::from_slice(&frame.into_data()).expect("valid JSON")
    }

    async fn send_json<S>(socket: &mut WebSocketStream<S>, value: Value)
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin,
    {
        socket
            .send(Message::Text(value.to_string().into()))
            .await
            .expect("write mock message");
    }

    async fn next_request<S>(socket: &mut WebSocketStream<S>, expected_type: &str) -> Value
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin,
    {
        loop {
            let value = next_json(socket).await;
            if value["type"] == "session" && value["message"]["type"] == expected_type {
                return value["message"].clone();
            }
        }
    }

    fn target(port: u16) -> ConnectionTarget {
        ConnectionTarget::Direct {
            websocket_url: format!("ws://127.0.0.1:{port}/ws"),
            editor_ssh: None,
        }
    }

    fn agent() -> Value {
        json!({"id":"agent-1", "title":"Task", "status":"idle", "cwd":"/tmp/project", "pendingPermissions":[], "futureField":{"ok":true}})
    }

    #[tokio::test]
    async fn handshake_correlates_replies_and_emits_subscriptions() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = server_socket(stream).await;
            let directory = next_request(&mut socket, "fetch_agents_request").await;
            assert_eq!(directory["subscribe"], json!({}));
            send_json(&mut socket, json!({"type":"session", "message":{"type":"fetch_agents_response", "payload":{"requestId":directory["requestId"], "entries":[{"agent":agent(), "project":{}}], "pageInfo":{"nextCursor":null,"prevCursor":null,"hasMore":false}}}})).await;
            let first = next_request(&mut socket, "get_providers_snapshot_request").await;
            let second = next_request(&mut socket, "get_providers_snapshot_request").await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"get_providers_snapshot_response", "payload":{"requestId":second["requestId"], "entries":[{"provider":"second","status":"ready"}]}}})).await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"get_providers_snapshot_response", "payload":{"requestId":first["requestId"], "entries":[{"provider":"first","status":"ready"}]}}})).await;
            let timeline_subscription =
                next_request(&mut socket, "agent.timeline.set_subscription.request").await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"agent.timeline.set_subscription.response", "payload":{"requestId":timeline_subscription["requestId"], "agentIds":["agent-1"]}}})).await;
            let history = next_request(&mut socket, "fetch_agent_timeline_request").await;
            assert_eq!(history["direction"], "tail");
            assert_eq!(history["limit"], TIMELINE_PAGE_SIZE);
            send_json(&mut socket, json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":history["requestId"],"agentId":"agent-1","epoch":"epoch-1","startCursor":{"epoch":"epoch-1","seq":1},"endCursor":{"epoch":"epoch-1","seq":1},"hasOlder":true,"hasNewer":false,"entries":[{"seqStart":1,"timestamp":"now","item":{"type":"assistant_message","text":"hello","future":7}}],"error":null}}})).await;
            let older = next_request(&mut socket, "fetch_agent_timeline_request").await;
            assert_eq!(older["direction"], "before");
            assert_eq!(older["cursor"], json!({"epoch":"epoch-1","seq":1}));
            assert_eq!(older["limit"], TIMELINE_PAGE_SIZE);
            send_json(&mut socket, json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":older["requestId"],"agentId":"agent-1","epoch":"epoch-1","startCursor":{"epoch":"epoch-1","seq":0},"endCursor":{"epoch":"epoch-1","seq":0},"hasOlder":false,"hasNewer":true,"entries":[{"seqStart":0,"timestamp":"before","item":{"type":"user_message","text":"earlier"}}],"error":null}}})).await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"agent_permission_request", "payload":{"agentId":"agent-1","request":{"id":"permission-1","provider":"codex","name":"Run command","kind":"tool"}}}})).await;
            let permission_response = next_request(&mut socket, "agent_permission_response").await;
            assert_eq!(permission_response["agentId"], "agent-1");
            send_json(&mut socket, json!({"type":"session", "message":{"type":"agent_permission_resolved", "payload":{"requestId":"permission-1","agentId":"agent-1","resolution":{"behavior":"allow"}}}})).await;
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        let (first, second) = tokio::join!(
            session.providers(None),
            session.providers(Some(Path::new("/tmp")))
        );
        assert_eq!(first.expect("first response")[0].id, "first");
        assert_eq!(second.expect("second response")[0].id, "second");
        let history = session.select_agent_page("agent-1").await.expect("history");
        assert_eq!(history.entries[0].sequence, 1);
        assert_eq!(history.entries[0].extra["item"]["future"], 7);
        assert!(history.has_older);
        let older = session
            .timeline_before(
                "agent-1",
                history.start_cursor.as_ref().expect("start cursor"),
            )
            .await
            .expect("older history");
        assert_eq!(older.entries[0].sequence, 0);
        assert!(!older.has_older);
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("event timed out")
                .expect("event receiver closed");
            if let PaseoEvent::PermissionRequested(permission) = event {
                assert_eq!(permission.request_id, "permission-1");
                break;
            }
        }
        session
            .answer_permission("permission-1", true)
            .await
            .expect("permission reply");
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn reconnect_renews_subscriptions_and_fetches_after_last_sequence() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.expect("first connection");
            let mut first = server_socket(first_stream).await;
            let _ = next_request(&mut first, "fetch_agents_request").await;
            let subscription =
                next_request(&mut first, "agent.timeline.set_subscription.request").await;
            send_json(&mut first, json!({"type":"session", "message":{"type":"agent.timeline.set_subscription.response", "payload":{"requestId":subscription["requestId"],"agentIds":["agent-1"]}}})).await;
            let history = next_request(&mut first, "fetch_agent_timeline_request").await;
            send_json(&mut first, json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":history["requestId"],"agentId":"agent-1","epoch":"epoch-1","startCursor":{"epoch":"epoch-1","seq":1},"endCursor":{"epoch":"epoch-1","seq":3},"hasOlder":false,"hasNewer":false,"entries":[{"seqStart":1,"seqEnd":3,"timestamp":"now","item":{"type":"assistant_message","text":"hello"}}],"error":null}}})).await;
            drop(first);
            let (second_stream, _) = listener.accept().await.expect("second connection");
            let mut second = server_socket(second_stream).await;
            let _ = next_request(&mut second, "fetch_agents_request").await;
            let renewed =
                next_request(&mut second, "agent.timeline.set_subscription.request").await;
            assert_eq!(renewed["agentIds"], json!(["agent-1"]));
            let catch_up = next_request(&mut second, "fetch_agent_timeline_request").await;
            assert_eq!(catch_up["direction"], "after");
            assert_eq!(catch_up["limit"], TIMELINE_PAGE_SIZE);
            assert_eq!(catch_up["cursor"], json!({"epoch":"epoch-1","seq":3}));
            send_json(&mut second, json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":catch_up["requestId"],"agentId":"agent-1","direction":"after","projection":"projected","epoch":"epoch-1","reset":false,"staleCursor":false,"gap":false,"window":{"minSeq":1,"maxSeq":5,"nextSeq":6},"startCursor":{"epoch":"epoch-1","seq":4},"endCursor":{"epoch":"epoch-1","seq":4},"hasOlder":true,"hasNewer":true,"entries":[{"seqStart":4,"seqEnd":4,"timestamp":"later","item":{"type":"assistant_message","text":"later"}}],"error":null}}})).await;
            let final_page = next_request(&mut second, "fetch_agent_timeline_request").await;
            assert_eq!(final_page["direction"], "after");
            assert_eq!(final_page["cursor"], json!({"epoch":"epoch-1","seq":4}));
            assert_eq!(final_page["limit"], TIMELINE_PAGE_SIZE);
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        let entries = session
            .select_agent("agent-1")
            .await
            .expect("initial history");
        assert_eq!(entries.len(), 1);
        server.await.expect("mock daemon task");
        let mut saw_disconnect = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("connection event timed out")
                .expect("event receiver closed");
            match event {
                PaseoEvent::Disconnected { .. } => saw_disconnect = true,
                PaseoEvent::Connected if saw_disconnect => break,
                _ => {}
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn password_uses_websocket_subprotocol_without_exposing_it_in_errors() {
        use async_tungstenite::tokio::accept_hdr_async;
        use async_tungstenite::tungstenite::handshake::server::{Request, Response};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let saw_password = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn({
            let saw_password = saw_password.clone();
            async move {
                let (stream, _) = listener.accept().await.expect("accept client");
                let mut socket =
                    accept_hdr_async(stream, move |request: &Request, mut response: Response| {
                        let protocol = request
                            .headers()
                            .get("Sec-WebSocket-Protocol")
                            .and_then(|value| value.to_str().ok());
                        saw_password.store(
                            protocol == Some("paseo.bearer.test-secret"),
                            Ordering::SeqCst,
                        );
                        response.headers_mut().insert(
                            "Sec-WebSocket-Protocol",
                            HeaderValue::from_static("paseo.bearer.test-secret"),
                        );
                        Ok(response)
                    })
                    .await
                    .expect("accept authenticated socket");
                let hello = next_json(&mut socket).await;
                assert_eq!(hello["type"], "hello");
                send_json(&mut socket, json!({"type":"session", "message":{"type":"status", "payload":{"status":"server_info", "serverId":"mock", "features":{"ownedSubscriptions":true,"providersSnapshot":true,"creationLifecycle":true}}}})).await;
                let _ = next_request(&mut socket, "fetch_agents_request").await;
            }
        });
        let (_session, _events) = connect(
            target(port),
            Some(RuntimePassword::new("test-secret".into())),
            "test-client".into(),
        )
        .await
        .expect("authenticated connection");
        server.await.expect("mock daemon task");
        assert!(saw_password.load(Ordering::SeqCst));
        let invalid = ConnectionTarget::Direct {
            websocket_url: "ws://user:password@127.0.0.1:1/ws".into(),
            editor_ssh: None,
        };
        let error = match connect(invalid, None, "test-client".into()).await {
            Ok(_) => panic!("URL credentials should be rejected"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("password@"));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let wrong_protocol = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let _socket = accept_hdr_async(stream, |_request: &Request, mut response: Response| {
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static("paseo.bearer.wrong"),
                );
                Ok(response)
            })
            .await
            .expect("accept mock socket");
        });
        let error = match connect(
            target(port),
            Some(RuntimePassword::new("test-secret".into())),
            "test-client".into(),
        )
        .await
        {
            Ok(_) => panic!("mismatched subprotocol should be rejected"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("test-secret"));
        wrong_protocol.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn application_ping_keeps_daemon_lease_alive() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = server_socket(stream).await;
            let _directory = next_request(&mut socket, "fetch_agents_request").await;
            let frame = tokio::time::timeout(PING_INTERVAL + Duration::from_secs(2), socket.next())
                .await
                .expect("application ping timed out")
                .expect("socket closed")
                .expect("read ping");
            let ping: Value = serde_json::from_slice(&frame.into_data()).expect("ping JSON");
            assert_eq!(ping, json!({"type":"ping"}));
            send_json(&mut socket, json!({"type":"pong"})).await;
        });
        let (_session, _events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        server.await.expect("mock daemon task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ssh_reconnect_starts_a_fresh_owned_tunnel() {
        use std::os::unix::fs::PermissionsExt;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let test_directory = std::env::temp_dir().join(format!(
            "zaseo-ssh-test-{}-{}",
            std::process::id(),
            next_request_id()
        ));
        std::fs::create_dir(&test_directory).expect("create mock SSH directory");
        let executable = test_directory.join("ssh");
        let process_log = test_directory.join("processes");
        let script = format!(
            r##"#!/usr/bin/env python3
import os
import select
import socket
import sys
with open({process_log:?}, "a", encoding="ascii") as log:
    log.write(str(os.getpid()) + "\n")
connection = socket.create_connection(("127.0.0.1", {port}))
input_fd = sys.stdin.buffer.fileno()
while True:
    ready, _, _ = select.select([input_fd, connection], [], [])
    if input_fd in ready:
        data = os.read(input_fd, 65536)
        if not data:
            break
        connection.sendall(data)
    if connection in ready:
        data = connection.recv(65536)
        if not data:
            break
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
"##,
            process_log = process_log.to_string_lossy()
        );
        std::fs::write(&executable, script).expect("write mock SSH executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("make mock SSH executable runnable");

        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.expect("first tunnel");
            let mut first = server_socket(first_stream).await;
            let _first_directory = next_request(&mut first, "fetch_agents_request").await;
            drop(first);
            let (second_stream, _) = listener.accept().await.expect("second tunnel");
            let mut second = server_socket(second_stream).await;
            let _second_directory = next_request(&mut second, "fetch_agents_request").await;
            let providers = next_request(&mut second, "get_providers_snapshot_request").await;
            send_json(&mut second, json!({"type":"session","message":{"type":"get_providers_snapshot_response","payload":{"requestId":providers["requestId"],"entries":[{"provider":"codex","status":"ready"}]}}})).await;
            let terminal_frame = second.next().await;
            assert!(matches!(
                terminal_frame,
                None | Some(Ok(Message::Close(_))) | Some(Err(_))
            ));
        });
        let target = ConnectionTarget::Ssh {
            host: "example.com".into(),
            username: Some("alice".into()),
            ssh_port: 2222,
            daemon_port: 6767,
        };
        let (session, events) =
            connect_with_ssh_executable(target, None, "test-client".into(), executable)
                .await
                .expect("connect through SSH");
        let mut saw_disconnect = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("reconnect event timed out")
                .expect("event receiver closed");
            match event {
                PaseoEvent::Disconnected { .. } => saw_disconnect = true,
                PaseoEvent::Connected if saw_disconnect => break,
                _ => {}
            }
        }
        assert_eq!(
            session
                .providers(None)
                .await
                .expect("providers after reconnect")[0]
                .id,
            "codex"
        );
        session.close().await.expect("close SSH session");
        server.await.expect("mock daemon task");
        let process_ids: Vec<_> = std::fs::read_to_string(&process_log)
            .expect("read mock SSH process log")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(process_ids.len(), 2);
        assert_ne!(process_ids[0], process_ids[1]);
        std::fs::remove_dir_all(&test_directory).expect("remove mock SSH files");
    }

    #[tokio::test]
    async fn creation_replays_with_same_idempotency_key_but_send_is_not_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.expect("first connection");
            let mut first = server_socket(first_stream).await;
            let creation = next_request(&mut first, "agent.create.request").await;
            assert_eq!(creation["idempotencyKey"], "stable-key");
            assert_eq!(creation["config"]["cwd"], "C:\\Users\\agent\\project");
            assert_eq!(creation["config"]["model"], "gpt-5.5");
            assert!(creation["config"].get("title").is_none());
            assert!(creation.get("initialPrompt").is_none());
            drop(first);
            let (second_stream, _) = listener.accept().await.expect("reconnection");
            let mut second = server_socket(second_stream).await;
            let replay = next_request(&mut second, "agent.create.request").await;
            assert_eq!(replay["idempotencyKey"], creation["idempotencyKey"]);
            assert_eq!(replay["requestId"], creation["requestId"]);
            send_json(&mut second, json!({"type":"session", "message":{"type":"agent.create.response", "payload":{"requestId":replay["requestId"],"agent":agent(),"error":null}}})).await;
            let send = next_request(&mut second, "send_agent_message_request").await;
            assert_eq!(send["messageId"], "message-1");
            drop(second);
            let (third_stream, _) = listener.accept().await.expect("second reconnection");
            let mut third = server_socket(third_stream).await;
            let _ = next_request(&mut third, "fetch_agents_request").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(200), third.next())
                    .await
                    .is_err()
            );
        });
        let (session, _events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        let created = session
            .create(CreateAgent {
                provider: "codex".into(),
                model: Some("gpt-5.5".into()),
                directory: "C:\\Users\\agent\\project".into(),
                title: None,
                initial_prompt: None,
                idempotency_key: "stable-key".into(),
            })
            .await
            .expect("creation replay");
        assert_eq!(created.id, "agent-1");
        let error = session
            .send("agent-1", "hello", "message-1")
            .await
            .expect_err("send should have unknown outcome");
        assert!(error.to_string().contains("outcome unknown"));
        server.await.expect("mock daemon task");
    }

    #[test]
    fn older_timeline_page_does_not_move_reconnect_cursor_backward() {
        let (reply, _response) = oneshot::channel();
        let mut pending = HashMap::from([(
            "before-page".to_string(),
            Pending {
                message: json!({"type":"fetch_agent_timeline_request", "requestId":"before-page"}),
                response_type: "fetch_agent_timeline_response",
                retry_creation: false,
                reply,
            },
        )]);
        let mut agents = HashMap::new();
        let mut permissions = HashMap::new();
        let (events, event_receiver) = async_channel::unbounded();
        let mut cursor = Some(("epoch-1".to_string(), 100));
        handle_message(
            &json!({
                "type":"fetch_agent_timeline_response",
                "payload":{
                    "requestId":"before-page",
                    "agentId":"agent-1",
                    "epoch":"epoch-1",
                    "direction":"before",
                    "entries":[{
                        "seqStart":20,
                        "seqEnd":20,
                        "timestamp":"now",
                        "item":{"type":"assistant_message","text":"older"}
                    }],
                    "endCursor":{"epoch":"epoch-1","seq":20}
                }
            }),
            &mut pending,
            &mut agents,
            &mut permissions,
            &events,
            Some("agent-1"),
            &mut cursor,
        );
        assert_eq!(cursor, Some(("epoch-1".to_string(), 100)));
        assert!(matches!(
            event_receiver.try_recv(),
            Ok(PaseoEvent::TimelineEntry(_))
        ));
    }

    #[tokio::test]
    async fn correlated_rpc_error_completes_request() {
        let (reply, response) = oneshot::channel();
        let mut pending = HashMap::from([(
            "request-1".to_string(),
            Pending {
                message: json!({"type":"fetch_agents_request", "requestId":"request-1"}),
                response_type: "fetch_agents_response",
                retry_creation: false,
                reply,
            },
        )]);
        let (events, _) = async_channel::unbounded();
        handle_message(
            &json!({"type":"rpc_error", "payload":{"requestId":"request-1", "error":"directory unavailable", "code":"fetch_agents_failed"}}),
            &mut pending,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            None,
            &mut None,
        );
        assert!(
            response
                .await
                .expect("reply")
                .expect_err("RPC error")
                .to_string()
                .contains("directory unavailable")
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn agents_request_every_page() {
        let (commands, mut receiver) = mpsc::channel(4);
        let session = PaseoSession { commands };
        let daemon = tokio::spawn(async move {
            let Some(Command::Request { message, reply, .. }) = receiver.recv().await else {
                panic!("first request")
            };
            assert!(message.get("page").is_none());
            deliver(
                reply,
                Ok(
                    json!({"entries":[{"agent":agent()}], "pageInfo":{"hasMore":true,"nextCursor":"next-page"}}),
                ),
            );
            let Some(Command::Request { message, reply, .. }) = receiver.recv().await else {
                panic!("second request")
            };
            assert_eq!(message["page"]["cursor"], "next-page");
            deliver(
                reply,
                Ok(
                    json!({"entries":[{"agent":{"id":"agent-2", "status":"idle", "cwd":"/tmp/project"}}], "pageInfo":{"hasMore":false,"nextCursor":null}}),
                ),
            );
        });
        let agents = session.agents().await.expect("all agents");
        assert_eq!(agents.len(), 2);
        daemon.await.expect("daemon task");
    }

    #[tokio::test]
    async fn rejected_send_shows_daemon_error() {
        let (commands, mut receiver) = mpsc::channel(1);
        let session = PaseoSession { commands };
        let daemon = tokio::spawn(async move {
            let Some(Command::Request { reply, .. }) = receiver.recv().await else {
                panic!("send request")
            };
            deliver(reply, Ok(json!({"accepted":false,"error":"agent is busy"})));
        });
        let error = session
            .send("agent-1", "hello", "message-1")
            .await
            .expect_err("rejected send");
        assert!(error.to_string().contains("agent is busy"));
        daemon.await.expect("daemon task");
    }

    #[tokio::test]
    async fn request_timeout_preserves_unknown_send_outcome() {
        let (commands, _receiver) = mpsc::channel(1);
        let session = PaseoSession { commands };
        let error = session
            .request_with_timeout(
                json!({"type":"send_agent_message_request", "requestId":"request-1"}),
                "send_agent_message_response",
                false,
                Duration::from_millis(1),
            )
            .await
            .expect_err("timed out send");
        assert!(error.to_string().contains("message outcome unknown"));
    }

    #[test]
    fn subscribed_directory_pages_append_without_replacing_live_updates() {
        let (events, _) = async_channel::unbounded();
        let mut agents = HashMap::new();
        let mut permissions = HashMap::new();
        let mut cursor = None;
        handle_message(
            &json!({"type":"fetch_agents_response", "payload":{
                "requestId":"subscription", "subscriptionId":"owned", "entries":[{"agent":agent()}],
                "pageInfo":{"hasMore":true,"nextCursor":"next"}
            }}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            None,
            &mut cursor,
        );
        handle_message(
            &json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":{
                "id":"live-agent", "status":"running", "cwd":"/tmp/project"
            }}}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            None,
            &mut cursor,
        );
        handle_message(
            &json!({"type":"fetch_agents_response", "payload":{
                "requestId":"next-page", "entries":[{"agent":{
                    "id":"agent-2", "status":"idle", "cwd":"/tmp/project"
                }}], "pageInfo":{"hasMore":false,"nextCursor":null}
            }}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            None,
            &mut cursor,
        );
        assert_eq!(agents.len(), 3);
        assert_eq!(agents["live-agent"].status, "running");
    }

    #[test]
    fn replacement_event_is_filtered_to_selected_agent() {
        let (events, receiver) = async_channel::unbounded();
        let replacement = json!({"type":"agent.timeline.replacement", "payload":{"agentId":"agent-1", "epoch":"epoch-2"}});
        handle_message(
            &replacement,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            Some("other"),
            &mut None,
        );
        assert!(receiver.try_recv().is_err());
        handle_message(
            &replacement,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            Some("agent-1"),
            &mut None,
        );
        assert!(
            matches!(receiver.try_recv(), Ok(PaseoEvent::TimelineReplaced { agent_id, epoch }) if agent_id == "agent-1" && epoch == "epoch-2")
        );
    }
}
