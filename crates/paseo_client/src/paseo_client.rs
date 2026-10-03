mod model;
mod protocol;
mod transport;

pub use model::*;
pub use protocol::{is_absolute_workspace_path, parse_features, pending_permissions};
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
const DIRECTORY_SUGGESTION_LIMIT: usize = 100;
const PING_INTERVAL: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Commits and pushes can run hooks; the daemon allows its own git commands two minutes.
const GIT_ACTION_TIMEOUT: Duration = Duration::from_secs(150);
const WORKSPACE_PAGE_SIZE: usize = 200;
/// Paseo's own client waits this long for a provider health check and a daemon update.
const PROVIDER_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(180);
const DAEMON_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);
const PROVIDER_REFRESH_TIMEOUT: Duration = Duration::from_secs(120);
const TERMINAL_RESTORE_SCROLLBACK: usize = 200;
const DICTATION_FORMAT: &str = "audio/pcm;rate=16000;bits=16";
/// Paseo's own client uploads in chunks this size.
const FILE_CHUNK_SIZE: usize = 128 * 1024;
const FILE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const FILE_BEGIN: u8 = 16;
const FILE_CHUNK: u8 = 17;
const FILE_END: u8 = 18;

enum Command {
    Request {
        message: Value,
        response_type: &'static str,
        retry_creation: bool,
        reply: oneshot::Sender<Result<Value>>,
    },
    /// A message the daemon never answers, such as terminal input or dictation audio.
    Notify(Value),
    /// A binary frame, such as part of a file upload.
    Binary(Vec<u8>),
    /// Releases a terminal output subscription and stops routing its frames.
    ReleaseTerminal {
        terminal_id: String,
        subscription_id: String,
    },
    Close(oneshot::Sender<Result<()>>),
}

struct Pending {
    /// The request to send again after a reconnect. Only creations are replayed, because their
    /// idempotency key makes a second delivery safe.
    replay: Option<Value>,
    /// Whether the request sends a chat message, whose outcome is unknown once the connection drops.
    sends_message: bool,
    response_type: &'static str,
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

    #[cfg(test)]
    pub async fn select_agent(&self, id: &str) -> Result<Vec<TimelineEntry>> {
        Ok(self.select_agent_page(id).await?.entries)
    }

    #[cfg(test)]
    pub async fn select_agent_page(&self, id: &str) -> Result<TimelinePage> {
        self.set_timeline_subscriptions(vec![id.to_owned()]).await?;
        self.timeline_tail(id).await
    }

    /// Replaces the set of agents whose live timeline is streamed. An agent newly added to the
    /// set needs a `timeline_tail` so reconnects can catch it up from a known cursor.
    pub async fn set_timeline_subscriptions(&self, agent_ids: Vec<String>) -> Result<()> {
        self.request(json!({"type":"agent.timeline.set_subscription.request", "requestId":next_request_id(), "agentIds":agent_ids}), "agent.timeline.set_subscription.response", false).await?;
        Ok(())
    }

    pub async fn timeline_tail(&self, agent_id: &str) -> Result<TimelinePage> {
        let payload = self.request(json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":agent_id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE, "projection":"projected"}), "fetch_agent_timeline_response", false).await?;
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
        if let Some(mode_id) = request.mode_id {
            config["modeId"] = json!(mode_id);
        }
        if let Some(thinking_option_id) = request.thinking_option_id {
            config["thinkingOptionId"] = json!(thinking_option_id);
        }
        if !request.feature_values.is_empty() {
            config["featureValues"] = json!(request.feature_values);
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
        if let Some(workspace_id) = request.workspace_id {
            message["workspaceId"] = json!(workspace_id);
        }
        if !request.images.is_empty() {
            message["images"] = image_payloads(request.images);
        }
        if !request.attachments.is_empty() {
            message["attachments"] = Value::Array(request.attachments);
        }
        if let Some(worktree) = request.worktree {
            if worktree.new_branch.trim().is_empty() {
                bail!("worktree branch name must not be empty");
            }
            let mut target = json!({"mode":"branch-off", "newBranch":worktree.new_branch});
            if let Some(base) = worktree.base {
                target["base"] = json!(base);
            }
            message["worktree"] = target;
        }
        // Creating a worktree runs `git worktree add` and setup scripts, which can outlast the
        // usual request timeout.
        let timeout = if message.get("worktree").is_some() {
            GIT_ACTION_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let payload = self
            .request_with_timeout(message, "agent.create.response", true, timeout)
            .await?;
        protocol::parse_agent(
            payload
                .get("agent")
                .filter(|agent| !agent.is_null())
                .context("creation did not return an agent")?,
        )
    }

    #[cfg(test)]
    pub async fn send(&self, id: &str, text: &str, message_id: &str) -> Result<()> {
        self.send_message(SendMessage {
            agent_id: id.to_owned(),
            text: text.to_owned(),
            message_id: message_id.to_owned(),
            behavior: None,
            images: Vec::new(),
            attachments: Vec::new(),
        })
        .await
    }

    pub async fn send_message(&self, message: SendMessage) -> Result<()> {
        if message.message_id.is_empty() {
            bail!("message ID must not be empty");
        }
        let mut request = json!({"type":"send_agent_message_request", "requestId":next_request_id(), "agentId":message.agent_id, "text":message.text, "messageId":message.message_id});
        if let Some(behavior) = message.behavior {
            request["activeTurnBehavior"] = json!(match behavior {
                ActiveTurnBehavior::Interrupt => "interrupt",
                ActiveTurnBehavior::Steer => "steer",
            });
        }
        if !message.images.is_empty() {
            request["images"] = image_payloads(message.images);
        }
        if !message.attachments.is_empty() {
            request["attachments"] = Value::Array(message.attachments);
        }
        let payload = self
            .request(request, "send_agent_message_response", false)
            .await?;
        require_accepted(&payload, "the message")
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

    #[cfg(test)]
    pub async fn answer_permission(&self, request_id: &str, allow: bool) -> Result<()> {
        let response = if allow {
            PermissionResponse::Allow {
                selected_action_id: None,
                updated_input: None,
            }
        } else {
            PermissionResponse::Deny {
                selected_action_id: None,
                message: None,
            }
        };
        self.respond_permission(request_id, response).await
    }

    pub async fn respond_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<()> {
        let response = match response {
            PermissionResponse::Allow {
                selected_action_id,
                updated_input,
            } => {
                let mut response = json!({"behavior":"allow"});
                if let Some(selected_action_id) = selected_action_id {
                    response["selectedActionId"] = json!(selected_action_id);
                }
                if let Some(updated_input) = updated_input {
                    if !updated_input.is_object() {
                        bail!("permission input must be a JSON object");
                    }
                    response["updatedInput"] = updated_input;
                }
                response
            }
            PermissionResponse::Deny {
                selected_action_id,
                message,
            } => {
                let mut response = json!({"behavior":"deny"});
                if let Some(selected_action_id) = selected_action_id {
                    response["selectedActionId"] = json!(selected_action_id);
                }
                if let Some(message) = message {
                    response["message"] = json!(message);
                }
                response
            }
        };
        let payload = self.request(json!({"type":"agent_permission_response", "requestId":request_id, "response":response}), "agent_permission_resolved", false).await?;
        if payload.get("requestId").and_then(Value::as_str) != Some(request_id) {
            bail!("permission acknowledgement mismatch");
        }
        Ok(())
    }

    pub async fn archive(&self, agent_id: &str) -> Result<()> {
        self.request(
            json!({"type":"archive_agent_request", "requestId":next_request_id(), "agentId":agent_id}),
            "agent_archived",
            false,
        )
        .await?;
        Ok(())
    }

    pub async fn unarchive(&self, agent_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"refresh_agent_request", "requestId":next_request_id(), "agentId":agent_id}),
                "status",
                false,
            )
            .await?;
        if payload.get("status").and_then(Value::as_str) != Some("agent_refreshed") {
            bail!("unexpected Paseo unarchive reply");
        }
        Ok(())
    }

    pub async fn delete(&self, agent_id: &str) -> Result<()> {
        self.request(
            json!({"type":"delete_agent_request", "requestId":next_request_id(), "agentId":agent_id}),
            "agent_deleted",
            false,
        )
        .await?;
        Ok(())
    }

    /// The agent's conversation as a chat-history attachment for starting a forked agent.
    pub async fn fork_context(&self, agent_id: &str) -> Result<Value> {
        let payload = self
            .request(
                json!({"type":"agent.fork_context.request", "requestId":next_request_id(), "agentId":agent_id}),
                "agent.fork_context.response",
                false,
            )
            .await?;
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            bail!("Paseo could not fork the agent: {error}");
        }
        payload
            .get("attachment")
            .filter(|attachment| attachment.is_object())
            .cloned()
            .context("Paseo returned no conversation to fork")
    }

    pub async fn rename(&self, agent_id: &str, name: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"update_agent_request", "requestId":next_request_id(), "agentId":agent_id, "name":name}),
                "update_agent_response",
                false,
            )
            .await?;
        require_accepted(&payload, "the rename")
    }

    pub async fn set_mode(&self, agent_id: &str, mode_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"set_agent_mode_request", "requestId":next_request_id(), "agentId":agent_id, "modeId":mode_id}),
                "set_agent_mode_response",
                false,
            )
            .await?;
        require_accepted(&payload, "the mode change")
    }

    pub async fn set_model(&self, agent_id: &str, model_id: Option<&str>) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"set_agent_model_request", "requestId":next_request_id(), "agentId":agent_id, "modelId":model_id}),
                "set_agent_model_response",
                false,
            )
            .await?;
        require_accepted(&payload, "the model change")
    }

    pub async fn set_thinking(&self, agent_id: &str, option_id: Option<&str>) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"set_agent_thinking_request", "requestId":next_request_id(), "agentId":agent_id, "thinkingOptionId":option_id}),
                "set_agent_thinking_response",
                false,
            )
            .await?;
        require_accepted(&payload, "the thinking change")
    }

    /// Sets one of the agent's provider features, such as Codex's `fast_mode`.
    pub async fn set_feature(&self, agent_id: &str, feature_id: &str, value: Value) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"set_agent_feature_request", "requestId":next_request_id(), "agentId":agent_id, "featureId":feature_id, "value":value}),
                "set_agent_feature_response",
                false,
            )
            .await?;
        require_accepted(&payload, "the feature change")
    }

    /// The features an agent with the draft's settings would have, for the composer before it
    /// exists.
    pub async fn provider_features(&self, draft: DraftConfig) -> Result<Vec<AgentFeature>> {
        let payload = self
            .request(
                json!({"type":"list_provider_features_request", "requestId":next_request_id(), "draftConfig":draft_config_json(draft)?}),
                "list_provider_features_response",
                false,
            )
            .await?;
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            bail!("Paseo could not list the provider's features: {error}");
        }
        Ok(payload
            .get("features")
            .map(protocol::parse_features)
            .unwrap_or_default())
    }

    pub async fn clear_attention(&self, agent_ids: Vec<String>) -> Result<()> {
        self.request(
            json!({"type":"clear_agent_attention", "requestId":next_request_id(), "agentId":agent_ids}),
            "clear_agent_attention_response",
            false,
        )
        .await?;
        Ok(())
    }

    /// Lists slash commands for an existing agent or, with `draft`, for an agent not yet created.
    pub async fn list_commands(
        &self,
        agent_id: Option<&str>,
        draft: Option<DraftConfig>,
    ) -> Result<Vec<AgentCommand>> {
        if agent_id.is_none() && draft.is_none() {
            bail!("listing commands needs an agent or a draft configuration");
        }
        let request_id = next_request_id();
        // The daemon requires an agent ID and falls back to the draft configuration when no
        // agent has that ID, so drafts use an ID no agent can have.
        let agent_id = agent_id.map_or_else(|| format!("draft:{request_id}"), str::to_owned);
        let mut message =
            json!({"type":"list_commands_request", "requestId":request_id, "agentId":agent_id});
        if let Some(draft) = draft {
            message["draftConfig"] = draft_config_json(draft)?;
        }
        let payload = self
            .request(message, "list_commands_response", false)
            .await?;
        protocol::parse_commands(&payload)
    }

    pub async fn directory_suggestions(
        &self,
        query: &str,
        cwd: Option<&str>,
        include_files: bool,
        include_directories: bool,
        limit: usize,
    ) -> Result<Vec<DirectorySuggestion>> {
        if !(1..=DIRECTORY_SUGGESTION_LIMIT).contains(&limit) {
            bail!("directory suggestion limit must be between 1 and {DIRECTORY_SUGGESTION_LIMIT}");
        }
        let mut message = json!({"type":"directory_suggestions_request", "requestId":next_request_id(), "query":query, "includeFiles":include_files, "includeDirectories":include_directories, "limit":limit});
        if let Some(cwd) = cwd {
            message["cwd"] = json!(cwd);
        }
        let payload = self
            .request(message, "directory_suggestions_response", false)
            .await?;
        protocol::parse_directory_suggestions(&payload)
    }

    /// One page of the daemon's agent history, active and archived agents together, most
    /// recently updated first, as Paseo's History screen asks for it: no filter, so the daemon's
    /// defaults (archived included) apply, and `search` matched by the daemon.
    pub async fn agent_history(
        &self,
        search: &str,
        cursor: Option<String>,
    ) -> Result<AgentHistoryPage> {
        let mut page = json!({"limit":AGENT_HISTORY_PAGE_LIMIT});
        if let Some(cursor) = cursor {
            page["cursor"] = json!(cursor);
        }
        let mut message = json!({"type":"fetch_agent_history_request", "requestId":next_request_id(), "sort":[{"key":"updated_at","direction":"desc"}], "page":page});
        let search = search.trim();
        if !search.is_empty() {
            message["search"] = json!(search);
        }
        let payload = self
            .request(message, "fetch_agent_history_response", false)
            .await?;
        Ok(AgentHistoryPage {
            agents: protocol::parse_agents(&payload)?,
            next_cursor: next_agents_cursor(&payload)?,
            search_truncated: payload["searchTruncated"] == true,
        })
    }

    async fn notify(&self, message: Value) -> Result<()> {
        self.commands
            .send(Command::Notify(message))
            .await
            .context("Paseo connection closed")
    }

    /// Restores the agent to just before the user message with `message_id`.
    pub async fn rewind(&self, agent_id: &str, message_id: &str, mode: RewindMode) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"agent.rewind.request", "requestId":next_request_id(), "agentId":agent_id, "messageId":message_id, "mode":mode.as_str()}),
                "agent.rewind.response",
                false,
            )
            .await?;
        if payload.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!(
                "Paseo could not rewind the agent: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Failed to rewind agent")
            );
        }
        Ok(())
    }

    pub async fn subagents(&self, parent_agent_id: &str) -> Result<Vec<ProviderSubagent>> {
        let payload = self
            .request(
                json!({"type":"agent.provider_subagents.list.request", "requestId":next_request_id(), "parentAgentId":parent_agent_id}),
                "agent.provider_subagents.list.response",
                false,
            )
            .await?;
        protocol::parse_subagents(&payload)
    }

    /// The subagent's latest timeline page, or the page before `cursor`.
    pub async fn subagent_timeline(
        &self,
        parent_agent_id: &str,
        subagent_id: &str,
        before: Option<&TimelineCursor>,
    ) -> Result<TimelinePage> {
        let mut message = json!({"type":"agent.provider_subagents.timeline.get.request", "requestId":next_request_id(), "parentAgentId":parent_agent_id, "subagentId":subagent_id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE});
        if let Some(cursor) = before {
            message["direction"] = json!("before");
            message["cursor"] = json!({"epoch":cursor.epoch, "seq":cursor.sequence});
        }
        let payload = self
            .request(
                message,
                "agent.provider_subagents.timeline.get.response",
                false,
            )
            .await?;
        protocol::parse_subagent_timeline_page(&payload)
    }

    pub async fn provider_usage(&self) -> Result<Vec<ProviderUsage>> {
        let payload = self
            .request(
                json!({"type":"provider.usage.list.request", "requestId":next_request_id()}),
                "provider.usage.list.response",
                false,
            )
            .await?;
        protocol::parse_provider_usage(&payload)
    }

    pub async fn terminals(&self, cwd: &str) -> Result<Vec<TerminalInfo>> {
        let payload = self
            .request(
                json!({"type":"list_terminals_request", "requestId":next_request_id(), "cwd":cwd}),
                "list_terminals_response",
                false,
            )
            .await?;
        protocol::parse_terminals(&payload)
    }

    /// Starts a shell in `cwd`. Current daemons reject agent-backed terminals, so terminals are
    /// scoped to a directory only.
    pub async fn create_terminal(&self, cwd: &str, rows: u16, cols: u16) -> Result<TerminalInfo> {
        let message = json!({"type":"create_terminal_request", "requestId":next_request_id(), "cwd":cwd, "size":{"rows":rows.max(1), "cols":cols.max(1)}});
        let payload = self
            .request(message, "create_terminal_response", false)
            .await?;
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            bail!("Paseo could not create a terminal: {error}");
        }
        protocol::parse_terminal(
            payload
                .get("terminal")
                .filter(|terminal| terminal.is_object())
                .context("Paseo returned no terminal")?,
        )
    }

    /// Streams the terminal's screen and output as `PaseoEvent::TerminalOutput`, starting with a
    /// restore of what is currently visible.
    /// Returns the daemon's subscription ID, which `release_terminal` needs.
    pub async fn subscribe_terminal(
        &self,
        terminal_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Option<String>> {
        let payload = self
            .request(
                json!({"type":"subscribe_terminal_request", "requestId":next_request_id(), "terminalId":terminal_id, "restore":{"mode":"visible-snapshot", "scrollbackLines":TERMINAL_RESTORE_SCROLLBACK, "size":{"rows":rows.max(1), "cols":cols.max(1)}}}),
                "subscribe_terminal_response",
                false,
            )
            .await?;
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            bail!("Paseo could not open the terminal: {error}");
        }
        Ok(payload
            .get("subscriptionId")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Streams the terminal list for `cwd` as `PaseoEvent::TerminalsChanged`, starting now. The
    /// daemon only opens an owned subscription, and so only sends updates, for requests with an ID.
    pub async fn watch_terminals(&self, cwd: &str) -> Result<()> {
        self.notify(
            json!({"type":"subscribe_terminals_request", "requestId":next_request_id(), "cwd":cwd}),
        )
        .await
    }

    /// Stops a terminal's output stream. Daemons that track owned subscriptions only release by
    /// subscription ID; the terminal ID form is for daemons that sent none.
    pub async fn release_terminal(
        &self,
        terminal_id: &str,
        subscription_id: Option<&str>,
    ) -> Result<()> {
        match subscription_id {
            Some(subscription_id) => self
                .commands
                .send(Command::ReleaseTerminal {
                    terminal_id: terminal_id.to_owned(),
                    subscription_id: subscription_id.to_owned(),
                })
                .await
                .context("Paseo connection closed"),
            None => {
                self.notify(
                    json!({"type":"unsubscribe_terminal_request", "terminalId":terminal_id}),
                )
                .await
            }
        }
    }

    pub async fn release_subscription(&self, subscription_id: &str) -> Result<()> {
        self.notify(json!({"type":"subscription.release.request", "requestId":next_request_id(), "subscriptionId":subscription_id}))
            .await
    }

    pub async fn terminal_input(&self, terminal_id: &str, data: String) -> Result<()> {
        self.notify(json!({"type":"terminal_input", "terminalId":terminal_id, "message":{"type":"input", "data":data}}))
            .await
    }

    pub async fn resize_terminal(&self, terminal_id: &str, rows: u16, cols: u16) -> Result<()> {
        self.notify(json!({"type":"terminal_input", "terminalId":terminal_id, "message":{"type":"resize", "rows":rows.max(1), "cols":cols.max(1), "intent":"claim"}}))
            .await
    }

    pub async fn kill_terminal(&self, terminal_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"kill_terminal_request", "requestId":next_request_id(), "terminalId":terminal_id}),
                "kill_terminal_response",
                false,
            )
            .await?;
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("Paseo could not close the terminal");
        }
        Ok(())
    }

    pub async fn rename_terminal(&self, terminal_id: &str, title: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"terminal.rename.request", "requestId":next_request_id(), "terminalId":terminal_id, "title":title}),
                "terminal.rename.response",
                false,
            )
            .await?;
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            bail!(
                "Paseo could not rename the terminal: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown reason")
            );
        }
        Ok(())
    }

    pub async fn start_dictation(&self, dictation_id: &str) -> Result<()> {
        self.notify(json!({"type":"dictation_stream_start", "dictationId":dictation_id, "format":DICTATION_FORMAT}))
            .await
    }

    /// Sends one chunk of 16 kHz mono PCM16 audio; `sequence` starts at zero.
    pub async fn dictation_chunk(
        &self,
        dictation_id: &str,
        sequence: u64,
        samples: &[i16],
    ) -> Result<()> {
        use base64::Engine as _;
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let audio = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.notify(json!({"type":"dictation_stream_chunk", "dictationId":dictation_id, "seq":sequence, "audio":audio, "format":DICTATION_FORMAT}))
            .await
    }

    /// Ends the audio; the transcript arrives as `PaseoEvent::DictationFinal`.
    pub async fn finish_dictation(&self, dictation_id: &str, final_sequence: u64) -> Result<()> {
        self.notify(json!({"type":"dictation_stream_finish", "dictationId":dictation_id, "finalSeq":final_sequence}))
            .await
    }

    pub async fn cancel_dictation(&self, dictation_id: &str) -> Result<()> {
        self.notify(json!({"type":"dictation_stream_cancel", "dictationId":dictation_id}))
            .await
    }

    pub async fn checkout_status(&self, cwd: &str) -> Result<CheckoutStatus> {
        let payload = self
            .request(
                json!({"type":"checkout_status_request", "requestId":next_request_id(), "cwd":cwd}),
                "checkout_status_response",
                false,
            )
            .await?;
        protocol::parse_checkout_status(&payload)
    }

    /// Reads a file by absolute path on the daemon's host, the way Paseo loads images that
    /// agents produce.
    pub async fn read_file(&self, path: &str) -> Result<FileContent> {
        let Some(relative) = path.strip_prefix('/') else {
            bail!("Paseo file paths must be absolute: {path}");
        };
        let payload = self
            .request(
                json!({"type":"file_explorer_request", "requestId":next_request_id(), "cwd":"/", "path":relative, "mode":"file"}),
                "file_explorer_response",
                false,
            )
            .await?;
        protocol::parse_file_content(&payload)
    }

    pub async fn branch_suggestions(
        &self,
        cwd: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<BranchSuggestion>> {
        let mut message = json!({"type":"branch_suggestions_request", "requestId":next_request_id(), "cwd":cwd, "limit":limit});
        if !query.trim().is_empty() {
            message["query"] = json!(query.trim());
        }
        let payload = self
            .request(message, "branch_suggestions_response", false)
            .await?;
        protocol::parse_branch_suggestions(&payload)
    }

    pub async fn checkout_diff(&self, cwd: &str, compare: DiffCompare) -> Result<CheckoutDiff> {
        let mode = match compare {
            DiffCompare::Uncommitted => "uncommitted",
            DiffCompare::Base => "base",
        };
        let payload = self
            .request(
                json!({"type":"checkout.diff.get.request", "requestId":next_request_id(), "cwd":cwd, "compare":{"mode":mode}}),
                "checkout.diff.get.response",
                false,
            )
            .await?;
        protocol::parse_checkout_diff(&payload)
    }

    /// Commits every change; the daemon writes the message when `message` is empty.
    pub async fn commit(&self, cwd: &str, message: &str) -> Result<()> {
        let mut request = json!({"type":"checkout_commit_request", "requestId":next_request_id(), "cwd":cwd, "addAll":true});
        if !message.trim().is_empty() {
            request["message"] = json!(message.trim());
        }
        self.checkout_action(request, "checkout_commit_response", "commit")
            .await
    }

    pub async fn pull(&self, cwd: &str) -> Result<()> {
        self.checkout_action(
            json!({"type":"checkout_pull_request", "requestId":next_request_id(), "cwd":cwd}),
            "checkout_pull_response",
            "pull",
        )
        .await
    }

    pub async fn push(&self, cwd: &str) -> Result<()> {
        self.checkout_action(
            json!({"type":"checkout_push_request", "requestId":next_request_id(), "cwd":cwd}),
            "checkout_push_response",
            "push",
        )
        .await
    }

    /// Permanently discards uncommitted changes to `paths`, including untracked files.
    pub async fn discard_changes(&self, cwd: &str, paths: Vec<String>) -> Result<()> {
        if paths.is_empty() {
            bail!("no files to discard");
        }
        self.checkout_action(
            json!({"type":"checkout.discard_changes.request", "requestId":next_request_id(), "cwd":cwd, "paths":paths}),
            "checkout.discard_changes.response",
            "discard the changes",
        )
        .await
    }

    /// Opens a pull request for the current branch and returns its URL.
    pub async fn create_pull_request(&self, cwd: &str) -> Result<Option<String>> {
        let payload = self
            .request_with_timeout(
                json!({"type":"checkout_pr_create_request", "requestId":next_request_id(), "cwd":cwd}),
                "checkout_pr_create_response",
                false,
                GIT_ACTION_TIMEOUT,
            )
            .await?;
        protocol::checkout_error(&payload, "create the pull request")?;
        Ok(payload
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// The workspaces page after `cursor`, with projects that have no workspaces on the first page.
    pub async fn workspaces_page(
        &self,
        cursor: Option<&str>,
    ) -> Result<(
        Vec<WorkspaceDescriptor>,
        Vec<ProjectDescriptor>,
        Option<String>,
    )> {
        let mut page = json!({"limit":WORKSPACE_PAGE_SIZE});
        if let Some(cursor) = cursor {
            page["cursor"] = json!(cursor);
        }
        let payload = self
            .request(
                json!({"type":"fetch_workspaces_request", "requestId":next_request_id(), "page":page}),
                "fetch_workspaces_response",
                false,
            )
            .await?;
        protocol::parse_workspace_page(&payload)
    }

    pub async fn projects(&self) -> Result<Vec<ProjectDescriptor>> {
        let payload = self
            .request(
                json!({"type":"project.list.request", "requestId":next_request_id()}),
                "project.list.response",
                false,
            )
            .await?;
        protocol::parse_projects(&payload)
    }

    /// Sets the workspace's title, or reverts to its derived name with `None`.
    pub async fn set_workspace_title(&self, workspace_id: &str, title: Option<&str>) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.title.set.request", "requestId":next_request_id(), "workspaceId":workspace_id, "title":title}),
                "workspace.title.set.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the workspace rename")
    }

    pub async fn set_workspace_pinned(&self, workspace_id: &str, pinned: bool) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.pin.set.request", "requestId":next_request_id(), "workspaceId":workspace_id, "pinned":pinned}),
                "workspace.pin.set.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the workspace pin")
    }

    /// Archives the workspace and its agents. A Paseo worktree it used is removed once no active
    /// workspace uses it, which can take as long as a git action.
    pub async fn archive_workspace(&self, workspace_id: &str) -> Result<()> {
        self.request_with_timeout(
            json!({"type":"archive_workspace_request", "requestId":next_request_id(), "workspaceId":workspace_id}),
            "archive_workspace_response",
            false,
            GIT_ACTION_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Marks the workspace's most recent finished agent as needing attention.
    pub async fn mark_workspace_unread(&self, workspace_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.mark_unread.request", "requestId":next_request_id(), "workspaceId":workspace_id}),
                "workspace.mark_unread.response",
                false,
            )
            .await?;
        require_success(&payload, "mark the workspace unread")
    }

    /// Clears attention on the workspaces' agents, except agents waiting on a permission.
    pub async fn clear_workspace_attention(&self, workspace_ids: Vec<String>) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.clear_attention.request", "requestId":next_request_id(), "workspaceId":workspace_ids}),
                "workspace.clear_attention.response",
                false,
            )
            .await?;
        require_success(&payload, "mark the workspace read")
    }

    /// Creates a workspace with no agent. Retrying with the same `idempotency_key` cannot
    /// create a second one.
    pub async fn create_workspace(
        &self,
        source: &WorkspaceSource,
        title: Option<&str>,
        idempotency_key: &str,
    ) -> Result<WorkspaceDescriptor> {
        let mut message = json!({"type":"workspace.create.request", "requestId":next_request_id(), "idempotencyKey":idempotency_key, "source":workspace_source_value(source)});
        if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
            message["title"] = json!(title);
        }
        let payload = self
            .request_with_timeout(
                message,
                "workspace.create.response",
                true,
                GIT_ACTION_TIMEOUT,
            )
            .await?;
        protocol::parse_workspace(
            payload
                .get("workspace")
                .filter(|workspace| workspace.is_object())
                .context("Paseo did not create the workspace")?,
        )
    }

    pub async fn workspace_recovery(&self, workspace_id: &str) -> Result<RecoveryState> {
        let payload = self
            .request(
                json!({"type":"workspace.recovery.inspect.request", "requestId":next_request_id(), "workspaceId":workspace_id}),
                "workspace.recovery.inspect.response",
                false,
            )
            .await?;
        protocol::parse_recovery_state(&payload)
    }

    pub async fn restore_workspace(&self, workspace_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.recovery.restore.request", "requestId":next_request_id(), "workspaceId":workspace_id}),
                "workspace.recovery.restore.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the workspace restore")
    }

    pub async fn workspace_setup_status(
        &self,
        workspace_id: &str,
    ) -> Result<Option<SetupSnapshot>> {
        let payload = self
            .request(
                json!({"type":"workspace_setup_status_request", "requestId":next_request_id(), "workspaceId":workspace_id}),
                "workspace_setup_status_response",
                false,
            )
            .await?;
        payload
            .get("snapshot")
            .filter(|snapshot| snapshot.is_object())
            .map(protocol::parse_setup_snapshot)
            .transpose()
    }

    pub async fn run_workspace_setup(&self, workspace_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"workspace.setup.run.request", "requestId":next_request_id(), "workspaceId":workspace_id}),
                "workspace.setup.run.response",
                false,
            )
            .await?;
        if payload.get("started").and_then(Value::as_bool) != Some(true) {
            bail!("Paseo did not start the workspace setup");
        }
        Ok(())
    }

    /// Starts or stops one of the workspace's scripts.
    pub async fn set_workspace_script_running(
        &self,
        workspace_id: &str,
        script_name: &str,
        running: bool,
    ) -> Result<()> {
        let (request_type, response_type) = if running {
            (
                "workspace.script.start.request",
                "workspace.script.start.response",
            )
        } else {
            (
                "workspace.script.stop.request",
                "workspace.script.stop.response",
            )
        };
        self.request(
            json!({"type":request_type, "requestId":next_request_id(), "workspaceId":workspace_id, "scriptName":script_name}),
            response_type,
            false,
        )
        .await?;
        Ok(())
    }

    /// Adds the label to the workspace, or removes it, and returns the workspace's labels.
    pub async fn set_workspace_label(
        &self,
        workspace_id: &str,
        label: &WorkspaceLabel,
        assigned: bool,
    ) -> Result<Vec<String>> {
        let payload = self
            .request(
                json!({"type":"workspace.label.assignment.set.request", "requestId":next_request_id(), "workspaceId":workspace_id, "label":{"name":label.name, "color":label.color}, "assigned":assigned}),
                "workspace.label.assignment.set.response",
                false,
            )
            .await?;
        Ok(payload
            .get("workspaceLabels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect())
    }

    pub async fn update_label(
        &self,
        name: &str,
        new_name: Option<&str>,
        color: Option<&str>,
    ) -> Result<WorkspaceLabel> {
        let mut message = json!({"type":"workspace.label.update.request", "requestId":next_request_id(), "name":name});
        if let Some(new_name) = new_name {
            message["newName"] = json!(new_name);
        }
        if let Some(color) = color {
            message["color"] = json!(color);
        }
        let payload = self
            .request(message, "workspace.label.update.response", false)
            .await?;
        protocol::parse_label(payload.get("label").context("missing label")?)
    }

    /// How many workspaces deleting the label would unlabel, without deleting it.
    pub async fn label_usage(&self, name: &str) -> Result<u64> {
        let payload = self
            .request(
                json!({"type":"workspace.label.delete.inspect.request", "requestId":next_request_id(), "name":name}),
                "workspace.label.delete.inspect.response",
                false,
            )
            .await?;
        Ok(payload
            .get("affectedWorkspaceCount")
            .and_then(Value::as_u64)
            .unwrap_or_default())
    }

    pub async fn delete_label(&self, name: &str) -> Result<()> {
        self.request(
            json!({"type":"workspace.label.delete.request", "requestId":next_request_id(), "name":name}),
            "workspace.label.delete.response",
            false,
        )
        .await?;
        Ok(())
    }

    pub async fn add_project(&self, cwd: &str) -> Result<ProjectDescriptor> {
        let payload = self
            .request(
                json!({"type":"project.add.request", "requestId":next_request_id(), "cwd":cwd}),
                "project.add.response",
                false,
            )
            .await?;
        protocol::parse_project(
            payload
                .get("project")
                .filter(|project| project.is_object())
                .context("Paseo did not add the project")?,
        )
    }

    /// Removes the project and its workspaces from Paseo. Files on disk are not changed.
    pub async fn remove_project(&self, project_id: &str) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"project.remove.request", "requestId":next_request_id(), "projectId":project_id}),
                "project.remove.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the project removal")
    }

    /// Sets the project's name, or reverts to its derived name with `None`.
    pub async fn rename_project(&self, project_id: &str, name: Option<&str>) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"project.rename.request", "requestId":next_request_id(), "projectId":project_id, "customName":name}),
                "project.rename.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the project rename")
    }

    /// The project's icon bytes and MIME type, when it has one.
    pub async fn project_icon(&self, project_id: &str) -> Result<Option<(Vec<u8>, String)>> {
        use base64::Engine as _;
        let payload = self
            .request(
                json!({"type":"project.icon.get.request", "requestId":next_request_id(), "projectId":project_id}),
                "project.icon.get.response",
                false,
            )
            .await?;
        let Some(icon) = payload.get("icon").filter(|icon| icon.is_object()) else {
            return Ok(None);
        };
        let data = icon
            .get("data")
            .and_then(Value::as_str)
            .context("missing icon data")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("invalid project icon data")?;
        let mime_type = icon
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_owned();
        Ok(Some((bytes, mime_type)))
    }

    /// Uploads an icon image for the project, or reverts to the automatic icon with `None`.
    pub async fn set_project_icon(&self, project_id: &str, image: Option<&[u8]>) -> Result<()> {
        use base64::Engine as _;
        let source = match image {
            Some(bytes) => {
                json!({"type":"upload", "data":base64::engine::general_purpose::STANDARD.encode(bytes)})
            }
            None => json!({"type":"automatic"}),
        };
        let payload = self
            .request(
                json!({"type":"project.icon.set.request", "requestId":next_request_id(), "projectId":project_id, "source":source}),
                "project.icon.set.response",
                false,
            )
            .await?;
        require_accepted(&payload, "the project icon")
    }

    /// Creates `name` inside `parent_path` on the host and adds it as a project.
    pub async fn create_project_directory(
        &self,
        parent_path: &str,
        name: &str,
    ) -> Result<ProjectDescriptor> {
        let payload = self
            .request(
                json!({"type":"project.create_directory.request", "requestId":next_request_id(), "parentPath":parent_path, "name":name}),
                "project.create_directory.response",
                false,
            )
            .await?;
        protocol::parse_project(
            payload
                .get("project")
                .filter(|project| project.is_object())
                .context("Paseo did not create the directory")?,
        )
    }

    pub async fn daemon_status(&self) -> Result<DaemonStatus> {
        let payload = self
            .request(
                json!({"type":"daemon.get_status.request", "requestId":next_request_id()}),
                "daemon.get_status.response",
                false,
            )
            .await?;
        protocol::parse_daemon_status(&payload)
    }

    pub async fn available_providers(&self) -> Result<Vec<ProviderAvailability>> {
        let payload = self
            .request(
                json!({"type":"list_available_providers_request", "requestId":next_request_id()}),
                "list_available_providers_response",
                false,
            )
            .await?;
        protocol::parse_provider_availability(&payload)
    }

    /// Asks the daemon to recheck its providers; the new list arrives as `ProvidersChanged`.
    pub async fn refresh_providers(&self) -> Result<()> {
        self.request_with_timeout(
            json!({"type":"refresh_providers_snapshot_request", "requestId":next_request_id()}),
            "refresh_providers_snapshot_response",
            false,
            PROVIDER_REFRESH_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Runs the provider's health check on the host and returns its report.
    pub async fn provider_diagnostic(&self, provider: &str) -> Result<String> {
        let payload = self
            .request_with_timeout(
                json!({"type":"provider_diagnostic_request", "requestId":next_request_id(), "provider":provider}),
                "provider_diagnostic_response",
                false,
                PROVIDER_DIAGNOSTIC_TIMEOUT,
            )
            .await?;
        Ok(payload
            .get("diagnostic")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    /// Restarts the daemon process. Agents keep running, and the session reconnects.
    pub async fn restart_daemon(&self) -> Result<()> {
        let payload = self
            .request(
                json!({"type":"restart_server_request", "requestId":next_request_id(), "reason":"Restarted from Zaseo"}),
                "status",
                false,
            )
            .await?;
        if payload.get("status").and_then(Value::as_str) != Some("restart_requested") {
            bail!("Paseo did not accept the restart");
        }
        Ok(())
    }

    /// Updates the daemon to the latest version and restarts it. Progress arrives as
    /// `DaemonUpdateProgress` events.
    pub async fn update_daemon(&self) -> Result<DaemonUpdate> {
        let payload = self
            .request_with_timeout(
                json!({"type":"daemon.update.request", "requestId":next_request_id()}),
                "daemon.update.response",
                false,
                DAEMON_UPDATE_TIMEOUT,
            )
            .await?;
        protocol::parse_daemon_update(&payload)
    }

    /// The Paseo-created worktrees of the repository at `repo_root`.
    pub async fn paseo_worktrees(&self, repo_root: &str) -> Result<Vec<PaseoWorktree>> {
        let payload = self
            .request(
                json!({"type":"paseo_worktree_list_request", "requestId":next_request_id(), "repoRoot":repo_root}),
                "paseo_worktree_list_response",
                false,
            )
            .await?;
        protocol::parse_paseo_worktrees(&payload)
    }

    /// Archives a Paseo worktree: archives its agents and workspace, then removes the worktree
    /// from disk once no active workspace uses it. The branch is kept. Returns the archived
    /// agents.
    pub async fn archive_paseo_worktree(&self, worktree_path: &str) -> Result<Vec<String>> {
        let payload = self
            .request_with_timeout(
                json!({"type":"paseo_worktree_archive_request", "requestId":next_request_id(), "worktreePath":worktree_path, "scope":"worktree"}),
                "paseo_worktree_archive_response",
                false,
                GIT_ACTION_TIMEOUT,
            )
            .await?;
        if let Some(error) = protocol::error_text(payload.get("error")) {
            bail!("Paseo could not archive the worktree: {error}");
        }
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("Paseo could not archive the worktree");
        }
        Ok(payload
            .get("removedAgents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|agent| agent.as_str().map(str::to_owned))
            .collect())
    }

    async fn checkout_action(
        &self,
        message: Value,
        response_type: &'static str,
        action: &str,
    ) -> Result<()> {
        let payload = self
            .request_with_timeout(message, response_type, false, GIT_ACTION_TIMEOUT)
            .await?;
        protocol::checkout_error(&payload, action)?;
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("Paseo could not {action}");
        }
        Ok(())
    }

    /// Uploads a file to the daemon's host: a request, then the bytes as binary frames on the same
    /// connection, answered once the daemon has stored the file.
    pub async fn upload_file(&self, upload: FileUpload) -> Result<UploadedFile> {
        let request_id = next_request_id();
        let size = upload.bytes.len();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Request {
                message: json!({"type":"file.upload.request", "requestId":request_id, "fileName":upload.file_name, "mimeType":upload.mime_type, "size":size, "modifiedAt":upload.modified_at}),
                response_type: "file.upload.response",
                retry_creation: false,
                reply,
            })
            .await
            .context("Paseo connection closed")?;
        let metadata = json!({"mime":upload.mime_type, "size":size, "encoding":"binary", "modifiedAt":upload.modified_at, "fileName":upload.file_name}).to_string();
        let metadata_length =
            u16::try_from(metadata.len()).context("file upload metadata is too long")?;
        let mut begin = file_frame_header(FILE_BEGIN, &request_id)?;
        begin.extend_from_slice(&metadata_length.to_be_bytes());
        begin.extend_from_slice(metadata.as_bytes());
        self.send_binary(begin).await?;
        for chunk in upload.bytes.chunks(FILE_CHUNK_SIZE) {
            let mut frame = file_frame_header(FILE_CHUNK, &request_id)?;
            frame.extend_from_slice(chunk);
            self.send_binary(frame).await?;
        }
        self.send_binary(file_frame_header(FILE_END, &request_id)?)
            .await?;
        let payload = tokio::time::timeout(FILE_UPLOAD_TIMEOUT, response)
            .await
            .map_err(|_| anyhow!("Paseo file upload timed out"))?
            .context("Paseo connection closed")??;
        protocol::parse_uploaded_file(&payload)
    }

    async fn send_binary(&self, frame: Vec<u8>) -> Result<()> {
        self.commands
            .send(Command::Binary(frame))
            .await
            .context("Paseo connection closed")
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

fn emit_subagent_update(payload: &Value, events: &Sender<PaseoEvent>) {
    let event = match payload["kind"].as_str() {
        Some("upsert") => match protocol::parse_subagent(&payload["subagent"]) {
            Ok(subagent) => PaseoEvent::SubagentUpserted(subagent),
            Err(error) => {
                log::warn!("Paseo sent an unreadable subagent: {error:#}");
                return;
            }
        },
        Some("remove") => {
            let (Some(parent_agent_id), Some(subagent_id)) = (
                payload["parentAgentId"].as_str(),
                payload["subagentId"].as_str(),
            ) else {
                return;
            };
            PaseoEvent::SubagentRemoved {
                parent_agent_id: parent_agent_id.to_owned(),
                subagent_id: subagent_id.to_owned(),
            }
        }
        Some("timeline") => {
            let (
                Some(parent_agent_id),
                Some(subagent_id),
                Some(epoch),
                Some(sequence),
                Some(timestamp),
                Some(item),
            ) = (
                payload["parentAgentId"].as_str(),
                payload["subagentId"].as_str(),
                payload["epoch"].as_str(),
                payload["seq"].as_u64(),
                payload["timestamp"].as_str(),
                payload["item"].as_object(),
            )
            else {
                return;
            };
            PaseoEvent::TimelineEntry(TimelineEntry {
                agent_id: subagent_timeline_id(parent_agent_id, subagent_id),
                epoch: epoch.to_owned(),
                sequence,
                timestamp: timestamp.to_owned(),
                payload: protocol::timeline_payload(Value::Object(item.clone())),
                extra: protocol::entry_extra(payload, "item"),
            })
        }
        _ => return,
    };
    emit_event(events, event);
}

fn emit_workspace_update(payload: &Value, events: &Sender<PaseoEvent>) {
    let event = match payload["kind"].as_str() {
        Some("upsert") => match protocol::parse_workspace(&payload["workspace"]) {
            Ok(workspace) => PaseoEvent::WorkspaceUpserted(workspace),
            Err(error) => {
                log::warn!("invalid Paseo workspace update: {error:#}");
                return;
            }
        },
        Some("remove") => {
            let Some(workspace_id) = payload["id"].as_str() else {
                return;
            };
            PaseoEvent::WorkspaceRemoved {
                workspace_id: workspace_id.to_owned(),
                removed_project_id: payload["removedProjectId"].as_str().map(str::to_owned),
            }
        }
        _ => return,
    };
    emit_event(events, event);
}

fn emit_project_update(payload: &Value, events: &Sender<PaseoEvent>) {
    let event = match payload["kind"].as_str() {
        Some("upsert") => match protocol::parse_project(&payload["project"]) {
            Ok(project) => PaseoEvent::ProjectUpserted(project),
            Err(error) => {
                log::warn!("invalid Paseo project update: {error:#}");
                return;
            }
        },
        Some("remove") => {
            let Some(project_id) = payload["projectId"].as_str() else {
                return;
            };
            PaseoEvent::ProjectRemoved {
                project_id: project_id.to_owned(),
            }
        }
        _ => return,
    };
    emit_event(events, event);
}

fn emit_label_update(payload: &Value, events: &Sender<PaseoEvent>) {
    let event = match payload["kind"].as_str() {
        Some("upsert") => match protocol::parse_label(&payload["label"]) {
            Ok(label) => PaseoEvent::LabelUpserted {
                label,
                previous_name: payload["previousName"].as_str().map(str::to_owned),
            },
            Err(error) => {
                log::warn!("invalid Paseo label update: {error:#}");
                return;
            }
        },
        Some("remove") => {
            let Some(name) = payload["name"].as_str() else {
                return;
            };
            PaseoEvent::LabelRemoved {
                name: name.to_owned(),
            }
        }
        _ => return,
    };
    emit_event(events, event);
}

fn emit_event(events: &Sender<PaseoEvent>, event: PaseoEvent) {
    if events.try_send(event).is_err() {
        log::debug!("Paseo event receiver closed");
    }
}

/// Terminal frames are `[opcode][slot][payload]`; only output and screen restores carry bytes
/// for the screen.
fn emit_terminal_frame(data: &[u8], slots: &HashMap<u8, String>, events: &Sender<PaseoEvent>) {
    const OUTPUT: u8 = 0x01;
    const RESTORE: u8 = 0x05;
    let [opcode, slot, bytes @ ..] = data else {
        return;
    };
    let restore = match *opcode {
        OUTPUT => false,
        RESTORE => true,
        _ => return,
    };
    if let Some(terminal_id) = slots.get(slot) {
        emit_event(
            events,
            PaseoEvent::TerminalOutput {
                terminal_id: terminal_id.clone(),
                bytes: bytes.to_vec(),
                restore,
            },
        );
    }
}

fn image_payloads(images: Vec<ImageAttachment>) -> Value {
    images
        .into_iter()
        .map(|image| json!({"data":image.data_base64, "mimeType":image.mime_type}))
        .collect()
}

/// A `workspace.create` source. Unset fields are left out: the daemon accepts a missing field
/// but rejects `null`.
fn workspace_source_value(source: &WorkspaceSource) -> Value {
    let (mut value, project_id, base_ref) = match source {
        WorkspaceSource::Directory { path, project_id } => {
            (json!({"kind":"directory", "path":path}), project_id, &None)
        }
        WorkspaceSource::Worktree {
            cwd,
            project_id,
            base_ref,
        } => (
            json!({"kind":"worktree", "cwd":cwd, "action":"branch-off"}),
            project_id,
            base_ref,
        ),
    };
    if let Some(project_id) = project_id {
        value["projectId"] = json!(project_id);
    }
    if let Some(base_ref) = base_ref.as_deref().filter(|base| !base.is_empty()) {
        value["refName"] = json!(base_ref);
    }
    value
}

fn require_success(payload: &Value, action: &str) -> Result<()> {
    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        bail!(
            "Paseo could not {action}: {}",
            protocol::error_text(payload.get("error")).unwrap_or_else(|| "unknown reason".into())
        );
    }
    Ok(())
}

fn require_accepted(payload: &Value, action: &str) -> Result<()> {
    if payload.get("accepted").and_then(Value::as_bool) != Some(true) {
        bail!(
            "Paseo did not accept {action}: {}",
            payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason")
        );
    }
    Ok(())
}

/// A file transfer frame's opening bytes: the opcode, then the request ID with its length.
fn file_frame_header(opcode: u8, request_id: &str) -> Result<Vec<u8>> {
    let length = u8::try_from(request_id.len()).context("file upload request ID is too long")?;
    let mut frame = vec![opcode, length];
    frame.extend_from_slice(request_id.as_bytes());
    Ok(frame)
}

fn next_request_id() -> String {
    format!("zaseo-{}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
}

/// The `draftConfig` Paseo takes for listing a draft's commands and features.
fn draft_config_json(draft: DraftConfig) -> Result<Value> {
    let cwd = draft.cwd.to_str().context("draft directory is not UTF-8")?;
    let mut config = json!({"provider":draft.provider, "cwd":cwd});
    if let Some(mode_id) = draft.mode_id {
        config["modeId"] = json!(mode_id);
    }
    if let Some(model) = draft.model {
        config["model"] = json!(model);
    }
    if let Some(thinking_option_id) = draft.thinking_option_id {
        config["thinkingOptionId"] = json!(thinking_option_id);
    }
    if !draft.feature_values.is_empty() {
        config["featureValues"] = json!(draft.feature_values);
    }
    Ok(config)
}

/// Agents per history page, as Paseo's History screen asks for.
const AGENT_HISTORY_PAGE_LIMIT: usize = 200;

/// A page of agent history: its agents, the cursor of the next page if there is one, and whether
/// an older daemon stopped matching a search early.
#[derive(Clone, Debug)]
pub struct AgentHistoryPage {
    pub agents: Vec<AgentSummary>,
    pub next_cursor: Option<String>,
    pub search_truncated: bool,
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
    let credentials = Credentials {
        password,
        paseo_home: paseo_home(),
    };
    connect_with_ssh_executable(target, credentials, client_id, PathBuf::from("ssh")).await
}

#[derive(Default)]
struct Credentials {
    password: Option<RuntimePassword>,
    /// Where a daemon on this machine keeps `paseo.pid` and `local-credential`.
    paseo_home: Option<PathBuf>,
}

enum HelloAuth<'a> {
    Password(&'a str),
    LocalCredential(String),
}

impl Credentials {
    /// A typed password wins, as in the Paseo CLI, so a stale `paseo.pid` left by a crashed daemon
    /// cannot lock out a user who knows the password. The local credential is read on every
    /// attempt because the daemon writes a new one when it restarts.
    fn hello_auth(&self, target: &ConnectionTarget) -> Option<HelloAuth<'_>> {
        match &self.password {
            Some(password) => Some(HelloAuth::Password(password.as_str())),
            None => self
                .paseo_home
                .as_deref()
                .and_then(|home| local_credential(target, home))
                .map(HelloAuth::LocalCredential),
        }
    }
}

fn paseo_home() -> Option<PathBuf> {
    match std::env::var_os("PASEO_HOME") {
        Some(configured) => {
            let configured = PathBuf::from(configured);
            match configured.strip_prefix("~") {
                Ok(relative) => std::env::home_dir().map(|home| home.join(relative)),
                Err(_) => Some(configured),
            }
        }
        None => std::env::home_dir().map(|home| home.join(".paseo")),
    }
}

/// The token that lets a client on the daemon's own machine connect without its password. Paseo
/// only offers it when the target is the daemon recorded in `paseo.pid`.
fn local_credential(target: &ConnectionTarget, paseo_home: &Path) -> Option<String> {
    let ConnectionTarget::Direct { websocket_url, .. } = target else {
        return None;
    };
    let url = url::Url::parse(websocket_url).ok()?;
    let target_endpoint = local_endpoint(url.host_str()?, url.port_or_known_default()?);
    let lock: Value =
        serde_json::from_slice(&std::fs::read(paseo_home.join("paseo.pid")).ok()?).ok()?;
    let listen = url::Url::parse(&format!("tcp://{}", lock["listen"].as_str()?)).ok()?;
    if local_endpoint(listen.host_str()?, listen.port()?) != target_endpoint {
        return None;
    }
    let token = std::fs::read_to_string(paseo_home.join("local-credential")).ok()?;
    let token = token.trim();
    let valid = token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    valid.then(|| token.to_owned())
}

fn local_endpoint(host: &str, port: u16) -> String {
    let host = match host {
        "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]" => "localhost",
        host => host,
    };
    format!("{host}:{port}")
}

/// Only RFC 7230 token characters fit in the legacy `paseo.bearer.<password>` subprotocol.
fn header_safe_password(password: &str) -> bool {
    !password.is_empty()
        && password
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

async fn connect_with_ssh_executable(
    target: ConnectionTarget,
    credentials: Credentials,
    client_id: String,
    ssh_executable: PathBuf,
) -> Result<(PaseoSession, Receiver<PaseoEvent>)> {
    if client_id.trim().is_empty() {
        bail!("client ID must not be empty");
    }
    let (mut socket, server_info) =
        open_socket(&target, &credentials, &client_id, &ssh_executable).await?;
    subscribe(&mut socket, &TimelineSubscriptions::default())
        .await
        .context("Paseo initial subscription failed")?;
    let (commands, command_receiver) = mpsc::channel(64);
    let (events, event_receiver) = async_channel::unbounded();
    events
        .send(PaseoEvent::ServerInfo(server_info))
        .await
        .context("event receiver closed")?;
    events
        .send(PaseoEvent::Connected)
        .await
        .context("event receiver closed")?;
    tokio::spawn(run(
        socket,
        target,
        credentials,
        client_id,
        ssh_executable,
        command_receiver,
        events,
    ));
    Ok((PaseoSession { commands }, event_receiver))
}

async fn open_socket(
    target: &ConnectionTarget,
    credentials: &Credentials,
    client_id: &str,
    ssh_executable: &Path,
) -> Result<(Socket, ServerInfo)> {
    let websocket_url = transport::websocket_url(target)?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .context("invalid Paseo WebSocket request")?;
    let auth = credentials.hello_auth(target);
    // COMPAT(headerAuth): daemons before v0.10 only read the password from the subprotocol.
    let bearer = match &auth {
        Some(HelloAuth::Password(password)) if header_safe_password(password) => {
            Some(format!("paseo.bearer.{password}"))
        }
        _ => None,
    };
    if let Some(bearer) = &bearer {
        let protocol = HeaderValue::from_str(bearer)
            .map_err(|_| anyhow!("password cannot be used in WebSocket subprotocol"))?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocol);
    }
    let (mut socket, response) = transport::connect_socket(target, request, ssh_executable).await?;
    if let Some(bearer) = &bearer
        && response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            != Some(bearer.as_str())
    {
        bail!("Paseo daemon did not accept password authentication");
    }
    let mut hello = json!({"type":"hello", "clientId":client_id, "clientType":"cli", "protocolVersion":1, "capabilities":{"hello_rejection":true, "owned_subscriptions":true, "explicit_event_subscriptions":true, "selective_agent_timeline":true, "all_providers":true, "timeline_replacement_invalidation":true, "timeline_notifications":true, "reasoning_merge_enum":true, "terminal-restore-modes":true, "provider_subagents":true, "projected_subagent_timeline":true, "project_updates":true}});
    match auth {
        Some(HelloAuth::Password(password)) => {
            hello["auth"] = json!({"kind":"password", "password":password});
        }
        Some(HelloAuth::LocalCredential(token)) => {
            hello["auth"] = json!({"kind":"localCredential", "token":token});
        }
        None => {}
    }
    socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .context("Paseo hello failed")?;
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .context("Paseo hello timed out")?
        .context("Paseo closed during hello")?
        .context("Paseo hello failed")?;
    if let Message::Close(frame) = &message {
        // A daemon sends no `hello.rejected` when it rejects the bearer subprotocol or predates v0.10.
        return Err(match frame.as_ref().map(|frame| frame.reason.as_str()) {
            Some("Password required") => AuthRejection::PasswordRequired.into(),
            Some("Incorrect password") => AuthRejection::IncorrectPassword.into(),
            Some("Incompatible protocol version") => anyhow!("incompatible Paseo protocol version"),
            _ => anyhow!("Paseo closed during hello"),
        });
    }
    let value: Value =
        serde_json::from_slice(&message.into_data()).context("invalid Paseo hello response")?;
    if value["type"] == "hello.rejected" {
        return Err(match value["reason"].as_str() {
            Some("password_required") => AuthRejection::PasswordRequired.into(),
            Some("incorrect_password") => AuthRejection::IncorrectPassword.into(),
            _ => anyhow!("incompatible Paseo protocol version"),
        });
    }
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
    let server_info = ServerInfo {
        server_id: info["payload"]["serverId"].as_str().map(str::to_owned),
        features: features.clone(),
        capabilities: info["payload"]["capabilities"].clone(),
        desktop_managed: info["payload"]["desktopManaged"] == true,
    };
    Ok((socket, server_info))
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

/// Agents whose live timeline is streamed, with the last timeline position seen for each so a
/// reconnect can fetch only what was missed. Cursor keys are always subscribed agents.
#[derive(Default)]
struct TimelineSubscriptions {
    agent_ids: Vec<String>,
    cursors: HashMap<String, (String, u64)>,
}

impl TimelineSubscriptions {
    fn replace(&mut self, agent_ids: Vec<String>) {
        let mut seen = HashSet::new();
        self.agent_ids = agent_ids
            .into_iter()
            .filter(|agent_id| seen.insert(agent_id.clone()))
            .collect();
        self.cursors.retain(|agent_id, _| seen.contains(agent_id));
    }

    fn contains(&self, agent_id: &str) -> bool {
        self.agent_ids
            .iter()
            .any(|subscribed| subscribed == agent_id)
    }

    fn advance(&mut self, agent_id: &str, epoch: &str, sequence: u64) {
        if self.contains(agent_id) {
            self.cursors
                .insert(agent_id.to_owned(), (epoch.to_owned(), sequence));
        }
    }
}

fn timeline_tail_request(agent_id: &str) -> Value {
    json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":agent_id, "direction":"tail", "limit":TIMELINE_PAGE_SIZE, "projection":"projected"})
}

fn timeline_after_request(agent_id: &str, epoch: &str, sequence: u64) -> Value {
    json!({"type":"fetch_agent_timeline_request", "requestId":next_request_id(), "agentId":agent_id, "direction":"after", "cursor":{"epoch":epoch, "seq":sequence}, "limit":TIMELINE_PAGE_SIZE, "projection":"projected"})
}

async fn subscribe(socket: &mut Socket, timeline: &TimelineSubscriptions) -> Result<()> {
    send_message(socket, json!({"type":"session.events.set_subscription.request", "requestId":next_request_id(), "events":["agent_permission_request", "agent_permission_resolved", "providers_snapshot_update", "agent.provider_subagents.update", "project.update", "script_status_update", "workspace_setup_progress"]})).await?;
    send_message(socket, json!({"type":"fetch_agents_request", "requestId":next_request_id(), "scope":"active", "subscribe":{}})).await?;
    send_message(socket, json!({"type":"fetch_workspaces_request", "requestId":next_request_id(), "page":{"limit":WORKSPACE_PAGE_SIZE}, "subscribe":{}})).await?;
    send_message(socket, json!({"type":"workspace.label.list.request", "requestId":next_request_id(), "subscribe":{}})).await?;
    if timeline.agent_ids.is_empty() {
        return Ok(());
    }
    send_message(socket, json!({"type":"agent.timeline.set_subscription.request", "requestId":next_request_id(), "agentIds":timeline.agent_ids})).await?;
    for agent_id in &timeline.agent_ids {
        let history = match timeline.cursors.get(agent_id) {
            Some((epoch, sequence)) => timeline_after_request(agent_id, epoch, *sequence),
            None => timeline_tail_request(agent_id),
        };
        send_message(socket, history).await?;
    }
    Ok(())
}

/// The daemon socket, with everything a reconnect needs to replace it.
struct Connection {
    socket: Socket,
    target: ConnectionTarget,
    credentials: Credentials,
    client_id: String,
    ssh_executable: PathBuf,
    commands: mpsc::Receiver<Command>,
    events: Sender<PaseoEvent>,
    pending: HashMap<String, Pending>,
    timeline: TimelineSubscriptions,
    ping_pending: bool,
}

impl Connection {
    /// Sends a session message, reconnecting when the socket fails. Returns `false` once the
    /// session has ended.
    async fn send_or_reconnect(&mut self, message: Value) -> bool {
        match send_message(&mut self.socket, message).await {
            Ok(()) => true,
            Err(error) => self.reconnect_after(&error).await,
        }
    }

    async fn send_frame_or_reconnect(&mut self, frame: Message) -> bool {
        match self.socket.send(frame).await {
            Ok(()) => true,
            Err(error) => self.reconnect_after(&error.into()).await,
        }
    }

    async fn reconnect_after(&mut self, error: &anyhow::Error) -> bool {
        // Socket errors can carry request details, so only their kind is logged.
        match error.downcast_ref::<transport::WebSocketError>() {
            Some(socket_error) => log::warn!(
                "Paseo connection lost ({}); reconnecting",
                transport::websocket_error_kind(socket_error)
            ),
            None => log::warn!("{error}; reconnecting"),
        }
        self.reconnect().await
    }

    /// Opens a replacement socket, waiting longer after each failed attempt. Returns `false` when
    /// the session ends instead.
    async fn reconnect(&mut self) -> bool {
        emit_event(
            &self.events,
            PaseoEvent::Disconnected {
                reason: "Paseo connection lost; reconnecting".into(),
            },
        );
        let mut retry = HashMap::new();
        for (request_id, request) in self.pending.drain() {
            if request.reply.is_closed() {
                continue;
            }
            if request.replay.is_some() {
                retry.insert(request_id, request);
            } else {
                let message = if request.sends_message {
                    "Paseo connection lost; message outcome unknown"
                } else {
                    "Paseo connection lost; request outcome unknown"
                };
                deliver(request.reply, Err(anyhow!(message)));
            }
        }
        self.pending = retry;
        let mut failed_attempts = 0;
        loop {
            if self.events.is_closed() {
                return false;
            }
            tokio::select! {
                _ = tokio::time::sleep(reconnect_delay(failed_attempts)) => {},
                command = self.commands.recv() => match command {
                    Some(Command::Request { reply, .. }) => {
                        deliver(reply, Err(anyhow!("Paseo is reconnecting")));
                        continue;
                    }
                    Some(Command::Notify(_) | Command::ReleaseTerminal { .. } | Command::Binary(_)) => continue,
                    Some(Command::Close(reply)) => {
                        deliver(reply, Ok(()));
                        return false;
                    }
                    None => return false,
                }
            }
            failed_attempts = failed_attempts.saturating_add(1);
            let (mut replacement, server_info) = match open_socket(
                &self.target,
                &self.credentials,
                &self.client_id,
                &self.ssh_executable,
            )
            .await
            {
                Ok(opened) => opened,
                Err(error) => {
                    if let Some(rejection) = error.downcast_ref::<AuthRejection>() {
                        // Retrying the same credentials cannot succeed, so the user has to enter
                        // new ones.
                        for (_, request) in self.pending.drain() {
                            deliver(request.reply, Err(anyhow!("{rejection}")));
                        }
                        emit_event(
                            &self.events,
                            PaseoEvent::ConnectionFailed {
                                reason: rejection.to_string(),
                            },
                        );
                        return false;
                    }
                    log::debug!("Paseo reconnect attempt failed: {error}");
                    continue;
                }
            };
            if let Err(error) = subscribe(&mut replacement, &self.timeline).await {
                log::debug!("Paseo reconnect could not subscribe: {error}");
                continue;
            }
            let mut replay_failed = false;
            for message in self
                .pending
                .values()
                .filter_map(|request| request.replay.as_ref())
            {
                if let Err(error) = send_message(&mut replacement, message.clone()).await {
                    log::debug!("Paseo reconnect could not replay a request: {error}");
                    replay_failed = true;
                    break;
                }
            }
            if replay_failed {
                continue;
            }
            self.socket = replacement;
            self.ping_pending = false;
            emit_event(&self.events, PaseoEvent::ServerInfo(server_info));
            emit_event(&self.events, PaseoEvent::Connected);
            return true;
        }
    }
}

const RECONNECT_DELAY_LIMIT: Duration = Duration::from_secs(30);

/// One second before the first attempt, doubling after each failure up to the limit, so a host
/// that stays down is not retried every second; an SSH host starts a new `ssh` per attempt.
fn reconnect_delay(failed_attempts: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failed_attempts)).min(RECONNECT_DELAY_LIMIT)
}

async fn run(
    socket: Socket,
    target: ConnectionTarget,
    credentials: Credentials,
    client_id: String,
    ssh_executable: PathBuf,
    commands: mpsc::Receiver<Command>,
    events: Sender<PaseoEvent>,
) {
    let mut connection = Connection {
        socket,
        target,
        credentials,
        client_id,
        ssh_executable,
        commands,
        events,
        pending: HashMap::new(),
        timeline: TimelineSubscriptions::default(),
        ping_pending: false,
    };
    let mut agents: HashMap<String, AgentSummary> = HashMap::new();
    let mut permissions: HashMap<String, String> = HashMap::new();
    let mut subscriptions: HashMap<&'static str, String> = HashMap::new();
    let mut terminal_slots: HashMap<u8, String> = HashMap::new();
    // The directory page this loop asked for, whose failure must still deliver the directory.
    let mut directory_page_request: Option<String> = None;
    let mut ping_timer =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    loop {
        tokio::select! {
            _ = ping_timer.tick() => {
                connection.pending.retain(|_, request| !request.reply.is_closed());
                let ping = if connection.ping_pending {
                    Err(anyhow!("Paseo daemon did not answer a ping"))
                } else {
                    connection
                        .socket
                        .send(Message::Text(json!({"type":"ping"}).to_string().into()))
                        .await
                        .map_err(anyhow::Error::from)
                };
                match ping {
                    Ok(()) => connection.ping_pending = true,
                    Err(error) => {
                        if !connection.reconnect_after(&error).await { break; }
                    }
                }
            },
            command = connection.commands.recv() => match command {
                Some(Command::Request { message, response_type, retry_creation, reply }) => {
                    let Some(request_id) = message.get("requestId").and_then(Value::as_str).map(str::to_owned) else {
                        deliver(reply, Err(anyhow!("request ID missing")));
                        continue;
                    };
                    if message["type"] == "agent.timeline.set_subscription.request" {
                        connection.timeline.replace(
                            message["agentIds"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                        );
                    }
                    if message["type"] == "fetch_agent_timeline_request"
                        && message["direction"] == "tail"
                        && let Some(agent_id) = message["agentId"].as_str()
                    {
                        connection.timeline.cursors.remove(agent_id);
                    }
                    let request = Pending {
                        replay: retry_creation.then(|| message.clone()),
                        sends_message: message["type"] == "send_agent_message_request",
                        response_type,
                        reply,
                    };
                    if message["type"] == "agent_permission_response" {
                        if let Some(agent_id) = permissions.get(&request_id) {
                            let mut message = message;
                            message["agentId"] = json!(agent_id);
                            match send_message(&mut connection.socket, message).await {
                                Ok(()) => {
                                    connection.pending.insert(request_id, request);
                                }
                                Err(error) => {
                                    deliver(request.reply, Err(anyhow!("Paseo connection lost; permission outcome unknown")));
                                    if !connection.reconnect_after(&error).await { break; }
                                }
                            }
                            continue;
                        }
                        deliver(request.reply, Err(anyhow!("permission request is no longer pending")));
                        continue;
                    }
                    match send_message(&mut connection.socket, message).await {
                        Ok(()) => {
                            connection.pending.insert(request_id, request);
                        }
                        Err(error) => {
                            if request.replay.is_some() {
                                connection.pending.insert(request_id, request);
                            } else {
                                deliver(request.reply, Err(anyhow!("Paseo connection lost; request outcome unknown")));
                            }
                            if !connection.reconnect_after(&error).await { break; }
                        }
                    }
                }
                Some(Command::ReleaseTerminal { terminal_id, subscription_id }) => {
                    terminal_slots.retain(|_, subscribed| *subscribed != terminal_id);
                    if !connection.send_or_reconnect(json!({"type":"subscription.release.request", "requestId":next_request_id(), "subscriptionId":subscription_id})).await { break; }
                }
                Some(Command::Binary(frame)) => {
                    if !connection.send_frame_or_reconnect(Message::Binary(frame.into())).await { break; }
                }
                Some(Command::Notify(message)) => {
                    if message["type"] == "unsubscribe_terminal_request" {
                        terminal_slots.retain(|_, terminal_id| message["terminalId"] != terminal_id.as_str());
                    }
                    if !connection.send_or_reconnect(message).await { break; }
                }
                Some(Command::Close(reply)) => {
                    let result = connection.socket.close(None).await.context("Paseo close failed");
                    deliver(reply, result);
                    break;
                }
                None => break,
            },
            frame = connection.socket.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let value = match serde_json::from_str::<Value>(&text) {
                        Ok(value) => value,
                        Err(error) => {
                            log::warn!("Paseo sent an unreadable message: {error}");
                            continue;
                        }
                    };
                    if value["type"] == "session" {
                        let message = &value["message"];
                        let timeline_agent = message["payload"]["agentId"].as_str().filter(|agent_id| connection.timeline.contains(agent_id)).map(str::to_owned);
                        let timeline_response = message["type"] == "fetch_agent_timeline_response";
                        let refetch_tail = message["type"] == "agent.timeline.replacement" || (timeline_response && (message["payload"]["staleCursor"] == true || message["payload"]["reset"] == true));
                        let directory_page = message["type"] == "fetch_agents_response"
                            && message["payload"]["requestId"].as_str().is_some_and(|request_id| !connection.pending.contains_key(request_id));
                        let next_directory_cursor = if directory_page { next_agents_cursor(&message["payload"]).ok().flatten() } else { None };
                        let directory_page_failed = message["type"] == "rpc_error"
                            && directory_page_request.as_deref().is_some_and(|request_id| message["payload"]["requestId"] == request_id);
                        let previous_cursor = timeline_agent.as_ref().and_then(|agent_id| connection.timeline.cursors.get(agent_id).cloned());
                        let old_subscription = update_subscription(message, &mut subscriptions);
                        if message["type"] == "subscribe_terminal_response"
                            && let (Some(slot), Some(terminal_id)) = (
                                message["payload"]["slot"].as_u64().and_then(|slot| u8::try_from(slot).ok()),
                                message["payload"]["terminalId"].as_str(),
                            )
                        {
                            terminal_slots.insert(slot, terminal_id.to_owned());
                        }
                        handle_message(message, &mut connection.pending, &mut agents, &mut permissions, &connection.events, &mut connection.timeline);
                        if directory_page_failed {
                            directory_page_request = None;
                            log::warn!(
                                "Paseo could not load the rest of the agent directory: {}",
                                message["payload"]["error"].as_str().unwrap_or("unknown error")
                            );
                            emit_event(&connection.events, PaseoEvent::AgentsChanged(agents.values().cloned().collect()));
                        }
                        if let Some(page_cursor) = next_directory_cursor {
                            let request_id = next_request_id();
                            directory_page_request = Some(request_id.clone());
                            if !connection.send_or_reconnect(json!({"type":"fetch_agents_request", "requestId":request_id, "scope":"active", "page":{"limit":200,"cursor":page_cursor}})).await { break; }
                        }
                        if let Some(subscription_id) = old_subscription
                            && !connection.send_or_reconnect(json!({"type":"subscription.release.request", "requestId":next_request_id(), "subscriptionId":subscription_id})).await
                        {
                            break;
                        }
                        let follow_up = timeline_agent.and_then(|agent_id| {
                            if refetch_tail {
                                connection.timeline.cursors.remove(&agent_id);
                                Some(timeline_tail_request(&agent_id))
                            } else if timeline_response
                                && message["payload"]["direction"] == "after"
                                && message["payload"]["hasNewer"] == true
                            {
                                connection.timeline.cursors.get(&agent_id)
                                    .filter(|cursor| previous_cursor.as_ref() != Some(*cursor))
                                    .map(|(epoch, sequence)| timeline_after_request(&agent_id, epoch, *sequence))
                            } else {
                                None
                            }
                        });
                        if let Some(request) = follow_up
                            && !connection.send_or_reconnect(request).await
                        {
                            break;
                        }
                    } else if value["type"] == "pong" {
                        connection.ping_pending = false;
                    } else if value["type"] == "ping"
                        && !connection.send_frame_or_reconnect(Message::Text(json!({"type":"pong"}).to_string().into())).await
                    {
                        break;
                    }
                }
                Some(Ok(Message::Binary(data))) => emit_terminal_frame(&data, &terminal_slots, &connection.events),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    if !connection.reconnect_after(&error.into()).await { break; }
                }
                None => {
                    if !connection.reconnect_after(&anyhow!("Paseo closed the connection")).await { break; }
                }
            }
        }
    }
    connection.pending.into_values().for_each(|pending| {
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
        Some("fetch_workspaces_response") => "workspaces",
        Some("workspace.label.list.response") => "labels",
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
    timeline: &mut TimelineSubscriptions,
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
                    // The caller applies the page it asked for, which lets the store reject a
                    // stale one, so its entries are not sent as events as well.
                    if message_type == "fetch_agent_timeline_response" {
                        advance_timeline_cursor(payload, timeline);
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
            if let (Some(agent_id), Some(epoch)) =
                (payload["agentId"].as_str(), payload["epoch"].as_str())
                && timeline.contains(agent_id)
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
        "agent_update" => match payload["kind"].as_str() {
            Some("upsert") => {
                match protocol::parse_agent_with_project(&payload["agent"], payload.get("project"))
                {
                    Ok(agent) => {
                        agents.insert(agent.id.clone(), agent.clone());
                        emit_event(events, PaseoEvent::AgentUpserted(agent));
                    }
                    Err(error) => log::warn!("Paseo sent an unreadable agent update: {error:#}"),
                }
            }
            Some("remove") => {
                if let Some(agent_id) = payload["agentId"].as_str() {
                    agents.remove(agent_id);
                    emit_event(
                        events,
                        PaseoEvent::AgentRemoved {
                            agent_id: agent_id.to_owned(),
                        },
                    );
                }
            }
            _ => {}
        },
        "fetch_agent_timeline_response" => emit_timeline(payload, events, timeline),
        "agent.provider_subagents.update" => emit_subagent_update(payload, events),
        "fetch_workspaces_response" => match protocol::parse_workspace_page(payload) {
            Ok((workspaces, empty_projects, next_cursor)) => emit_event(
                events,
                PaseoEvent::WorkspacesSnapshot {
                    workspaces,
                    empty_projects,
                    next_cursor,
                },
            ),
            Err(error) => log::warn!("invalid Paseo workspaces snapshot: {error:#}"),
        },
        "workspace_update" => emit_workspace_update(payload, events),
        "project.update" => emit_project_update(payload, events),
        "workspace.label.list.response" => match protocol::parse_labels(payload) {
            Ok(labels) => emit_event(events, PaseoEvent::LabelsSnapshot(labels)),
            Err(error) => log::warn!("invalid Paseo labels snapshot: {error:#}"),
        },
        "workspace.label.update" => emit_label_update(payload, events),
        "script_status_update" => match (
            payload["workspaceId"].as_str(),
            protocol::parse_workspace_scripts(payload),
        ) {
            (Some(workspace_id), Ok(scripts)) => emit_event(
                events,
                PaseoEvent::ScriptsChanged {
                    workspace_id: workspace_id.to_owned(),
                    scripts,
                },
            ),
            (None, _) => log::warn!("Paseo sent script statuses without a workspace"),
            (_, Err(error)) => log::warn!("Paseo sent unreadable script statuses: {error:#}"),
        },
        "workspace_setup_progress" => match (
            payload["workspaceId"].as_str(),
            protocol::parse_setup_snapshot(payload),
        ) {
            (Some(workspace_id), Ok(snapshot)) => emit_event(
                events,
                PaseoEvent::SetupProgress {
                    workspace_id: workspace_id.to_owned(),
                    snapshot,
                },
            ),
            (None, _) => log::warn!("Paseo sent setup progress without a workspace"),
            (_, Err(error)) => log::warn!("Paseo sent unreadable setup progress: {error:#}"),
        },
        "daemon.update.progress" => {
            if let Some(phase) = payload["phase"].as_str() {
                emit_event(
                    events,
                    PaseoEvent::DaemonUpdateProgress {
                        phase: phase.to_owned(),
                    },
                );
            }
        }
        "agent_stream" => {
            let Some(agent_id) = payload["agentId"]
                .as_str()
                .filter(|agent_id| timeline.contains(agent_id))
            else {
                return;
            };
            if let (Some(epoch), Some(sequence), Some(timestamp), Some(item)) = (
                payload["epoch"].as_str(),
                payload["seq"].as_u64(),
                payload["timestamp"].as_str(),
                payload["event"]["item"].as_object(),
            ) {
                let entry = TimelineEntry {
                    agent_id: agent_id.to_owned(),
                    epoch: epoch.to_owned(),
                    sequence,
                    timestamp: timestamp.to_owned(),
                    payload: protocol::timeline_payload(Value::Object(item.clone())),
                    extra: protocol::entry_extra(payload, "event"),
                };
                timeline.advance(agent_id, epoch, sequence);
                emit_event(events, PaseoEvent::TimelineEntry(entry));
            }
        }
        "providers_snapshot_update" => {
            // Project-scoped snapshots answer a caller's `providers(Some(cwd))`; only the global
            // snapshot replaces the provider list.
            if payload.get("cwd").is_none_or(Value::is_null) {
                match protocol::parse_providers(payload) {
                    Ok(providers) => emit_event(events, PaseoEvent::ProvidersChanged(providers)),
                    Err(error) => log::warn!("invalid Paseo provider snapshot update: {error:#}"),
                }
            }
        }
        "agent_permission_request" => match protocol::parse_permission(payload) {
            Ok(permission) => {
                permissions.insert(permission.request_id.clone(), permission.agent_id.clone());
                emit_event(events, PaseoEvent::PermissionRequested(permission));
            }
            Err(error) => log::warn!("Paseo sent an unreadable permission request: {error:#}"),
        },
        "terminals_changed" => {
            match (payload["cwd"].as_str(), protocol::parse_terminals(payload)) {
                (Some(cwd), Ok(terminals)) => emit_event(
                    events,
                    PaseoEvent::TerminalsChanged {
                        cwd: cwd.to_owned(),
                        subscription_id: payload["subscriptionId"].as_str().map(str::to_owned),
                        terminals,
                    },
                ),
                (None, _) => log::warn!("Paseo sent a terminal list without a directory"),
                (_, Err(error)) => log::warn!("Paseo sent an unreadable terminal list: {error:#}"),
            }
        }
        "terminal_stream_exit" => {
            if let Some(terminal_id) = payload["terminalId"].as_str() {
                emit_event(
                    events,
                    PaseoEvent::TerminalExited {
                        terminal_id: terminal_id.to_owned(),
                        error: payload["error"].as_str().map(str::to_owned),
                    },
                );
            }
        }
        "dictation_stream_partial" | "dictation_stream_final" | "dictation_stream_error" => {
            let Some(dictation_id) = payload["dictationId"].as_str().map(str::to_owned) else {
                return;
            };
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            let event = match message_type {
                "dictation_stream_partial" => PaseoEvent::DictationPartial { dictation_id, text },
                "dictation_stream_final" => PaseoEvent::DictationFinal { dictation_id, text },
                _ => PaseoEvent::DictationFailed {
                    dictation_id,
                    error: payload["error"]
                        .as_str()
                        .unwrap_or("dictation failed")
                        .to_owned(),
                },
            };
            emit_event(events, event);
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
    match protocol::parse_agents(payload) {
        Ok(parsed) => {
            if payload["subscriptionId"].as_str().is_some() {
                agents.clear();
                permissions.clear();
            }
            for agent in parsed {
                permissions.extend(
                    protocol::pending_permissions(&agent)
                        .map(|request| (request.request_id, request.agent_id)),
                );
                agents.insert(agent.id.clone(), agent);
            }
        }
        Err(error) => log::warn!("Paseo sent an unreadable agent directory page: {error:#}"),
    }
    // The run loop fetches the next page itself; receivers get the directory once it is whole.
    if next_agents_cursor(payload).ok().flatten().is_none() {
        emit_event(
            events,
            PaseoEvent::AgentsChanged(agents.values().cloned().collect()),
        );
    }
}

/// Moves an agent's reconnect cursor to the end of a timeline page. Older pages leave it alone,
/// because they end before what was already seen.
fn advance_timeline_cursor(payload: &Value, timeline: &mut TimelineSubscriptions) {
    if payload["direction"] == "before" {
        return;
    }
    let Some(agent_id) = payload["agentId"].as_str() else {
        return;
    };
    let last_entry = payload["entries"]
        .as_array()
        .and_then(|entries| entries.last())
        .and_then(|entry| {
            entry["seqEnd"]
                .as_u64()
                .or_else(|| entry["seqStart"].as_u64())
        });
    if let (Some(epoch), Some(sequence)) = (payload["epoch"].as_str(), last_entry) {
        timeline.advance(agent_id, epoch, sequence);
    }
    if let (Some(epoch), Some(sequence)) = (
        payload["endCursor"]["epoch"].as_str(),
        payload["endCursor"]["seq"].as_u64(),
    ) {
        timeline.advance(agent_id, epoch, sequence);
    }
}

/// Delivers a timeline page nobody is waiting for, such as a reconnect catch-up, as events.
fn emit_timeline(
    payload: &Value,
    events: &Sender<PaseoEvent>,
    timeline: &mut TimelineSubscriptions,
) {
    match protocol::parse_timeline(payload) {
        Ok(entries) => {
            advance_timeline_cursor(payload, timeline);
            for entry in entries {
                emit_event(events, PaseoEvent::TimelineEntry(entry));
            }
        }
        Err(error) => log::warn!("Paseo sent an unreadable timeline page: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::{WebSocketStream, tokio::accept_async};
    use futures::StreamExt;
    use std::collections::BTreeMap;
    use tokio::net::{TcpListener, TcpStream};

    async fn server_socket(
        stream: TcpStream,
    ) -> async_tungstenite::WebSocketStream<async_tungstenite::tokio::TokioAdapter<TcpStream>> {
        let mut socket = accept_async(stream).await.expect("accept mock socket");
        let hello = next_json(&mut socket).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["protocolVersion"], 1);
        assert_eq!(hello["capabilities"]["selective_agent_timeline"], true);
        assert_eq!(hello["capabilities"]["projected_subagent_timeline"], true);
        assert_eq!(hello["capabilities"]["timeline_notifications"], true);
        assert_eq!(hello["capabilities"]["reasoning_merge_enum"], true);
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
        assert!(matches!(
            &history.entries[0].payload,
            TimelinePayload::Message(item) if item["future"] == 7
        ));
        assert!(history.entries[0].extra.get("item").is_none());
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
                assert_eq!(
                    hello["auth"],
                    json!({"kind":"password", "password":"test-secret"})
                );
                assert_eq!(hello["capabilities"]["hello_rejection"], true);
                send_json(&mut socket, json!({"type":"session", "message":{"type":"status", "payload":{"status":"server_info", "serverId":"mock", "features":{"ownedSubscriptions":true,"providersSnapshot":true,"creationLifecycle":true}}}})).await;
                next_request(&mut socket, "fetch_agents_request").await;
            }
        });
        let (_session, _events) = connect_with_ssh_executable(
            target(port),
            Credentials {
                password: Some(RuntimePassword::new("test-secret".into())),
                paseo_home: None,
            },
            "test-client".into(),
            PathBuf::from("ssh"),
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

    /// Accepts one socket, records its subprotocol header, and returns the hello it sent.
    #[allow(clippy::result_large_err)]
    async fn accept_recording_protocol(
        listener: TcpListener,
    ) -> (
        WebSocketStream<async_tungstenite::tokio::TokioAdapter<TcpStream>>,
        Option<String>,
        Value,
    ) {
        use async_tungstenite::tokio::accept_hdr_async;
        use async_tungstenite::tungstenite::handshake::server::{Request, Response};

        let (stream, _) = listener.accept().await.expect("accept client");
        let mut protocol = None;
        let mut socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            protocol = request
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            Ok(response)
        })
        .await
        .expect("accept mock socket");
        let hello = next_json(&mut socket).await;
        (socket, protocol, hello)
    }

    fn server_info_json() -> Value {
        json!({"type":"session", "message":{"type":"status", "payload":{"status":"server_info", "serverId":"mock", "features":{"ownedSubscriptions":true,"providersSnapshot":true,"creationLifecycle":true}}}})
    }

    #[tokio::test]
    async fn password_that_is_not_a_header_token_is_sent_only_in_hello_auth() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (mut socket, protocol, hello) = accept_recording_protocol(listener).await;
            assert_eq!(protocol, None);
            assert_eq!(
                hello["auth"],
                json!({"kind":"password", "password":"pass word@/x"})
            );
            send_json(&mut socket, server_info_json()).await;
            next_request(&mut socket, "fetch_agents_request").await;
        });
        let (_session, _events) = connect_with_ssh_executable(
            target(port),
            Credentials {
                password: Some(RuntimePassword::new("pass word@/x".into())),
                paseo_home: None,
            },
            "test-client".into(),
            PathBuf::from("ssh"),
        )
        .await
        .expect("connection with a non-token password");
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn hello_rejections_are_auth_errors() {
        use async_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

        let cases = [
            (
                Some("password_required"),
                None,
                AuthRejection::PasswordRequired,
            ),
            (
                None,
                Some("Incorrect password"),
                AuthRejection::IncorrectPassword,
            ),
            (
                Some("incorrect_password"),
                Some("Incorrect password"),
                AuthRejection::IncorrectPassword,
            ),
        ];
        for (rejected_reason, close_reason, expected) in cases {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock daemon");
            let port = listener.local_addr().expect("mock address").port();
            let server = tokio::spawn(async move {
                let (mut socket, _, _) = accept_recording_protocol(listener).await;
                if let Some(reason) = rejected_reason {
                    send_json(
                        &mut socket,
                        json!({"type":"hello.rejected", "reason":reason, "accepts":["password"]}),
                    )
                    .await;
                }
                if let Some(reason) = close_reason {
                    socket
                        .close(Some(CloseFrame {
                            code: CloseCode::from(4401),
                            reason: reason.into(),
                        }))
                        .await
                        .expect("close mock socket");
                }
            });
            let error = match connect_with_ssh_executable(
                target(port),
                Credentials::default(),
                "test-client".into(),
                PathBuf::from("ssh"),
            )
            .await
            {
                Ok(_) => panic!("rejected hello should fail"),
                Err(error) => error,
            };
            assert_eq!(error.downcast_ref::<AuthRejection>(), Some(&expected));
            assert_eq!(error.to_string(), expected.to_string());
            server.await.expect("mock daemon task");
        }
    }

    fn paseo_home_with(listen: &str, token: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("create paseo home");
        std::fs::write(
            home.path().join("paseo.pid"),
            json!({"pid":1, "listen":listen}).to_string(),
        )
        .expect("write paseo.pid");
        std::fs::write(home.path().join("local-credential"), format!("{token}\n"))
            .expect("write local credential");
        home
    }

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ0123-_x";

    fn direct(url: &str) -> ConnectionTarget {
        ConnectionTarget::Direct {
            websocket_url: url.into(),
            editor_ssh: None,
        }
    }

    #[test]
    fn local_credential_matches_the_running_daemon_on_loopback() {
        let home = paseo_home_with("127.0.0.1:6767", TOKEN);
        for url in [
            "ws://127.0.0.1:6767/ws",
            "ws://localhost:6767/ws",
            "ws://[::1]:6767/ws",
            "wss://localhost:6767/ws",
        ] {
            assert_eq!(
                local_credential(&direct(url), home.path()).as_deref(),
                Some(TOKEN),
                "{url}"
            );
        }
        let any_address = paseo_home_with("0.0.0.0:6767", TOKEN);
        assert_eq!(
            local_credential(&direct("ws://localhost:6767/ws"), any_address.path()).as_deref(),
            Some(TOKEN)
        );
    }

    #[test]
    fn local_credential_skips_other_daemons() {
        let home = paseo_home_with("127.0.0.1:6767", TOKEN);
        for target in [
            direct("ws://127.0.0.1:6768/ws"),
            direct("ws://example.com:6767/ws"),
            ConnectionTarget::Ssh {
                host: "localhost".into(),
                username: None,
                ssh_port: 22,
                daemon_port: 6767,
            },
        ] {
            assert_eq!(local_credential(&target, home.path()), None, "{target:?}");
        }
        let unix = paseo_home_with("unix:///tmp/paseo.sock", TOKEN);
        assert_eq!(
            local_credential(&direct("ws://localhost:6767/ws"), unix.path()),
            None
        );
        let missing = tempfile::tempdir().expect("create empty paseo home");
        assert_eq!(
            local_credential(&direct("ws://localhost:6767/ws"), missing.path()),
            None
        );
    }

    #[test]
    fn local_credential_rejects_a_malformed_token() {
        for (name, token) in [("short", "abc"), ("symbols", &"a/".repeat(22)[..43])] {
            let home = paseo_home_with("127.0.0.1:6767", token);
            assert_eq!(
                local_credential(&direct("ws://localhost:6767/ws"), home.path()),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn typed_password_wins_over_the_local_credential() {
        let home = paseo_home_with("127.0.0.1:6767", TOKEN);
        let credentials = Credentials {
            password: Some(RuntimePassword::new("test-secret".into())),
            paseo_home: Some(home.path().to_owned()),
        };
        assert!(matches!(
            credentials.hello_auth(&direct("ws://localhost:6767/ws")),
            Some(HelloAuth::Password("test-secret"))
        ));
    }

    #[tokio::test]
    async fn local_credential_is_sent_without_a_header() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let home = paseo_home_with(&format!("127.0.0.1:{port}"), TOKEN);
        let server = tokio::spawn(async move {
            let (mut socket, protocol, hello) = accept_recording_protocol(listener).await;
            assert_eq!(protocol, None);
            assert_eq!(
                hello["auth"],
                json!({"kind":"localCredential", "token":TOKEN})
            );
            send_json(&mut socket, server_info_json()).await;
            next_request(&mut socket, "fetch_agents_request").await;
        });
        let (_session, _events) = connect_with_ssh_executable(
            target(port),
            Credentials {
                password: None,
                paseo_home: Some(home.path().to_owned()),
            },
            "test-client".into(),
            PathBuf::from("ssh"),
        )
        .await
        .expect("connection with the local credential");
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn reconnect_stops_when_the_daemon_rejects_the_password() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("first connection");
            let mut first = server_socket(stream).await;
            next_request(&mut first, "fetch_agents_request").await;
            drop(first);
            let (mut second, _, _) = accept_recording_protocol(listener).await;
            send_json(
                &mut second,
                json!({"type":"hello.rejected", "reason":"incorrect_password", "accepts":["password"]}),
            )
            .await;
        });
        let (_session, events) = connect_with_ssh_executable(
            target(port),
            Credentials::default(),
            "test-client".into(),
            PathBuf::from("ssh"),
        )
        .await
        .expect("first connection");
        let mut failure = None;
        while let Ok(event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("event timed out")
        {
            if let PaseoEvent::ConnectionFailed { reason } = event {
                failure = Some(reason);
            }
        }
        assert_eq!(failure.as_deref(), Some("Incorrect password"));
        server.await.expect("mock daemon task");
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
            let _labels = next_request(&mut socket, "workspace.label.list.request").await;
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
        let (session, events) = connect_with_ssh_executable(
            target,
            Credentials::default(),
            "test-client".into(),
            executable,
        )
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
            assert_eq!(creation["config"]["modeId"], "plan");
            assert_eq!(creation["config"]["thinkingOptionId"], "high");
            assert_eq!(
                creation["config"]["featureValues"],
                json!({"fast_mode": true})
            );
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
            let _ = next_request(&mut third, "workspace.label.list.request").await;
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
                mode_id: Some("plan".into()),
                thinking_option_id: Some("high".into()),
                images: Vec::new(),
                attachments: Vec::new(),
                worktree: None,
                idempotency_key: "stable-key".into(),
                workspace_id: None,
                feature_values: BTreeMap::from([("fast_mode".into(), json!(true))]),
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
                replay: None,
                sends_message: false,
                response_type: "fetch_agent_timeline_response",
                reply,
            },
        )]);
        let mut agents = HashMap::new();
        let mut permissions = HashMap::new();
        let (events, event_receiver) = async_channel::unbounded();
        let mut timeline = TimelineSubscriptions::default();
        timeline.replace(vec!["agent-1".into()]);
        timeline
            .cursors
            .insert("agent-1".into(), ("epoch-1".into(), 100));
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
            &mut timeline,
        );
        assert_eq!(
            timeline.cursors.get("agent-1"),
            Some(&("epoch-1".to_string(), 100))
        );
        assert!(
            event_receiver.try_recv().is_err(),
            "the caller applies the page it asked for"
        );
    }

    fn tail_page(request_id: &str) -> Value {
        json!({
            "type":"fetch_agent_timeline_response",
            "payload":{
                "requestId":request_id,
                "agentId":"agent-1",
                "epoch":"epoch-1",
                "direction":"tail",
                "entries":[{
                    "seqStart":4,
                    "seqEnd":6,
                    "timestamp":"now",
                    "item":{"type":"assistant_message","text":"hello"}
                }],
                "endCursor":{"epoch":"epoch-1","seq":6}
            }
        })
    }

    #[tokio::test]
    async fn requested_timeline_page_reaches_only_its_caller() {
        let (reply, response) = oneshot::channel();
        let mut pending = HashMap::from([(
            "tail".to_string(),
            Pending {
                replay: None,
                sends_message: false,
                response_type: "fetch_agent_timeline_response",
                reply,
            },
        )]);
        let (events, event_receiver) = async_channel::unbounded();
        let mut timeline = subscriptions(&["agent-1"]);
        handle_message(
            &tail_page("tail"),
            &mut pending,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            &mut timeline,
        );
        let page = response.await.expect("reply").expect("tail payload");
        assert_eq!(page["entries"][0]["seqStart"], 4);
        assert!(event_receiver.try_recv().is_err());
        assert_eq!(
            timeline.cursors.get("agent-1"),
            Some(&("epoch-1".to_string(), 6)),
            "a reconnect still catches up from the requested page"
        );

        handle_message(
            &tail_page("catch-up"),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            &mut timeline,
        );
        let Ok(PaseoEvent::TimelineEntry(entry)) = event_receiver.try_recv() else {
            panic!("a page nobody asked for arrives as events");
        };
        assert_eq!(entry.sequence, 4);
        assert_eq!(entry.extra["seqEnd"], 6);
        assert!(entry.extra.get("item").is_none());
    }

    #[test]
    fn reconnect_waits_longer_after_each_failure() {
        let delays = (0..7)
            .map(|failed_attempts| reconnect_delay(failed_attempts).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(reconnect_delay(u32::MAX), RECONNECT_DELAY_LIMIT);
    }

    #[test]
    fn agent_updates_are_sent_one_agent_at_a_time() {
        let (events, receiver) = async_channel::unbounded();
        let mut agents = HashMap::new();
        let mut update = |message: Value| {
            handle_message(
                &message,
                &mut HashMap::new(),
                &mut agents,
                &mut HashMap::new(),
                &events,
                &mut TimelineSubscriptions::default(),
            )
        };
        update(json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":agent()}}));
        update(
            json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":{"title":"no ID"}}}),
        );
        update(json!({"type":"agent_update", "payload":{"kind":"remove", "agentId":"agent-1"}}));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PaseoEvent::AgentUpserted(agent)) if agent.id == "agent-1"
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PaseoEvent::AgentRemoved { agent_id }) if agent_id == "agent-1"
        ));
        assert!(
            receiver.try_recv().is_err(),
            "an unreadable upsert sends nothing"
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn directory_arrives_once_its_last_page_does() {
        let (events, receiver) = async_channel::unbounded();
        let mut agents = HashMap::new();
        let mut permissions = HashMap::from([("resolved-offline".to_string(), "gone".to_string())]);
        handle_message(
            &json!({"type":"fetch_agents_response", "payload":{
                "requestId":"subscription", "subscriptionId":"owned",
                "entries":[{"agent":{"id":"agent-1", "status":"idle", "cwd":"/tmp/project",
                    "pendingPermissions":[{"id":"permission-1"}]}}],
                "pageInfo":{"hasMore":true,"nextCursor":"next"}
            }}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            &mut TimelineSubscriptions::default(),
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            permissions.keys().collect::<Vec<_>>(),
            vec!["permission-1"],
            "a snapshot forgets permissions resolved while disconnected"
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
            &mut TimelineSubscriptions::default(),
        );
        let Ok(PaseoEvent::AgentsChanged(directory)) = receiver.try_recv() else {
            panic!("the whole directory follows its last page");
        };
        assert_eq!(directory.len(), 2);
    }

    #[tokio::test]
    async fn correlated_rpc_error_completes_request() {
        let (reply, response) = oneshot::channel();
        let mut pending = HashMap::from([(
            "request-1".to_string(),
            Pending {
                replay: None,
                sends_message: false,
                response_type: "fetch_agents_response",
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
            &mut TimelineSubscriptions::default(),
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
        let mut timeline = TimelineSubscriptions::default();
        handle_message(
            &json!({"type":"fetch_agents_response", "payload":{
                "requestId":"subscription", "subscriptionId":"owned", "entries":[{"agent":agent()}],
                "pageInfo":{"hasMore":true,"nextCursor":"next"}
            }}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            &mut timeline,
        );
        handle_message(
            &json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":{
                "id":"live-agent", "status":"running", "cwd":"/tmp/project"
            }}}),
            &mut HashMap::new(),
            &mut agents,
            &mut permissions,
            &events,
            &mut timeline,
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
            &mut timeline,
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
            &mut subscriptions(&["other"]),
        );
        assert!(receiver.try_recv().is_err());
        handle_message(
            &replacement,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            &mut subscriptions(&["other", "agent-1"]),
        );
        assert!(
            matches!(receiver.try_recv(), Ok(PaseoEvent::TimelineReplaced { agent_id, epoch }) if agent_id == "agent-1" && epoch == "epoch-2")
        );
    }

    fn subscriptions(agent_ids: &[&str]) -> TimelineSubscriptions {
        let mut timeline = TimelineSubscriptions::default();
        timeline.replace(agent_ids.iter().map(|id| id.to_string()).collect());
        timeline
    }

    fn timeline_response(
        request_id: &Value,
        agent_id: &str,
        direction: &str,
        sequence: u64,
        has_newer: bool,
    ) -> Value {
        json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":request_id,"agentId":agent_id,"direction":direction,"epoch":"epoch-1","reset":false,"staleCursor":false,"startCursor":{"epoch":"epoch-1","seq":sequence},"endCursor":{"epoch":"epoch-1","seq":sequence},"hasOlder":false,"hasNewer":has_newer,"entries":[{"seqStart":sequence,"seqEnd":sequence,"timestamp":"now","item":{"type":"assistant_message","text":agent_id}}],"error":null}}})
    }

    fn stream_entry(agent_id: &str, sequence: u64) -> Value {
        json!({"type":"session", "message":{"type":"agent_stream", "payload":{"agentId":agent_id,"epoch":"epoch-1","seq":sequence,"timestamp":"now","event":{"type":"timeline","item":{"type":"assistant_message","text":"chunk"}}}}})
    }

    async fn next_timeline_entry(events: &Receiver<PaseoEvent>) -> TimelineEntry {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("timeline event timed out")
                .expect("event receiver closed");
            if let PaseoEvent::TimelineEntry(entry) = event {
                return entry;
            }
        }
    }

    async fn answer_subscription<S>(socket: &mut WebSocketStream<S>, expected: Value)
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin,
    {
        let request = next_request(socket, "agent.timeline.set_subscription.request").await;
        assert_eq!(request["agentIds"], expected);
        send_json(socket, json!({"type":"session", "message":{"type":"agent.timeline.set_subscription.response", "payload":{"requestId":request["requestId"],"agentIds":expected}}})).await;
    }

    #[tokio::test]
    async fn every_subscribed_agent_receives_stream_entries() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = server_socket(stream).await;
            let session_events =
                next_request(&mut socket, "session.events.set_subscription.request").await;
            assert!(
                session_events["events"]
                    .as_array()
                    .expect("event list")
                    .contains(&json!("providers_snapshot_update"))
            );
            assert!(
                session_events["events"]
                    .as_array()
                    .expect("event list")
                    .contains(&json!("agent.provider_subagents.update"))
            );
            answer_subscription(&mut socket, json!(["agent-1", "agent-2"])).await;
            send_json(&mut socket, stream_entry("agent-3", 1)).await;
            send_json(&mut socket, stream_entry("agent-2", 5)).await;
            send_json(&mut socket, stream_entry("agent-1", 9)).await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"providers_snapshot_update", "payload":{"cwd":"/tmp/project","entries":[{"provider":"scoped","status":"ready"}],"generatedAt":"now"}}})).await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"providers_snapshot_update", "payload":{"entries":[{"provider":"codex","status":"ready"}],"generatedAt":"now"}}})).await;
            answer_subscription(&mut socket, json!([])).await;
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        session
            .set_timeline_subscriptions(vec!["agent-1".into(), "agent-2".into()])
            .await
            .expect("subscribe to two agents");
        let first = next_timeline_entry(&events).await;
        assert_eq!((first.agent_id.as_str(), first.sequence), ("agent-2", 5));
        let second = next_timeline_entry(&events).await;
        assert_eq!((second.agent_id.as_str(), second.sequence), ("agent-1", 9));
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("provider event timed out")
                .expect("event receiver closed");
            match event {
                PaseoEvent::ProvidersChanged(providers) => {
                    assert_eq!(providers[0].id, "codex");
                    break;
                }
                PaseoEvent::TimelineEntry(entry) => panic!("unexpected entry {entry:?}"),
                _ => {}
            }
        }
        session
            .set_timeline_subscriptions(Vec::new())
            .await
            .expect("clear subscriptions");
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn reconnect_renews_every_subscription_and_catches_up_each_agent() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.expect("first connection");
            let mut first = server_socket(first_stream).await;
            answer_subscription(&mut first, json!(["agent-1", "agent-2"])).await;
            for (agent_id, sequence) in [("agent-1", 3), ("agent-2", 7)] {
                let tail = next_request(&mut first, "fetch_agent_timeline_request").await;
                assert_eq!(tail["agentId"], agent_id);
                assert_eq!(tail["direction"], "tail");
                send_json(
                    &mut first,
                    timeline_response(&tail["requestId"], agent_id, "tail", sequence, false),
                )
                .await;
            }
            send_json(&mut first, stream_entry("agent-2", 8)).await;
            answer_subscription(&mut first, json!(["agent-1", "agent-2"])).await;
            drop(first);
            let (second_stream, _) = listener.accept().await.expect("second connection");
            let mut second = server_socket(second_stream).await;
            let renewed =
                next_request(&mut second, "agent.timeline.set_subscription.request").await;
            assert_eq!(renewed["agentIds"], json!(["agent-1", "agent-2"]));
            let mut catch_up = HashMap::new();
            for _ in 0..2 {
                let request = next_request(&mut second, "fetch_agent_timeline_request").await;
                assert_eq!(request["direction"], "after");
                assert_eq!(request["limit"], TIMELINE_PAGE_SIZE);
                catch_up.insert(
                    request["agentId"].as_str().expect("agent ID").to_owned(),
                    request["cursor"].clone(),
                );
            }
            assert_eq!(catch_up["agent-1"], json!({"epoch":"epoch-1","seq":3}));
            assert_eq!(catch_up["agent-2"], json!({"epoch":"epoch-1","seq":8}));
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        session
            .set_timeline_subscriptions(vec!["agent-1".into(), "agent-2".into()])
            .await
            .expect("subscribe");
        for agent_id in ["agent-1", "agent-2"] {
            let page = session.timeline_tail(agent_id).await.expect("tail page");
            assert_eq!(page.entries[0].agent_id, agent_id);
        }
        loop {
            let entry = next_timeline_entry(&events).await;
            if entry.agent_id == "agent-2" && entry.sequence == 8 {
                break;
            }
        }
        session
            .set_timeline_subscriptions(vec!["agent-1".into(), "agent-2".into()])
            .await
            .expect("resubscribe keeps cursors");
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn replacement_and_reset_refetch_the_affected_agent_and_after_pages_continue() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = server_socket(stream).await;
            answer_subscription(&mut socket, json!(["agent-1", "agent-2"])).await;
            send_json(&mut socket, json!({"type":"session", "message":{"type":"agent.timeline.replacement", "payload":{"agentId":"agent-2","epoch":"epoch-2"}}})).await;
            let refetch = next_request(&mut socket, "fetch_agent_timeline_request").await;
            assert_eq!(refetch["agentId"], "agent-2");
            assert_eq!(refetch["direction"], "tail");
            assert!(refetch.get("cursor").is_none());
            send_json(&mut socket, json!({"type":"session", "message":{"type":"fetch_agent_timeline_response", "payload":{"requestId":"unrelated","agentId":"agent-1","direction":"after","epoch":"epoch-9","reset":true,"staleCursor":true,"startCursor":null,"endCursor":null,"hasOlder":false,"hasNewer":false,"entries":[],"error":null}}})).await;
            let reset = next_request(&mut socket, "fetch_agent_timeline_request").await;
            assert_eq!(reset["agentId"], "agent-1");
            assert_eq!(reset["direction"], "tail");
            send_json(
                &mut socket,
                timeline_response(&json!("unrelated"), "agent-2", "after", 4, true),
            )
            .await;
            let next_page = next_request(&mut socket, "fetch_agent_timeline_request").await;
            assert_eq!(next_page["agentId"], "agent-2");
            assert_eq!(next_page["direction"], "after");
            assert_eq!(next_page["cursor"], json!({"epoch":"epoch-1","seq":4}));
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        session
            .set_timeline_subscriptions(vec!["agent-1".into(), "agent-2".into()])
            .await
            .expect("subscribe");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("replacement event timed out")
                .expect("event receiver closed");
            if let PaseoEvent::TimelineReplaced { agent_id, epoch } = event {
                assert_eq!((agent_id.as_str(), epoch.as_str()), ("agent-2", "epoch-2"));
                break;
            }
        }
        server.await.expect("mock daemon task");
    }

    fn mock_daemon(
        replies: Vec<(&'static str, Value)>,
    ) -> (PaseoSession, tokio::task::JoinHandle<Vec<Value>>) {
        let (commands, mut receiver) = mpsc::channel(4);
        let daemon = tokio::spawn(async move {
            let mut messages = Vec::new();
            for (expected_response_type, reply_payload) in replies {
                let Some(Command::Request {
                    message,
                    response_type,
                    reply,
                    ..
                }) = receiver.recv().await
                else {
                    panic!("expected a request for {expected_response_type}")
                };
                assert_eq!(response_type, expected_response_type);
                assert!(message["requestId"].as_str().is_some());
                deliver(reply, Ok(reply_payload));
                messages.push(message);
            }
            messages
        });
        (PaseoSession { commands }, daemon)
    }

    fn subagent_value(status: &str) -> Value {
        json!({"id":"task-1","parentAgentId":"agent-1","parentSubagentId":null,"provider":"claude","title":"sr-reviewer-deep","description":"Review the diff","status":status,"createdAt":"2026-09-28T10:00:00Z","updatedAt":"2026-09-28T10:01:00Z","toolCallId":"toolu_1","cwd":"/repo","subtitle":"sr-reviewer-deep · Opus 5.5 · 12k tokens"})
    }

    #[tokio::test]
    async fn subagent_requests_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "agent.provider_subagents.list.response",
                json!({"parentAgentId":"agent-1","subagents":[subagent_value("running")],"error":null}),
            ),
            (
                "agent.provider_subagents.timeline.get.response",
                json!({"parentAgentId":"agent-1","subagentId":"task-1","provider":"claude","direction":"tail","epoch":"epoch-1","startCursor":{"epoch":"epoch-1","seq":3},"endCursor":{"epoch":"epoch-1","seq":4},"reset":false,"staleCursor":false,"gap":false,"window":{"minSeq":1,"maxSeq":4,"nextSeq":5},"hasOlder":true,"hasNewer":false,"rows":[{"item":{"type":"assistant_message","text":"Reading"},"timestamp":"2026-09-28T10:00:30Z","seq":4,"seqStart":3}],"error":null}),
            ),
            (
                "agent.provider_subagents.timeline.get.response",
                json!({"parentAgentId":"agent-1","subagentId":"task-1","provider":"claude","direction":"before","epoch":"epoch-1","reset":false,"staleCursor":false,"gap":false,"window":{"minSeq":1,"maxSeq":4,"nextSeq":5},"hasOlder":false,"hasNewer":true,"rows":[],"error":"unknown subagent"}),
            ),
        ]);
        let subagents = session.subagents("agent-1").await.expect("subagents");
        assert_eq!(subagents[0].status, "running");
        assert_eq!(subagents[0].tool_call_id.as_deref(), Some("toolu_1"));
        let page = session
            .subagent_timeline("agent-1", "task-1", None)
            .await
            .expect("subagent timeline");
        assert_eq!(
            page.entries[0].agent_id,
            subagent_timeline_id("agent-1", "task-1")
        );
        assert_eq!(page.entries[0].sequence, 3);
        assert_eq!(page.entries[0].extra["seqEnd"], 4);
        assert!(page.has_older);
        let cursor = page.start_cursor.expect("start cursor");
        let error = session
            .subagent_timeline("agent-1", "task-1", Some(&cursor))
            .await
            .expect_err("daemon error");
        assert!(error.to_string().contains("unknown subagent"));
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["parentAgentId"], "agent-1");
        assert_eq!(messages[1]["direction"], "tail");
        assert_eq!(messages[2]["direction"], "before");
        assert_eq!(messages[2]["cursor"], json!({"epoch":"epoch-1","seq":3}));
    }

    fn workspace_value(id: &str) -> Value {
        json!({"id":id,"projectId":"prj_658731eeb474c372","projectDisplayName":"axon-monorepo","projectRootPath":"/home/sr/projects/work/axon-monorepo","workspaceDirectory":"/home/sr/.paseo/worktrees/3r36fnq4/prolific-snake","worktreeSlug":"prolific-snake","projectKind":"git","workspaceKind":"worktree","name":"Verify instinct-machine","title":"Verify instinct-machine","pinnedAt":null,"archivingAt":null,"status":"done","activityAt":null,"diffStat":{"additions":1386,"deletions":12},"scripts":[],"gitRuntime":{"currentBranch":"saanu-instinct-machine-tla","isPaseoOwnedWorktree":true}})
    }

    fn project_value() -> Value {
        json!({"projectId":"prj_f1eff855e1aa39cd","projectKey":"remote:github.com/saanuregh/dotfiles","projectDisplayName":"dotfiles","projectCustomName":null,"projectCustomIconRevision":null,"projectIconRevision":"automatic:none:v1","projectRootPath":"/home/sr/projects/personal/dotfiles","projectKind":"git"})
    }

    #[test]
    fn parse_workspace_descriptor_keeps_ids() {
        let workspace =
            protocol::parse_workspace(&workspace_value("wks_274c64ad64c5c640")).expect("workspace");
        assert_eq!(workspace.id, "wks_274c64ad64c5c640");
        assert_eq!(workspace.project_id, "prj_658731eeb474c372");
        assert_eq!(
            workspace.directory,
            PathBuf::from("/home/sr/.paseo/worktrees/3r36fnq4/prolific-snake")
        );
        assert_eq!(workspace.kind, "worktree");
        assert!(workspace.is_paseo_worktree);
        assert_eq!(
            workspace.current_branch.as_deref(),
            Some("saanu-instinct-machine-tla")
        );
        assert_eq!(
            workspace.diff_stat,
            Some(DiffStat {
                additions: 1386,
                deletions: 12
            })
        );
        assert!(workspace.labels.is_empty(), "labels are optional");

        let mut without_directory = workspace_value("wks_1");
        without_directory
            .as_object_mut()
            .expect("object")
            .remove("workspaceDirectory");
        assert_eq!(
            protocol::parse_workspace(&without_directory)
                .expect("workspace")
                .directory,
            PathBuf::from("/home/sr/projects/work/axon-monorepo"),
            "a workspace without its own directory runs in the project root"
        );

        let project = protocol::parse_project(&project_value()).expect("project");
        assert_eq!(project.id, "prj_f1eff855e1aa39cd");
        assert_eq!(project.display_name, "dotfiles");
        assert_eq!(project.icon_revision.as_deref(), Some("automatic:none:v1"));
    }

    #[tokio::test]
    async fn workspace_requests_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "fetch_workspaces_response",
                json!({"entries":[workspace_value("wks_1")],"emptyProjects":[project_value()],"pageInfo":{"nextCursor":"next","prevCursor":null,"hasMore":true}}),
            ),
            (
                "workspace.title.set.response",
                json!({"workspaceId":"wks_1","accepted":true,"title":"New name","error":null}),
            ),
            (
                "workspace.pin.set.response",
                json!({"workspaceId":"wks_1","accepted":true,"pinnedAt":"now","error":null}),
            ),
            (
                "workspace.mark_unread.response",
                json!({"workspaceId":"wks_1","markedAgentId":null,"success":false,"error":null}),
            ),
            (
                "workspace.clear_attention.response",
                json!({"workspaceId":["wks_1"],"clearedAgentIds":["agent-1"],"results":[],"success":true,"error":null}),
            ),
            (
                "workspace.create.response",
                json!({"workspace":workspace_value("wks_2"),"setupTerminalId":null,"error":null}),
            ),
            (
                "project.rename.response",
                json!({"projectId":"prj_1","accepted":true,"customName":null,"error":null}),
            ),
            (
                "project.icon.set.response",
                json!({"projectId":"prj_1","accepted":true,"error":null}),
            ),
        ]);
        let (workspaces, empty_projects, next_cursor) =
            session.workspaces_page(None).await.expect("workspaces");
        assert_eq!(workspaces[0].id, "wks_1");
        assert_eq!(empty_projects[0].id, "prj_f1eff855e1aa39cd");
        assert_eq!(next_cursor.as_deref(), Some("next"));
        session
            .set_workspace_title("wks_1", Some("New name"))
            .await
            .expect("title");
        session
            .set_workspace_pinned("wks_1", true)
            .await
            .expect("pin");
        let error = session
            .mark_workspace_unread("wks_1")
            .await
            .expect_err("nothing to mark");
        assert!(error.to_string().contains("mark the workspace unread"));
        session
            .clear_workspace_attention(vec!["wks_1".into()])
            .await
            .expect("clear");
        let created = session
            .create_workspace(
                &WorkspaceSource::Worktree {
                    cwd: "/repo".into(),
                    project_id: Some("prj_1".into()),
                    base_ref: Some("main".into()),
                },
                Some("Scratch"),
                "key-1",
            )
            .await
            .expect("create");
        assert_eq!(created.id, "wks_2");
        session.rename_project("prj_1", None).await.expect("rename");
        session
            .set_project_icon("prj_1", Some(b"png"))
            .await
            .expect("icon");

        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["page"]["limit"], WORKSPACE_PAGE_SIZE);
        assert!(
            messages[0].get("subscribe").is_none(),
            "a page request never subscribes"
        );
        assert_eq!(messages[1]["title"], "New name");
        assert_eq!(messages[2]["pinned"], true);
        assert_eq!(messages[3]["workspaceId"], "wks_1");
        assert_eq!(messages[4]["workspaceId"], json!(["wks_1"]));
        assert_eq!(
            messages[5]["source"],
            json!({"kind":"worktree","cwd":"/repo","projectId":"prj_1","action":"branch-off","refName":"main"})
        );
        assert_eq!(messages[5]["title"], "Scratch");
        assert_eq!(messages[5]["idempotencyKey"], "key-1");
        assert_eq!(messages[6]["customName"], Value::Null);
        assert_eq!(
            messages[7]["source"],
            json!({"type":"upload","data":"cG5n"})
        );
    }

    #[tokio::test]
    async fn daemon_requests_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "daemon.get_status.response",
                json!({"serverId":"srv-1","version":"0.9.2","pid":2795698,"nodePath":"/bin/node","startedAt":"2026-09-28T08:00:00Z","listen":"127.0.0.1:6767","relay":{"enabled":false,"endpoint":"relay.paseo.sh:443","publicEndpoint":null,"useTls":true,"publicUseTls":true},"providers":[{"provider":"claude","available":true,"error":null},{"provider":"pi","available":false,"error":"not installed"}]}),
            ),
            (
                "status",
                json!({"status":"restart_requested","clientId":"zaseo","reason":"Restarted from Zaseo"}),
            ),
            (
                "daemon.update.response",
                json!({"success":false,"error":"Update Paseo Desktop on the host.","previousVersion":null,"newVersion":null}),
            ),
            (
                "paseo_worktree_list_response",
                json!({"worktrees":[{"worktreePath":"/home/sr/.paseo/worktrees/3r36fnq4/prolific-snake","createdAt":"2026-09-23T12:01:55.734Z","branchName":"saanu-instinct-machine-tla","head":"46d836fd"}],"error":null}),
            ),
            (
                "paseo_worktree_archive_response",
                json!({"success":false,"error":{"code":"NOT_ALLOWED","message":"Only Paseo worktrees can be archived"}}),
            ),
        ]);
        let status = session.daemon_status().await.expect("status");
        assert_eq!(status.version.as_deref(), Some("0.9.2"));
        assert_eq!(status.listen.as_deref(), Some("127.0.0.1:6767"));
        assert_eq!(
            status.relay.as_ref().map(|relay| relay.enabled),
            Some(false)
        );
        assert_eq!(status.providers.len(), 2);
        assert!(!status.providers[1].available);
        session.restart_daemon().await.expect("restart");
        let error = session.update_daemon().await.expect_err("desktop managed");
        assert!(error.to_string().contains("Update Paseo Desktop"));
        let worktrees = session.paseo_worktrees("/repo").await.expect("worktrees");
        assert_eq!(
            worktrees[0].branch.as_deref(),
            Some("saanu-instinct-machine-tla")
        );
        let error = session
            .archive_paseo_worktree("/repo")
            .await
            .expect_err("not a Paseo worktree");
        assert!(error.to_string().contains("Only Paseo worktrees"));

        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "daemon.get_status.request");
        assert_eq!(messages[1]["type"], "restart_server_request");
        assert_eq!(messages[3]["repoRoot"], "/repo");
        assert_eq!(messages[4]["scope"], "worktree");
        assert_eq!(messages[4]["worktreePath"], "/repo");
    }

    #[test]
    fn new_workspace_request_uses_directory_or_worktree_source() {
        assert_eq!(
            workspace_source_value(&WorkspaceSource::Directory {
                path: "/repo".into(),
                project_id: Some("prj_1".into()),
            }),
            json!({"kind":"directory","path":"/repo","projectId":"prj_1"})
        );
        assert_eq!(
            workspace_source_value(&WorkspaceSource::Worktree {
                cwd: "/repo".into(),
                project_id: None,
                base_ref: None,
            }),
            json!({"kind":"worktree","cwd":"/repo","action":"branch-off"}),
            "an unset base lets the daemon use the repository's default branch"
        );
    }

    #[test]
    fn workspace_events_become_events() {
        let (events, receiver) = async_channel::unbounded();
        emit_workspace_update(
            &json!({"kind":"upsert","workspace":workspace_value("wks_1")}),
            &events,
        );
        emit_workspace_update(
            &json!({"kind":"remove","id":"wks_1","removedProjectId":"prj_1"}),
            &events,
        );
        emit_project_update(&json!({"kind":"upsert","project":project_value()}), &events);
        emit_project_update(&json!({"kind":"remove","projectId":"prj_1"}), &events);
        emit_label_update(
            &json!({"kind":"upsert","label":{"name":"review","color":"sky"},"previousName":"todo","generation":"g","seq":2}),
            &events,
        );
        emit_label_update(
            &json!({"kind":"remove","name":"review","generation":"g","seq":3}),
            &events,
        );
        emit_workspace_update(
            &json!({"kind":"upsert","workspace":{"id":"broken"}}),
            &events,
        );
        assert!(matches!(
            receiver.try_recv().expect("workspace upsert"),
            PaseoEvent::WorkspaceUpserted(workspace) if workspace.id == "wks_1"
        ));
        assert!(matches!(
            receiver.try_recv().expect("workspace remove"),
            PaseoEvent::WorkspaceRemoved { workspace_id, removed_project_id }
                if workspace_id == "wks_1" && removed_project_id.as_deref() == Some("prj_1")
        ));
        assert!(matches!(
            receiver.try_recv().expect("project upsert"),
            PaseoEvent::ProjectUpserted(project) if project.display_name == "dotfiles"
        ));
        assert!(matches!(
            receiver.try_recv().expect("project remove"),
            PaseoEvent::ProjectRemoved { project_id } if project_id == "prj_1"
        ));
        assert!(matches!(
            receiver.try_recv().expect("label upsert"),
            PaseoEvent::LabelUpserted { label, previous_name }
                if label.color == "sky" && previous_name.as_deref() == Some("todo")
        ));
        assert!(matches!(
            receiver.try_recv().expect("label remove"),
            PaseoEvent::LabelRemoved { name } if name == "review"
        ));
        assert!(
            receiver.try_recv().is_err(),
            "an unreadable workspace emits nothing"
        );
    }

    #[test]
    fn subagent_updates_become_events() {
        let (events, receiver) = async_channel::unbounded();
        emit_subagent_update(
            &json!({"kind":"upsert","subagent":subagent_value("completed")}),
            &events,
        );
        emit_subagent_update(
            &json!({"kind":"timeline","parentAgentId":"agent-1","subagentId":"task-1","provider":"claude","item":{"type":"assistant_message","text":"Done"},"timestamp":"now","seq":7,"epoch":"epoch-1"}),
            &events,
        );
        emit_subagent_update(
            &json!({"kind":"remove","parentAgentId":"agent-1","subagentId":"task-1"}),
            &events,
        );
        emit_subagent_update(
            &json!({"kind":"upsert","subagent":{"id":"broken"}}),
            &events,
        );
        match receiver.try_recv().expect("upsert") {
            PaseoEvent::SubagentUpserted(subagent) => assert_eq!(subagent.status, "completed"),
            other => panic!("expected an upsert, got {other:?}"),
        }
        match receiver.try_recv().expect("timeline") {
            PaseoEvent::TimelineEntry(entry) => {
                assert_eq!(entry.agent_id, subagent_timeline_id("agent-1", "task-1"));
                assert_eq!(entry.sequence, 7);
            }
            other => panic!("expected a timeline entry, got {other:?}"),
        }
        assert!(matches!(
            receiver.try_recv().expect("remove"),
            PaseoEvent::SubagentRemoved { subagent_id, .. } if subagent_id == "task-1"
        ));
        assert!(
            receiver.try_recv().is_err(),
            "an unreadable subagent emits nothing"
        );
    }

    #[tokio::test]
    async fn fork_context_and_creation_attachments_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "agent.fork_context.response",
                json!({"agentId":"agent-1","attachment":{"type":"text","mimeType":"text/plain","contextKind":"chat_history","title":"Task","text":"history"},"itemCount":3,"boundaryMessageId":null,"error":null}),
            ),
            (
                "agent.fork_context.response",
                json!({"agentId":"agent-1","attachment":null,"itemCount":0,"boundaryMessageId":null,"error":"nothing to fork"}),
            ),
            (
                "agent.create.response",
                json!({"agent":agent(),"error":null}),
            ),
        ]);
        let attachment = session.fork_context("agent-1").await.expect("fork context");
        assert_eq!(attachment["contextKind"], "chat_history");
        let error = session
            .fork_context("agent-1")
            .await
            .expect_err("fork failure");
        assert!(error.to_string().contains("nothing to fork"));
        session
            .create(CreateAgent {
                provider: "codex".into(),
                model: None,
                directory: "/tmp/project".into(),
                title: None,
                initial_prompt: Some("continue".into()),
                mode_id: None,
                thinking_option_id: None,
                images: vec![ImageAttachment {
                    data_base64: "aGk=".into(),
                    mime_type: "image/png".into(),
                }],
                attachments: vec![attachment],
                worktree: None,
                idempotency_key: "key".into(),
                workspace_id: None,
                feature_values: BTreeMap::new(),
            })
            .await
            .expect("create");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "agent.fork_context.request");
        assert_eq!(messages[0]["agentId"], "agent-1");
        assert_eq!(messages[2]["attachments"][0]["text"], "history");
        assert_eq!(
            messages[2]["images"],
            json!([{"data":"aGk=","mimeType":"image/png"}])
        );
    }

    #[tokio::test]
    async fn lifecycle_requests_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "agent_archived",
                json!({"agentId":"agent-1","archivedAt":"now"}),
            ),
            (
                "status",
                json!({"status":"agent_refreshed","agentId":"agent-1"}),
            ),
            ("agent_deleted", json!({"agentId":"agent-1"})),
            (
                "update_agent_response",
                json!({"agentId":"agent-1","accepted":true,"error":null}),
            ),
            (
                "update_agent_response",
                json!({"agentId":"agent-1","accepted":false,"error":"title too long"}),
            ),
            (
                "clear_agent_attention_response",
                json!({"agentId":["agent-1","agent-2"],"agents":[]}),
            ),
            (
                "status",
                json!({"status":"agent_resumed","agentId":"agent-1"}),
            ),
        ]);
        session.archive("agent-1").await.expect("archive");
        session.unarchive("agent-1").await.expect("unarchive");
        session.delete("agent-1").await.expect("delete");
        session.rename("agent-1", "New name").await.expect("rename");
        let error = session
            .rename("agent-1", "Too long")
            .await
            .expect_err("rejected rename");
        assert!(error.to_string().contains("title too long"));
        session
            .clear_attention(vec!["agent-1".into(), "agent-2".into()])
            .await
            .expect("clear attention");
        session
            .unarchive("agent-1")
            .await
            .expect_err("unexpected status");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "archive_agent_request");
        assert_eq!(messages[0]["agentId"], "agent-1");
        assert_eq!(messages[1]["type"], "refresh_agent_request");
        assert_eq!(messages[1]["agentId"], "agent-1");
        assert_eq!(messages[2]["type"], "delete_agent_request");
        assert_eq!(messages[3]["type"], "update_agent_request");
        assert_eq!(messages[3]["name"], "New name");
        assert_eq!(messages[5]["type"], "clear_agent_attention");
        assert_eq!(messages[5]["agentId"], json!(["agent-1", "agent-2"]));
    }

    #[tokio::test]
    async fn runtime_config_requests_surface_rejections() {
        let accepted = json!({"agentId":"agent-1","accepted":true,"error":null});
        let (session, daemon) = mock_daemon(vec![
            ("set_agent_mode_response", accepted.clone()),
            ("set_agent_model_response", accepted.clone()),
            ("set_agent_thinking_response", accepted.clone()),
            (
                "set_agent_thinking_response",
                json!({"agentId":"agent-1","accepted":false,"error":"unsupported option"}),
            ),
        ]);
        session.set_mode("agent-1", "plan").await.expect("mode");
        session.set_model("agent-1", None).await.expect("model");
        session
            .set_thinking("agent-1", Some("high"))
            .await
            .expect("thinking");
        let error = session
            .set_thinking("agent-1", Some("max"))
            .await
            .expect_err("rejected thinking");
        assert!(error.to_string().contains("unsupported option"));
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "set_agent_mode_request");
        assert_eq!(messages[0]["modeId"], "plan");
        assert_eq!(messages[1]["type"], "set_agent_model_request");
        assert!(messages[1]["modelId"].is_null());
        assert!(messages[1].get("modelId").is_some());
        assert_eq!(messages[2]["type"], "set_agent_thinking_request");
        assert_eq!(messages[2]["thinkingOptionId"], "high");
    }

    #[tokio::test]
    async fn permission_responses_carry_allow_and_deny_details() {
        let (session, daemon) = mock_daemon(vec![
            (
                "agent_permission_resolved",
                json!({"requestId":"permission-1"}),
            ),
            (
                "agent_permission_resolved",
                json!({"requestId":"permission-2"}),
            ),
            (
                "agent_permission_resolved",
                json!({"requestId":"permission-3"}),
            ),
        ]);
        session
            .respond_permission(
                "permission-1",
                PermissionResponse::Allow {
                    selected_action_id: Some("implement".into()),
                    updated_input: Some(json!({"answers":{"Color":"Blue"}})),
                },
            )
            .await
            .expect("allow");
        session
            .respond_permission(
                "permission-2",
                PermissionResponse::Deny {
                    selected_action_id: None,
                    message: Some("Dismissed by user".into()),
                },
            )
            .await
            .expect("deny");
        session
            .answer_permission("permission-3", false)
            .await
            .expect("plain deny");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "agent_permission_response");
        assert_eq!(messages[0]["requestId"], "permission-1");
        assert_eq!(
            messages[0]["response"],
            json!({"behavior":"allow","selectedActionId":"implement","updatedInput":{"answers":{"Color":"Blue"}}})
        );
        assert_eq!(
            messages[1]["response"],
            json!({"behavior":"deny","message":"Dismissed by user"})
        );
        assert_eq!(messages[2]["response"], json!({"behavior":"deny"}));
    }

    #[test]
    fn agent_features_parse_toggles_and_selects() {
        let features = protocol::parse_features(&json!([
            {"type":"toggle","id":"fast_mode","label":"Fast","description":"Priority inference","tooltip":"Toggle fast mode","icon":"zap","value":true},
            {"type":"select","id":"verbosity","label":"Verbosity","value":null,"options":[{"id":"low","label":"Low"},{"id":"high","label":"High","description":"More words"}]},
            {"type":"unknown","id":"future"},
            {"type":"toggle","label":"No id"}
        ]));
        assert_eq!(
            features,
            vec![
                AgentFeature {
                    id: "fast_mode".into(),
                    label: "Fast".into(),
                    description: Some("Priority inference".into()),
                    tooltip: Some("Toggle fast mode".into()),
                    icon: Some("zap".into()),
                    kind: AgentFeatureKind::Toggle(true),
                },
                AgentFeature {
                    id: "verbosity".into(),
                    label: "Verbosity".into(),
                    description: None,
                    tooltip: None,
                    icon: None,
                    kind: AgentFeatureKind::Select {
                        value: None,
                        options: vec![
                            AgentFeatureOption {
                                id: "low".into(),
                                label: "Low".into(),
                                description: None,
                            },
                            AgentFeatureOption {
                                id: "high".into(),
                                label: "High".into(),
                                description: Some("More words".into()),
                            },
                        ],
                    },
                },
            ]
        );
    }

    #[tokio::test]
    async fn features_are_set_and_listed_for_drafts() {
        let (session, daemon) = mock_daemon(vec![
            (
                "set_agent_feature_response",
                json!({"agentId":"agent-1","accepted":true,"error":null}),
            ),
            (
                "list_provider_features_response",
                json!({"provider":"codex","features":[{"type":"toggle","id":"plan_mode","label":"Plan","value":false}],"error":null,"fetchedAt":"now"}),
            ),
            (
                "list_provider_features_response",
                json!({"provider":"codex","features":null,"error":"provider unavailable","fetchedAt":"now"}),
            ),
        ]);
        session
            .set_feature("agent-1", "fast_mode", json!(true))
            .await
            .expect("feature set");
        let draft = DraftConfig {
            provider: "codex".into(),
            cwd: "/tmp/project".into(),
            mode_id: None,
            model: Some("gpt-6-luna".into()),
            thinking_option_id: None,
            feature_values: BTreeMap::from([("fast_mode".into(), json!(true))]),
        };
        let features = session
            .provider_features(draft.clone())
            .await
            .expect("draft features");
        assert_eq!(features[0].id, "plan_mode");
        assert!(session.provider_features(draft).await.is_err());
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "set_agent_feature_request");
        assert_eq!(messages[0]["agentId"], "agent-1");
        assert_eq!(messages[0]["featureId"], "fast_mode");
        assert_eq!(messages[0]["value"], true);
        assert_eq!(messages[1]["type"], "list_provider_features_request");
        assert_eq!(
            messages[1]["draftConfig"],
            json!({"provider":"codex","cwd":"/tmp/project","model":"gpt-6-luna","featureValues":{"fast_mode":true}})
        );
    }

    #[tokio::test]
    async fn commands_are_listed_for_agents_and_drafts() {
        let (session, daemon) = mock_daemon(vec![
            (
                "list_commands_response",
                json!({"agentId":"agent-1","commands":[{"name":"review","description":"Review code","argumentHint":"","kind":"skill"}],"error":null}),
            ),
            (
                "list_commands_response",
                json!({"agentId":"draft","commands":[{"name":"init","description":"Initialize","argumentHint":"<path>"}],"error":null}),
            ),
        ]);
        let commands = session
            .list_commands(Some("agent-1"), None)
            .await
            .expect("agent commands");
        assert_eq!(
            commands,
            vec![AgentCommand {
                name: "review".into(),
                description: "Review code".into(),
                argument_hint: None,
                kind: Some("skill".into()),
            }]
        );
        let draft_commands = session
            .list_commands(
                None,
                Some(DraftConfig {
                    provider: "codex".into(),
                    cwd: "/tmp/project".into(),
                    mode_id: Some("plan".into()),
                    model: None,
                    thinking_option_id: Some("high".into()),
                    feature_values: BTreeMap::new(),
                }),
            )
            .await
            .expect("draft commands");
        assert_eq!(draft_commands[0].argument_hint.as_deref(), Some("<path>"));
        assert!(session.list_commands(None, None).await.is_err());
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "list_commands_request");
        assert_eq!(messages[0]["agentId"], "agent-1");
        assert!(messages[0].get("draftConfig").is_none());
        assert!(
            messages[1]["agentId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(
            messages[1]["draftConfig"],
            json!({"provider":"codex","cwd":"/tmp/project","modeId":"plan","thinkingOptionId":"high"})
        );
    }

    #[tokio::test]
    async fn directory_suggestions_use_entries_or_legacy_directories() {
        let (session, daemon) = mock_daemon(vec![
            (
                "directory_suggestions_response",
                json!({"directories":["src"],"entries":[{"path":"src","kind":"directory"},{"path":"src/main.rs","kind":"file"}],"error":null}),
            ),
            (
                "directory_suggestions_response",
                json!({"directories":["/home/user/project"],"error":null}),
            ),
        ]);
        let suggestions = session
            .directory_suggestions("src", Some("/tmp/project"), true, true, 20)
            .await
            .expect("suggestions");
        assert_eq!(
            suggestions,
            vec![
                DirectorySuggestion {
                    path: "src".into(),
                    is_directory: true
                },
                DirectorySuggestion {
                    path: "src/main.rs".into(),
                    is_directory: false
                },
            ]
        );
        let legacy = session
            .directory_suggestions("proj", None, false, true, 10)
            .await
            .expect("legacy suggestions");
        assert_eq!(legacy[0].path, "/home/user/project");
        assert!(legacy[0].is_directory);
        assert!(
            session
                .directory_suggestions("x", None, false, true, 0)
                .await
                .is_err()
        );
        let messages = daemon.await.expect("daemon task");
        assert_eq!(
            messages[0],
            json!({"type":"directory_suggestions_request","requestId":messages[0]["requestId"],"query":"src","cwd":"/tmp/project","includeFiles":true,"includeDirectories":true,"limit":20})
        );
        assert!(messages[1].get("cwd").is_none());
    }

    #[tokio::test]
    async fn agent_history_fetches_one_page_with_search_and_cursor() {
        let archived = json!({"id":"archived-1", "status":"closed", "cwd":"/tmp/project", "archivedAt":"2026-09-01T00:00:00Z"});
        let (session, daemon) = mock_daemon(vec![
            (
                "fetch_agent_history_response",
                json!({"entries":[{"agent":agent(),"project":{"projectKey":"p","projectName":"Project","workspaceName":"Fix login"}},{"agent":archived,"project":null}],"pageInfo":{"hasMore":true,"nextCursor":"next","prevCursor":null}}),
            ),
            (
                "fetch_agent_history_response",
                json!({"entries":[],"pageInfo":{"hasMore":false,"nextCursor":null,"prevCursor":null},"searchTruncated":true}),
            ),
        ]);
        let first = session
            .agent_history("", None)
            .await
            .expect("first history page");
        assert_eq!(
            first
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["agent-1", "archived-1"]
        );
        assert_eq!(first.next_cursor.as_deref(), Some("next"));
        assert!(!first.search_truncated);
        let second = session
            .agent_history("login", Some("next".into()))
            .await
            .expect("second history page");
        assert!(second.agents.is_empty());
        assert_eq!(second.next_cursor, None);
        assert!(second.search_truncated);

        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "fetch_agent_history_request");
        assert_eq!(messages[0].get("filter"), None);
        assert_eq!(messages[0].get("search"), None);
        assert_eq!(
            messages[0]["sort"],
            json!([{"key":"updated_at","direction":"desc"}])
        );
        assert_eq!(messages[0]["page"], json!({"limit":200}));
        assert_eq!(messages[1]["search"], "login");
        assert_eq!(messages[1]["page"], json!({"limit":200,"cursor":"next"}));
    }

    #[tokio::test]
    async fn send_message_can_steer_with_images() {
        let (session, daemon) = mock_daemon(vec![
            (
                "send_agent_message_response",
                json!({"agentId":"agent-1","accepted":true,"error":null}),
            ),
            (
                "send_agent_message_response",
                json!({"agentId":"agent-1","accepted":true,"error":null}),
            ),
        ]);
        session
            .send_message(SendMessage {
                agent_id: "agent-1".into(),
                text: "also check tests".into(),
                message_id: "message-2".into(),
                behavior: Some(ActiveTurnBehavior::Steer),
                images: vec![ImageAttachment {
                    data_base64: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                }],
                attachments: vec![json!({"type":"uploaded_file","id":"file-1","fileName":"notes.txt","mimeType":"text/plain","size":2,"path":"/uploads/notes.txt"})],
            })
            .await
            .expect("steer");
        session
            .send("agent-1", "plain", "message-3")
            .await
            .expect("plain send");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["type"], "send_agent_message_request");
        assert_eq!(messages[0]["messageId"], "message-2");
        assert_eq!(messages[0]["activeTurnBehavior"], "steer");
        assert_eq!(
            messages[0]["images"],
            json!([{"data":"aGVsbG8=","mimeType":"image/png"}])
        );
        assert_eq!(messages[0]["attachments"][0]["path"], "/uploads/notes.txt");
        assert!(messages[1].get("activeTurnBehavior").is_none());
        assert!(messages[1].get("images").is_none());
        assert!(messages[1].get("attachments").is_none());
    }

    #[tokio::test]
    async fn file_upload_sends_request_then_frames() {
        let (commands, mut receiver) = mpsc::channel(8);
        let session = PaseoSession { commands };
        let bytes = vec![7u8; FILE_CHUNK_SIZE + 3];
        let upload = tokio::spawn(async move {
            session
                .upload_file(FileUpload {
                    file_name: "notes.txt".into(),
                    mime_type: "text/plain".into(),
                    modified_at: "2026-09-30T10:00:00Z".into(),
                    bytes,
                })
                .await
        });
        let Some(Command::Request {
            message,
            response_type,
            reply,
            ..
        }) = receiver.recv().await
        else {
            panic!("expected the upload request first")
        };
        assert_eq!(message["type"], "file.upload.request");
        assert_eq!(response_type, "file.upload.response");
        assert_eq!(message["fileName"], "notes.txt");
        assert_eq!(message["size"], FILE_CHUNK_SIZE + 3);
        let request_id = message["requestId"]
            .as_str()
            .expect("request ID")
            .to_owned();
        let mut frames = Vec::new();
        for _ in 0..4 {
            let Some(Command::Binary(frame)) = receiver.recv().await else {
                panic!("expected a file frame")
            };
            frames.push(frame);
        }
        let id = request_id.as_bytes();
        let header = |opcode: u8| [&[opcode, id.len() as u8][..], id].concat();
        assert!(frames[0].starts_with(&header(16)));
        let metadata_start = header(16).len() + 2;
        let metadata: Value =
            serde_json::from_slice(&frames[0][metadata_start..]).expect("begin metadata");
        assert_eq!(
            metadata,
            json!({"mime":"text/plain","size":FILE_CHUNK_SIZE + 3,"encoding":"binary","modifiedAt":"2026-09-30T10:00:00Z","fileName":"notes.txt"})
        );
        assert_eq!(
            frames[1].len() - header(17).len(),
            FILE_CHUNK_SIZE,
            "full first chunk"
        );
        assert!(frames[1].starts_with(&header(17)));
        assert_eq!(frames[2], [header(17), vec![7, 7, 7]].concat());
        assert_eq!(frames[3], header(18));
        deliver(
            reply,
            Ok(
                json!({"requestId":request_id,"file":{"type":"uploaded_file","id":"file-1","fileName":"notes.txt","mimeType":"text/plain","size":FILE_CHUNK_SIZE + 3,"path":"/uploads/notes.txt"},"error":null}),
            ),
        );
        let file = upload.await.expect("upload task").expect("uploaded");
        assert_eq!(file.path, "/uploads/notes.txt");
        assert_eq!(file.attachment()["type"], "uploaded_file");
    }

    #[tokio::test]
    async fn status_reply_completes_matching_request_only() {
        let (reply, response) = oneshot::channel();
        let mut pending = HashMap::from([(
            "refresh-1".to_string(),
            Pending {
                replay: None,
                sends_message: false,
                response_type: "status",
                reply,
            },
        )]);
        let (events, _) = async_channel::unbounded();
        handle_message(
            &json!({"type":"status", "payload":{"status":"agent_refreshed", "agentId":"agent-1", "requestId":"refresh-1", "timelineSize":3}}),
            &mut pending,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &events,
            &mut TimelineSubscriptions::default(),
        );
        let payload = response.await.expect("reply").expect("status payload");
        assert_eq!(payload["status"], "agent_refreshed");
        assert!(pending.is_empty());
    }

    #[test]
    fn agent_updates_carry_project_placement() {
        let (events, receiver) = async_channel::unbounded();
        let mut agents = HashMap::new();
        handle_message(
            &json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":agent(), "project":{"projectKey":"p","projectName":"Project"}}}),
            &mut HashMap::new(),
            &mut agents,
            &mut HashMap::new(),
            &events,
            &mut TimelineSubscriptions::default(),
        );
        assert_eq!(
            agents["agent-1"].project.as_ref().expect("project")["projectName"],
            "Project"
        );
        handle_message(
            &json!({"type":"agent_update", "payload":{"kind":"upsert", "agent":agent(), "project":null}}),
            &mut HashMap::new(),
            &mut agents,
            &mut HashMap::new(),
            &events,
            &mut TimelineSubscriptions::default(),
        );
        assert_eq!(agents["agent-1"].project, None);
        assert!(matches!(
            receiver.try_recv(),
            Ok(PaseoEvent::AgentUpserted(_))
        ));
    }

    #[tokio::test]
    async fn terminal_frames_route_by_subscription_slot() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock daemon");
        let port = listener.local_addr().expect("mock address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = server_socket(stream).await;
            let subscribe = next_request(&mut socket, "subscribe_terminal_request").await;
            assert_eq!(subscribe["restore"]["mode"], "visible-snapshot");
            assert_eq!(subscribe["restore"]["size"], json!({"rows":24,"cols":80}));
            send_json(&mut socket, json!({"type":"session", "message":{"type":"subscribe_terminal_response", "payload":{"requestId":subscribe["requestId"],"terminalId":"terminal-1","slot":3,"subscriptionId":"subscription-1","error":null}}})).await;
            socket
                .send(Message::Binary(vec![0x05, 3, b'h', b'i'].into()))
                .await
                .expect("send restore");
            socket
                .send(Message::Binary(vec![0x01, 9, b'x'].into()))
                .await
                .expect("send unknown slot");
            socket
                .send(Message::Binary(vec![0x01, 3, b'\n'].into()))
                .await
                .expect("send output");
            let input = next_request(&mut socket, "terminal_input").await;
            assert_eq!(input["message"], json!({"type":"input","data":"ls\r"}));
            assert!(input.get("requestId").is_none());
            let release = next_request(&mut socket, "subscription.release.request").await;
            assert_eq!(release["subscriptionId"], "subscription-1");
            socket
                .send(Message::Binary(vec![0x01, 3, b'z'].into()))
                .await
                .expect("send output after release");
            send_json(&mut socket, json!({"type":"session", "message":{"type":"terminal_stream_exit", "payload":{"terminalId":"terminal-1"}}})).await;
        });
        let (session, events) = connect(target(port), None, "test-client".into())
            .await
            .expect("connect");
        let subscription = session
            .subscribe_terminal("terminal-1", 24, 80)
            .await
            .expect("subscribe terminal");
        assert_eq!(subscription.as_deref(), Some("subscription-1"));
        let mut outputs = Vec::new();
        while outputs.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("terminal output timed out")
                .expect("event receiver closed");
            if let PaseoEvent::TerminalOutput {
                terminal_id,
                bytes,
                restore,
            } = event
            {
                outputs.push((terminal_id, bytes, restore));
            }
        }
        assert_eq!(
            outputs,
            vec![
                ("terminal-1".to_owned(), b"hi".to_vec(), true),
                ("terminal-1".to_owned(), b"\n".to_vec(), false),
            ]
        );
        session
            .terminal_input("terminal-1", "ls\r".into())
            .await
            .expect("terminal input");
        session
            .release_terminal("terminal-1", subscription.as_deref())
            .await
            .expect("release terminal");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("terminal exit timed out")
                .expect("event receiver closed");
            match event {
                PaseoEvent::TerminalExited { terminal_id, error } => {
                    assert_eq!((terminal_id.as_str(), error), ("terminal-1", None));
                    break;
                }
                PaseoEvent::TerminalOutput { .. } => panic!("output routed after release"),
                _ => {}
            }
        }
        server.await.expect("mock daemon task");
    }

    #[tokio::test]
    async fn usage_rewind_and_checkout_requests_use_daemon_shapes() {
        let (session, daemon) = mock_daemon(vec![
            (
                "provider.usage.list.response",
                json!({"fetchedAt":"2026-09-26T10:00:00Z","providers":[{"providerId":"claude","displayName":"Claude","status":"available","planLabel":"Max","windows":[{"id":"5h","label":"5-hour","usedPct":42,"resetsAt":"2026-09-26T12:00:00Z"},{"id":"week","label":"Weekly","remainingPct":75}],"balances":[{"id":"credits","label":"Credits","remaining":12.5,"unit":"usd"}],"details":[{"id":"org","label":"Org","value":"Acme"}]}]}),
            ),
            (
                "agent.rewind.response",
                json!({"agentId":"agent-1","ok":false,"error":"Cannot rewind before the provider acknowledges the submitted prompt"}),
            ),
            (
                "checkout_status_response",
                json!({"cwd":"/tmp/project","isGit":true,"isPaseoOwnedWorktree":false,"repoRoot":"/tmp/project","currentBranch":"main","isDirty":true,"baseRef":"origin/main","aheadBehind":{"ahead":2,"behind":1},"aheadOfOrigin":2,"behindOfOrigin":0,"hasRemote":true,"remoteUrl":null,"error":null}),
            ),
            (
                "checkout.diff.get.response",
                json!({"cwd":"/tmp/project","files":[{"path":"src/main.rs","isNew":false,"isDeleted":false,"additions":1,"deletions":1,"hunks":[{"oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"lines":[{"type":"remove","content":"old"},{"type":"add","content":"new"}]}]},{"path":"logo.png","isNew":true,"isDeleted":false,"additions":0,"deletions":0,"hunks":[],"status":"binary"}],"error":null}),
            ),
            (
                "checkout_commit_response",
                json!({"cwd":"/tmp/project","success":false,"error":{"code":"UNKNOWN","message":"nothing to commit"}}),
            ),
        ]);
        let usage = session.provider_usage().await.expect("usage");
        assert_eq!(usage[0].plan_label.as_deref(), Some("Max"));
        assert_eq!(usage[0].windows[0].used_percent, Some(42.0));
        assert_eq!(usage[0].windows[1].used_percent, Some(25.0));
        assert_eq!(usage[0].balances[0].remaining, Some(12.5));
        assert_eq!(usage[0].details[0].value, "Acme");
        let rewind = session
            .rewind("agent-1", "message-1", RewindMode::Both)
            .await
            .expect_err("rewind refused");
        assert!(rewind.to_string().contains("acknowledges"));
        let status = session
            .checkout_status("/tmp/project")
            .await
            .expect("status");
        assert_eq!(
            (
                status.current_branch.as_deref(),
                status.ahead_of_base,
                status.behind_base
            ),
            (Some("main"), 2, 1)
        );
        let diff = session
            .checkout_diff("/tmp/project", DiffCompare::Base)
            .await
            .expect("diff");
        assert_eq!(diff.files[0].hunks[0].lines[1].kind, DiffLineKind::Added);
        assert_eq!(diff.files[1].status.as_deref(), Some("binary"));
        let commit = session
            .commit("/tmp/project", "  ")
            .await
            .expect_err("commit refused");
        assert!(commit.to_string().contains("nothing to commit"));
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[1]["messageId"], "message-1");
        assert_eq!(messages[1]["mode"], "both");
        assert_eq!(messages[3]["compare"], json!({"mode":"base"}));
        assert_eq!(messages[4]["addAll"], true);
        assert!(messages[4].get("message").is_none());
    }

    #[tokio::test]
    async fn create_joins_the_given_workspace() {
        let (session, daemon) = mock_daemon(vec![(
            "agent.create.response",
            json!({"agent":agent(),"error":null}),
        )]);
        session
            .create(CreateAgent {
                provider: "codex".into(),
                model: None,
                directory: "/tmp/project".into(),
                title: None,
                initial_prompt: Some("go".into()),
                mode_id: None,
                thinking_option_id: None,
                images: Vec::new(),
                attachments: Vec::new(),
                worktree: None,
                idempotency_key: "key".into(),
                workspace_id: Some("wks_0123456789abcdef".into()),
                feature_values: BTreeMap::new(),
            })
            .await
            .expect("create");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(messages[0]["workspaceId"], "wks_0123456789abcdef");
    }

    #[tokio::test]
    async fn worktree_creation_branches_off() {
        let (session, daemon) = mock_daemon(vec![(
            "agent.create.response",
            json!({"agent":agent(),"error":null}),
        )]);
        session
            .create(CreateAgent {
                provider: "codex".into(),
                model: None,
                directory: "/tmp/project".into(),
                title: None,
                initial_prompt: Some("go".into()),
                mode_id: None,
                thinking_option_id: None,
                images: Vec::new(),
                attachments: Vec::new(),
                worktree: Some(WorktreeTarget {
                    new_branch: "fix-login".into(),
                    base: Some("refs/remotes/origin/main".into()),
                }),
                idempotency_key: "key".into(),
                workspace_id: None,
                feature_values: BTreeMap::new(),
            })
            .await
            .expect("create");
        let messages = daemon.await.expect("daemon task");
        assert_eq!(
            messages[0]["worktree"],
            json!({"mode":"branch-off","newBranch":"fix-login","base":"refs/remotes/origin/main"})
        );
    }

    #[tokio::test]
    async fn read_file_decodes_base64_from_the_filesystem_root() {
        let (session, daemon) = mock_daemon(vec![
            (
                "file_explorer_response",
                json!({"cwd":"/","path":"tmp/a.png","mode":"file","directory":null,"file":{"path":"tmp/a.png","kind":"image","encoding":"base64","content":"iVBORw==","mimeType":"image/png","size":4},"error":null}),
            ),
            (
                "file_explorer_response",
                json!({"cwd":"/","path":"tmp/gone.png","mode":"file","directory":null,"file":null,"error":"File not found"}),
            ),
        ]);
        let file = session.read_file("/tmp/a.png").await.expect("file");
        assert_eq!(
            file,
            FileContent {
                bytes: vec![0x89, b'P', b'N', b'G'],
                mime_type: "image/png".into(),
            }
        );
        let missing = session
            .read_file("/tmp/gone.png")
            .await
            .expect_err("missing file");
        assert!(missing.to_string().contains("File not found"));
        assert!(session.read_file("relative.png").await.is_err());
        let messages = daemon.await.expect("daemon task");
        assert_eq!(
            (
                &messages[0]["cwd"],
                &messages[0]["path"],
                &messages[0]["mode"]
            ),
            (&json!("/"), &json!("tmp/a.png"), &json!("file"))
        );
        assert!(messages[0].get("acceptBinary").is_none());
    }

    #[tokio::test]
    async fn branch_suggestions_read_details_and_fall_back_to_names() {
        let (session, daemon) = mock_daemon(vec![
            (
                "branch_suggestions_response",
                json!({"branches":["main"],"branchDetails":[{"name":"main","committerDate":1790537870,"hasLocal":true,"hasRemote":true,"localAhead":0,"localBehind":9}],"error":null}),
            ),
            (
                "branch_suggestions_response",
                json!({"branches":["legacy"],"error":null}),
            ),
            (
                "checkout_status_response",
                json!({"cwd":"/tmp/project","isGit":true,"currentBranch":"feature","upstreamRef":"refs/remotes/upstream/feature","error":null}),
            ),
        ]);
        let detailed = session
            .branch_suggestions("/tmp/project", "  ", 20)
            .await
            .expect("suggestions");
        assert_eq!(
            detailed,
            vec![BranchSuggestion {
                name: "main".into(),
                committer_date: Some(1790537870),
                has_local: Some(true),
                has_remote: Some(true),
                local_ahead: Some(0),
                local_behind: Some(9),
            }]
        );
        let legacy = session
            .branch_suggestions("/tmp/project", "leg", 20)
            .await
            .expect("legacy suggestions");
        assert_eq!(legacy[0].name, "legacy");
        assert_eq!(legacy[0].has_remote, None);
        let status = session
            .checkout_status("/tmp/project")
            .await
            .expect("status");
        assert_eq!(
            status.upstream_ref.as_deref(),
            Some("refs/remotes/upstream/feature")
        );
        let messages = daemon.await.expect("daemon task");
        assert!(messages[0].get("query").is_none());
        assert_eq!(messages[0]["limit"], 20);
        assert_eq!(messages[1]["query"], "leg");
    }
}
