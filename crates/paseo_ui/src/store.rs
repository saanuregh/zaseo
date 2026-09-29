use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use gpui::{App, Context, EventEmitter, Task, TaskExt};
use gpui_tokio::Tokio;
use paseo_client::{
    AgentCommand, AgentSummary, CreateAgent, DirectorySuggestion, DraftConfig, PaseoEvent,
    PaseoSession, PermissionRequest, PermissionResponse, ProjectDescriptor, Provider,
    ProviderSubagent, RecoveryState, RuntimePassword, SendMessage, ServerInfo, SetupSnapshot,
    TerminalInfo, TimelineCursor, TimelineEntry, TimelinePage, WorkspaceDescriptor, WorkspaceLabel,
    parse_subagent_timeline_id,
};
use serde_json::Value;
use settings::PaseoConnectionProfile;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use util::ResultExt as _;

use crate::connection_picker;
use crate::timeline::parse_optional_timestamp;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

const ARCHIVED_SUBAGENTS_KEY: &str = "paseo_archived_subagents";
const REVIEWED_EDITS_KEY: &str = "paseo_reviewed_edits";

#[derive(Default)]
pub(crate) struct AgentPaging {
    pub older_cursor: Option<TimelineCursor>,
    pub has_older: bool,
    pub loading_older: bool,
    pub loaded: bool,
}

pub enum StoreEvent {
    /// An agent the user is not looking at finished, failed, or needs input.
    NeedsAttention { agent_id: String, message: String },
    /// Terminal and dictation streams, handled by the view that started them.
    Stream(PaseoEvent),
    /// The agent the user is looking at changed.
    FocusChanged,
    /// An entry arrived on this agent's or subagent's timeline. Sent instead of a notify,
    /// because streamed chunks are the most frequent event and only the views showing
    /// that timeline read it.
    TimelineChanged(String),
}

#[derive(Default)]
pub struct PaseoStore {
    pub(crate) state: StoreState,
    pub(crate) session: Option<Arc<PaseoSession>>,
    pub(crate) providers: Vec<Provider>,
    pub(crate) status: ConnectionStatus,
    pub(crate) active_profile: Option<PaseoConnectionProfile>,
    pub(crate) connection_generation: u64,
    pub(crate) paging: HashMap<String, AgentPaging>,
    /// Subagent timeline IDs the user archived from the subagent track, saved across restarts.
    pub(crate) archived_subagents: BTreeSet<String>,
    /// Parents whose subagent list is being fetched, so a newly opened chat can wait for it.
    pub(crate) subagents_loading: HashSet<String>,
    /// Agent edits kept or rejected in an editor, so they stay reviewed across restarts.
    pub(crate) reviewed_edits: BTreeSet<String>,
    pub(crate) archived: Option<Vec<AgentSummary>>,
    /// Whether each archived agent's workspace can be restored, by workspace ID.
    pub(crate) recovery: HashMap<String, RecoveryState>,
    /// Workspaces whose setup state was asked for on this connection.
    setup_checked: HashSet<String>,
    pub(crate) commands: HashMap<String, Vec<AgentCommand>>,
    pub(crate) focused_agent: Option<String>,
    /// The latest phase of a daemon update this client started.
    pub(crate) daemon_update_phase: Option<String>,
    /// Project icons by project ID, with the icon revision they were loaded for.
    pub(crate) project_icons: HashMap<String, (Option<String>, Option<Arc<gpui::Image>>)>,
    pub(crate) server_info: ServerInfo,
    /// Daemon terminals per directory, for directories some view watches.
    pub(crate) terminals: HashMap<String, Vec<TerminalInfo>>,
    terminal_watchers: BTreeMap<String, usize>,
    /// The daemon's subscription for each watched directory's terminal list.
    terminal_list_subscriptions: HashMap<String, String>,
    /// Counts `Connected` events, including automatic reconnects, which drop every
    /// daemon-side stream subscription.
    pub(crate) connection_count: u64,
    /// Images this app sent, by message ID. The daemon's timeline keeps only a message's text,
    /// so like Paseo's app, only the sender can show them.
    pub(crate) sent_images: HashMap<String, Vec<Arc<gpui::Image>>>,
    watchers: BTreeMap<String, usize>,
    /// Passwords typed this session, so reconnects and host switches can reuse them. Never saved.
    session_passwords: HashMap<String, String>,
    connection_task: Option<Task<()>>,
    event_task: Option<Task<()>>,
}

impl EventEmitter<StoreEvent> for PaseoStore {}

impl PaseoStore {
    /// Whether agent directories are on this machine, so their files can open in the editor.
    pub(crate) fn is_local_host(&self) -> bool {
        self.active_profile.as_ref().is_some_and(|profile| {
            url::Url::parse(&profile.target_uri)
                .ok()
                .is_some_and(|url| {
                    matches!(url.scheme(), "ws" | "wss")
                        && url.host_str().is_some_and(|host| {
                            host.eq_ignore_ascii_case("localhost")
                                || host
                                    .trim_matches(['[', ']'])
                                    .parse::<std::net::IpAddr>()
                                    .is_ok_and(|address| address.is_loopback())
                        })
                })
        })
    }

    fn buckets(&self) -> HashMap<String, AgentBucket> {
        self.state
            .agents
            .iter()
            .map(|agent| {
                let pending = self
                    .state
                    .permissions
                    .values()
                    .any(|request| request.agent_id == agent.id);
                (agent.id.clone(), agent_bucket(agent, pending))
            })
            .collect()
    }

    fn announce_transitions(&self, before: &HashMap<String, AgentBucket>, cx: &mut Context<Self>) {
        for (agent_id, bucket) in self.buckets() {
            let Some(previous) = before.get(&agent_id).copied() else {
                continue;
            };
            if previous == bucket || self.focused_agent.as_deref() == Some(agent_id.as_str()) {
                continue;
            }
            let Some(title) = self.agent(&agent_id).map(agent_title) else {
                continue;
            };
            let message = match bucket {
                AgentBucket::NeedsInput => format!("“{title}” needs your input"),
                AgentBucket::Failed => format!("“{title}” failed"),
                AgentBucket::Attention | AgentBucket::Done if previous == AgentBucket::Running => {
                    format!("“{title}” finished")
                }
                _ => continue,
            };
            cx.emit(StoreEvent::NeedsAttention { agent_id, message });
        }
    }

    pub(crate) fn connected(&self) -> bool {
        self.status == ConnectionStatus::Connected
    }

    pub(crate) fn is_current_connection(&self, generation: u64) -> bool {
        self.connection_generation == generation
    }

    pub(crate) fn archive_subagents(
        &mut self,
        timeline_ids: impl IntoIterator<Item = String>,
        cx: &mut Context<Self>,
    ) {
        self.archived_subagents.extend(timeline_ids);
        self.save_archived_subagents(cx);
        cx.notify();
    }

    fn save_archived_subagents(&self, cx: &App) {
        let json = match serde_json::to_string(&self.archived_subagents) {
            Ok(json) => json,
            Err(error) => {
                log::error!("Failed to serialize archived Paseo subagents: {error}");
                return;
            }
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            kvp.write_kvp(ARCHIVED_SUBAGENTS_KEY.to_string(), json)
                .await
        });
    }

    pub(crate) fn mark_edits_reviewed(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        let before = self.reviewed_edits.len();
        self.reviewed_edits.extend(keys);
        if self.reviewed_edits.len() == before {
            return;
        }
        let json = match serde_json::to_string(&self.reviewed_edits) {
            Ok(json) => json,
            Err(error) => {
                log::error!("Failed to serialize reviewed Paseo edits: {error}");
                return;
            }
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            kvp.write_kvp(REVIEWED_EDITS_KEY.to_string(), json).await
        });
        cx.notify();
    }

    pub(crate) fn load_reviewed_edits(&mut self, cx: &App) {
        self.reviewed_edits = db::kvp::KeyValueStore::global(cx)
            .read_kvp(REVIEWED_EDITS_KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default();
    }

    pub(crate) fn load_archived_subagents(&mut self, cx: &App) {
        self.archived_subagents = db::kvp::KeyValueStore::global(cx)
            .read_kvp(ARCHIVED_SUBAGENTS_KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
            .unwrap_or_default();
    }

    /// The subagent a `subagent_timeline_id` names, once its parent's list has loaded.
    pub(crate) fn subagent(&self, timeline_id: &str) -> Option<&ProviderSubagent> {
        let (parent_agent_id, subagent_id) = parse_subagent_timeline_id(timeline_id)?;
        self.state
            .subagents_for(parent_agent_id)
            .iter()
            .find(|subagent| subagent.id == subagent_id)
    }

    /// The directory an agent's or subagent's paths are relative to. A subagent without its own
    /// directory works in its parent's.
    pub(crate) fn timeline_directory(&self, timeline_id: &str) -> Option<PathBuf> {
        match parse_subagent_timeline_id(timeline_id) {
            Some((parent_agent_id, _)) => self
                .subagent(timeline_id)
                .and_then(|subagent| subagent.cwd.as_deref())
                .filter(|cwd| paseo_client::is_absolute_workspace_path(cwd))
                .map(PathBuf::from)
                .or_else(|| self.agent(parent_agent_id)?.directory.clone()),
            None => self.agent(timeline_id)?.directory.clone(),
        }
    }

    pub(crate) fn agent(&self, agent_id: &str) -> Option<&AgentSummary> {
        self.state
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .or_else(|| {
                self.archived
                    .iter()
                    .flatten()
                    .find(|agent| agent.id == agent_id)
            })
    }

    pub(crate) fn provider(&self, provider_id: &str) -> Option<&Provider> {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)
    }

    pub(crate) fn entries_for<'a>(
        &'a self,
        agent_id: &'a str,
    ) -> impl Iterator<Item = &'a TimelineEntry> + 'a {
        let epoch = self
            .state
            .current_epoch(agent_id)
            .unwrap_or_default()
            .to_owned();
        let start = (agent_id.to_owned(), epoch.clone(), 0);
        let end = (agent_id.to_owned(), epoch, u64::MAX);
        self.state
            .timeline
            .range(start..=end)
            .map(|(_, entry)| entry)
    }

    pub(crate) fn permissions_for(&self, agent_id: &str) -> Vec<PermissionRequest> {
        self.state
            .permissions
            .values()
            .filter(|request| request.agent_id == agent_id)
            .cloned()
            .collect()
    }

    pub(crate) fn begin_connection(&mut self, profile: PaseoConnectionProfile) -> u64 {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.status = ConnectionStatus::Connecting;
        self.connection_task = None;
        self.event_task = None;
        self.session = None;
        self.providers.clear();
        self.state.clear_for_connection();
        self.paging.clear();
        self.archived = None;
        self.recovery.clear();
        self.setup_checked.clear();
        self.project_icons.clear();
        self.commands.clear();
        self.terminals.clear();
        self.terminal_list_subscriptions.clear();
        self.server_info = ServerInfo::default();
        self.active_profile = Some(profile);
        self.connection_generation
    }

    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            Tokio::spawn_result(cx, async move { session.close().await }).detach();
        }
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.status = ConnectionStatus::Disconnected;
        self.connection_task = None;
        self.event_task = None;
        self.providers.clear();
        self.state.clear_for_connection();
        self.paging.clear();
        self.archived = None;
        cx.notify();
    }

    pub(crate) fn apply_refresh(
        &mut self,
        generation: u64,
        providers: Vec<Provider>,
        agents: Vec<AgentSummary>,
    ) -> bool {
        if !self.is_current_connection(generation) {
            return false;
        }
        self.providers = providers;
        self.state.set_agents(agents);
        true
    }

    pub(crate) fn apply_older_page(
        &mut self,
        generation: u64,
        agent_id: &str,
        requested_cursor: &TimelineCursor,
        page: TimelinePage,
    ) -> bool {
        let paging = self.paging.entry(agent_id.to_owned()).or_default();
        paging.loading_older = false;
        if self.connection_generation != generation
            || paging.older_cursor.as_ref() != Some(requested_cursor)
            || self
                .state
                .current_epoch(agent_id)
                .is_some_and(|epoch| epoch != requested_cursor.epoch)
            || page.epoch != requested_cursor.epoch
            || page
                .entries
                .iter()
                .any(|entry| entry.agent_id != agent_id || entry.epoch != requested_cursor.epoch)
            || page
                .start_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.epoch != requested_cursor.epoch)
        {
            return false;
        }
        for entry in page.entries {
            self.state.insert_projected_entry(entry);
        }
        paging.older_cursor = page.start_cursor;
        paging.has_older = page.has_older;
        true
    }

    pub(crate) fn connect(
        &mut self,
        profile: PaseoConnectionProfile,
        password: Option<String>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if !self.is_current_connection(generation) {
            return;
        }
        let target = match connection_picker::parse_target(&profile) {
            Ok(target) => target,
            Err(error) => {
                self.status = ConnectionStatus::Disconnected;
                self.state.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let password = password.filter(|password| !password.is_empty());
        if let Some(password) = &password {
            self.session_passwords
                .insert(profile.name.clone(), password.clone());
        }
        let password = password
            .or_else(|| self.session_passwords.get(&profile.name).cloned())
            .map(RuntimePassword::new);
        let connection = Tokio::spawn_result(
            cx,
            paseo_client::connect(target, password, profile.client_id),
        );
        self.connection_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok((session, events)) => {
                        store.session = Some(Arc::new(session));
                        store.status = ConnectionStatus::Connected;
                        store.state.error = None;
                        store.event_task = Some(cx.spawn(async move |this, cx| {
                            while let Ok(event) = events.recv().await {
                                if this
                                    .update(cx, |store, cx| {
                                        if store.is_current_connection(generation) {
                                            store.handle_event(event, cx);
                                        }
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }));
                        store.sync_subscriptions(cx);
                    }
                    Err(error) => {
                        store.status = ConnectionStatus::Disconnected;
                        store.state.error = Some(error.to_string());
                    }
                }
                cx.notify();
            }) {
                log::debug!("Paseo store released before connection update: {error}");
            }
        }));
        cx.notify();
    }

    fn handle_event(&mut self, event: PaseoEvent, cx: &mut Context<Self>) {
        if matches!(
            event,
            PaseoEvent::TerminalOutput { .. }
                | PaseoEvent::TerminalExited { .. }
                | PaseoEvent::DictationPartial { .. }
                | PaseoEvent::DictationFinal { .. }
                | PaseoEvent::DictationFailed { .. }
        ) {
            cx.emit(StoreEvent::Stream(event));
            return;
        }
        // The daemon broadcasts every subagent's timeline; keep only the ones a tab shows.
        if let PaseoEvent::TimelineEntry(entry) = &event
            && parse_subagent_timeline_id(&entry.agent_id).is_some()
            && !self.watchers.contains_key(&entry.agent_id)
        {
            return;
        }
        if let PaseoEvent::SubagentRemoved {
            parent_agent_id,
            subagent_id,
        } = &event
        {
            let timeline_id = paseo_client::subagent_timeline_id(parent_agent_id, subagent_id);
            if self.archived_subagents.contains(&timeline_id) {
                self.archived_subagents.remove(&timeline_id);
                self.save_archived_subagents(cx);
            }
        }
        if let PaseoEvent::TimelineEntry(entry) = &event {
            let timeline_id = entry.agent_id.clone();
            self.state.apply_event(event);
            cx.emit(StoreEvent::TimelineChanged(timeline_id));
            return;
        }
        match &event {
            PaseoEvent::Connected => {
                self.status = ConnectionStatus::Connected;
                self.connection_count = self.connection_count.wrapping_add(1);
                self.refresh(cx);
                self.refresh_projects(cx);
                self.sync_subscriptions(cx);
                self.rewatch_terminals(cx);
                for parent_agent_id in self.watched_agents() {
                    self.refresh_subagents(parent_agent_id, cx);
                }
                // The client's reconnect catch-up covers agent timelines only.
                let subagent_timelines = self
                    .watchers
                    .keys()
                    .filter(|timeline_id| parse_subagent_timeline_id(timeline_id).is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                for timeline_id in subagent_timelines {
                    self.load_tail(timeline_id, cx);
                }
            }
            PaseoEvent::TerminalsChanged {
                cwd,
                subscription_id,
                terminals,
            } => {
                if self.terminal_watchers.contains_key(cwd) {
                    self.terminals.insert(cwd.clone(), terminals.clone());
                    if let Some(subscription_id) = subscription_id {
                        self.terminal_list_subscriptions
                            .insert(cwd.clone(), subscription_id.clone());
                    }
                } else if let Some(subscription_id) = subscription_id.clone() {
                    // The directory was unwatched before its first snapshot arrived.
                    self.request_reporting_errors(cx, move |session| async move {
                        session.release_subscription(&subscription_id).await
                    });
                }
            }
            PaseoEvent::Disconnected { .. } => self.status = ConnectionStatus::Reconnecting,
            // The event handler below records the reason after `disconnect` clears the old state.
            PaseoEvent::ConnectionFailed { .. } => self.disconnect(cx),
            PaseoEvent::WorkspacesSnapshot {
                next_cursor: Some(cursor),
                ..
            } => self.load_workspace_pages(cursor.clone(), cx),
            PaseoEvent::DaemonUpdateProgress { phase } => {
                self.daemon_update_phase = Some(phase.clone());
            }
            PaseoEvent::ProvidersChanged(providers) => self.providers = providers.clone(),
            PaseoEvent::ServerInfo(info) => self.server_info = info.clone(),
            PaseoEvent::AgentsChanged(agents) => {
                if let Some(archived) = self.archived.as_mut() {
                    archived.retain(|archived| !agents.iter().any(|agent| agent.id == archived.id));
                }
            }
            _ => {}
        }
        let changes_projects = matches!(
            event,
            PaseoEvent::ProjectUpserted(_) | PaseoEvent::WorkspacesSnapshot { .. }
        );
        let changes_workspaces = matches!(
            event,
            PaseoEvent::WorkspaceUpserted(_) | PaseoEvent::WorkspacesSnapshot { .. }
        );
        let before = self.buckets();
        self.state.apply_event(event);
        self.announce_transitions(&before, cx);
        if changes_projects {
            self.load_project_icons(cx);
        }
        if changes_workspaces {
            self.load_setup_statuses(cx);
        }
        cx.notify();
    }

    pub(crate) fn refresh_projects(&mut self, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let task = self.session_request(cx, |session| async move { session.projects().await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(projects) => {
                        store.state.set_projects(projects);
                        store.load_project_icons(cx);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Loads the setup state of worktree workspaces not seen yet, so a setup that was already
    /// blocked or failed offers Run Setup; later changes arrive as events.
    fn load_setup_statuses(&mut self, cx: &mut Context<Self>) {
        let workspace_ids = self
            .state
            .workspaces
            .values()
            .filter(|workspace| workspace.kind == "worktree" || workspace.is_paseo_worktree)
            .map(|workspace| workspace.id.clone())
            .filter(|workspace_id| self.setup_checked.insert(workspace_id.clone()))
            .collect::<Vec<_>>();
        if workspace_ids.is_empty() {
            return;
        }
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            let mut snapshots = Vec::new();
            for workspace_id in workspace_ids {
                match session.workspace_setup_status(&workspace_id).await {
                    Ok(Some(snapshot)) => snapshots.push((workspace_id, snapshot)),
                    Ok(None) => {}
                    Err(error) => log::debug!(
                        "Paseo could not read the setup of workspace {workspace_id}: {error:#}"
                    ),
                }
            }
            Ok(snapshots)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(snapshots) => {
                        for (workspace_id, snapshot) in snapshots {
                            store.state.setup.entry(workspace_id).or_insert(snapshot);
                        }
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Loads the icons of projects whose icon changed since it was last loaded.
    fn load_project_icons(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .state
            .projects
            .values()
            .filter(|project| {
                self.project_icons
                    .get(&project.id)
                    .is_none_or(|(revision, _)| *revision != project.icon_revision)
            })
            .map(|project| (project.id.clone(), project.icon_revision.clone()))
            .collect::<Vec<_>>();
        for (project_id, revision) in stale {
            let has_icon = revision
                .as_deref()
                .is_some_and(|revision| !revision.starts_with("automatic:none"));
            let previous = self
                .project_icons
                .insert(project_id.clone(), (revision.clone(), None))
                .and_then(|(_, image)| image);
            if !has_icon {
                continue;
            }
            // The old icon stays up until the new one arrives.
            if let Some(entry) = self.project_icons.get_mut(&project_id) {
                entry.1 = previous;
            }
            let generation = self.connection_generation;
            let request_project = project_id.clone();
            let task = self.session_request(cx, move |session| async move {
                session.project_icon(&request_project).await
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |store, cx| {
                    if !store.is_current_connection(generation) {
                        return;
                    }
                    let image = match result {
                        Ok(Some((bytes, mime_type))) => {
                            gpui::ImageFormat::from_mime_type(&mime_type)
                                .map(|format| Arc::new(gpui::Image::from_bytes(format, bytes)))
                        }
                        Ok(None) => None,
                        Err(error) => {
                            log::debug!("Paseo project icon failed to load: {error:#}");
                            None
                        }
                    };
                    if let Some(entry) = store.project_icons.get_mut(&project_id)
                        && entry.0 == revision
                    {
                        entry.1 = image;
                        cx.notify();
                    }
                })
            })
            .detach_and_log_err(cx);
        }
    }

    /// Loads the workspace pages after the subscription's first one.
    fn load_workspace_pages(&mut self, cursor: String, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            let mut pages = Vec::new();
            let mut cursor = Some(cursor);
            while let Some(page_cursor) = cursor.take() {
                let (workspaces, empty_projects, next_cursor) =
                    session.workspaces_page(Some(&page_cursor)).await?;
                pages.push((workspaces, empty_projects));
                cursor = next_cursor;
            }
            Ok(pages)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(pages) => {
                        for (workspaces, empty_projects) in pages {
                            store
                                .state
                                .add_workspace_page(workspaces, empty_projects, false);
                        }
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            Ok((session.providers(None).await?, session.agents().await?))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok((providers, agents)) => {
                        store.apply_refresh(generation, providers, agents);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn session_request<R, F, Fut>(
        &self,
        cx: &mut Context<Self>,
        request: F,
    ) -> Task<Result<R>>
    where
        R: Send + 'static,
        F: FnOnce(Arc<PaseoSession>) -> Fut,
        Fut: Future<Output = Result<R>> + Send + 'static,
    {
        let Some(session) = self.session.clone() else {
            return Task::ready(Err(anyhow!("Not connected to Paseo")));
        };
        Tokio::spawn_result(cx, request(session))
    }

    /// Runs a request whose only visible outcome is an error banner on failure.
    pub(crate) fn request_reporting_errors<F, Fut>(&self, cx: &mut Context<Self>, request: F)
    where
        F: FnOnce(Arc<PaseoSession>) -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let generation = self.connection_generation;
        let task = self.session_request(cx, request);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |store, cx| {
                    if store.is_current_connection(generation) {
                        store.state.error = Some(error.to_string());
                        cx.notify();
                    }
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Records the agent the user is looking at, which scopes Paseo shortcuts such as Review Last
    /// Turn when no agent tab is active.
    pub(crate) fn set_focused_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        if self.focused_agent.as_deref() != Some(agent_id.as_str()) {
            self.focused_agent = Some(agent_id);
            cx.emit(StoreEvent::FocusChanged);
        }
    }

    pub(crate) fn watch_terminals(&mut self, directory: &str, cx: &mut Context<Self>) {
        let count = self
            .terminal_watchers
            .entry(directory.to_owned())
            .or_default();
        *count += 1;
        if *count == 1 {
            let directory = directory.to_owned();
            self.request_reporting_errors(cx, move |session| async move {
                session.watch_terminals(&directory).await
            });
        }
    }

    pub(crate) fn unwatch_terminals(&mut self, directory: &str, cx: &mut Context<Self>) {
        let Some(count) = self.terminal_watchers.get_mut(directory) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.terminal_watchers.remove(directory);
            self.terminals.remove(directory);
            if let Some(subscription_id) = self.terminal_list_subscriptions.remove(directory)
                && self.status == ConnectionStatus::Connected
            {
                self.request_reporting_errors(cx, move |session| async move {
                    session.release_subscription(&subscription_id).await
                });
            }
        }
    }

    fn rewatch_terminals(&mut self, cx: &mut Context<Self>) {
        self.terminal_list_subscriptions.clear();
        for directory in self.terminal_watchers.keys().cloned().collect::<Vec<_>>() {
            self.request_reporting_errors(cx, move |session| async move {
                session.watch_terminals(&directory).await
            });
        }
    }

    pub(crate) fn watch(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let count = self.watchers.entry(agent_id.to_owned()).or_default();
        *count += 1;
        if *count == 1 {
            self.sync_subscriptions(cx);
            // Broadcast upserts may have filled only part of this list while nothing watched it.
            let parent_agent_id = parse_subagent_timeline_id(agent_id)
                .map_or(agent_id, |(parent_agent_id, _)| parent_agent_id);
            self.refresh_subagents(parent_agent_id.to_owned(), cx);
        }
    }

    /// The agents whose timelines are watched, counting a watched subagent as its parent.
    fn watched_agents(&self) -> Vec<String> {
        self.watchers
            .keys()
            .map(|timeline_id| {
                parse_subagent_timeline_id(timeline_id)
                    .map_or(timeline_id.as_str(), |(parent_agent_id, _)| parent_agent_id)
                    .to_owned()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn unwatch(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let Some(count) = self.watchers.get_mut(agent_id) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.watchers.remove(agent_id);
            // Unsubscribed agents miss live chunks, so a later watch must refetch the tail.
            self.paging.remove(agent_id);
            self.state.timeline.retain(|(id, _, _), _| id != agent_id);
            self.sync_subscriptions(cx);
        }
    }

    fn sync_subscriptions(&mut self, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        let watched = self.watchers.keys().cloned().collect::<Vec<_>>();
        // Subagent timelines arrive as broadcast subagent updates, so only agents subscribe.
        let subscribed = watched
            .iter()
            .filter(|timeline_id| parse_subagent_timeline_id(timeline_id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let unloaded = watched
            .iter()
            .filter(|agent_id| {
                !self
                    .paging
                    .get(agent_id.as_str())
                    .is_some_and(|paging| paging.loaded)
            })
            .cloned()
            .collect::<Vec<_>>();
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            session.set_timeline_subscriptions(subscribed).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(()) => {
                        for agent_id in unloaded {
                            store.load_tail(agent_id, cx);
                        }
                    }
                    Err(error) => {
                        store.state.error = Some(error.to_string());
                        cx.notify();
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Fetches an agent's subagent list; live changes then arrive as update events.
    pub(crate) fn refresh_subagents(&mut self, parent_agent_id: String, cx: &mut Context<Self>) {
        if !self.server_info.has_feature("providerSubagents") {
            return;
        }
        let generation = self.connection_generation;
        let requested = parent_agent_id.clone();
        self.subagents_loading.insert(requested.clone());
        let task = self.session_request(cx, move |session| async move {
            session.subagents(&parent_agent_id).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                store.subagents_loading.remove(&requested);
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(subagents) => store.state.set_subagents(&requested, subagents),
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn load_tail(&mut self, agent_id: String, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let requested_agent_id = agent_id.clone();
        let task = self.session_request(cx, move |session| async move {
            match parse_subagent_timeline_id(&agent_id) {
                Some((parent_agent_id, subagent_id)) => {
                    session
                        .subagent_timeline(parent_agent_id, subagent_id, None)
                        .await
                }
                None => session.timeline_tail(&agent_id).await,
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(page) => {
                        if !store.state.begin_epoch(&requested_agent_id, &page.epoch) {
                            return;
                        }
                        store.state.set_history(&requested_agent_id, page.entries);
                        // A refetched tail can replace entries in place without changing their
                        // count, which chat views would otherwise not notice.
                        cx.emit(StoreEvent::TimelineChanged(requested_agent_id.clone()));
                        let paging = store.paging.entry(requested_agent_id).or_default();
                        paging.older_cursor = page.start_cursor;
                        paging.has_older = page.has_older;
                        paging.loaded = true;
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn load_older(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let Some(paging) = self.paging.get_mut(agent_id) else {
            return;
        };
        let Some(cursor) = paging.older_cursor.clone() else {
            return;
        };
        if paging.loading_older || !paging.has_older {
            return;
        }
        paging.loading_older = true;
        let generation = self.connection_generation;
        let requested_agent_id = agent_id.to_owned();
        let requested_cursor = cursor.clone();
        let agent_id = agent_id.to_owned();
        let task = self.session_request(cx, move |session| async move {
            match parse_subagent_timeline_id(&agent_id) {
                Some((parent_agent_id, subagent_id)) => {
                    session
                        .subagent_timeline(parent_agent_id, subagent_id, Some(&cursor))
                        .await
                }
                None => session.timeline_before(&agent_id, &cursor).await,
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                match result {
                    Ok(page) => {
                        store.apply_older_page(
                            generation,
                            &requested_agent_id,
                            &requested_cursor,
                            page,
                        );
                    }
                    Err(error) => {
                        if let Some(paging) = store.paging.get_mut(&requested_agent_id) {
                            paging.loading_older = false;
                        }
                        store.state.error = Some(error.to_string());
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn create_agent(
        &mut self,
        request: CreateAgent,
        cx: &mut Context<Self>,
    ) -> Task<Result<AgentSummary>> {
        let generation = self.connection_generation;
        let task = self.session_request(
            cx,
            move |session| async move { session.create(request).await },
        );
        cx.spawn(async move |this, cx| {
            let agent = task.await?;
            this.update(cx, |store, cx| {
                if store.is_current_connection(generation) {
                    store.state.upsert_agent(agent.clone());
                    cx.notify();
                }
            })?;
            Ok(agent)
        })
    }

    pub(crate) fn send_message(
        &mut self,
        message: SendMessage,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.session_request(cx, move |session| async move {
            session.send_message(message).await
        })
    }

    pub(crate) fn fork_context(
        &mut self,
        agent_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Value>> {
        let agent_id = agent_id.to_owned();
        self.session_request(cx, move |session| async move {
            session.fork_context(&agent_id).await
        })
    }

    pub(crate) fn cancel(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(
            cx,
            move |session| async move { session.cancel(&agent_id).await },
        );
    }

    pub(crate) fn respond_permission(
        &mut self,
        request_id: String,
        response: PermissionResponse,
        cx: &mut Context<Self>,
    ) {
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            session
                .respond_permission(&request_id, response)
                .await
                .map(|()| request_id)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(request_id) => {
                        store.state.permissions.remove(&request_id);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn archive(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if let Some(agent) = self.agent(agent_id).cloned() {
            self.state.agents.retain(|existing| existing.id != agent_id);
            if let Some(archived) = self.archived.as_mut() {
                archived.insert(0, agent);
            }
            cx.notify();
        }
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(cx, move |session| async move {
            session.archive(&agent_id).await
        });
    }

    pub(crate) fn unarchive(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if let Some(archived) = self.archived.as_mut() {
            archived.retain(|agent| agent.id != agent_id);
        }
        let generation = self.connection_generation;
        let agent_id = agent_id.to_owned();
        let task = self.session_request(cx, move |session| async move {
            session.unarchive(&agent_id).await?;
            session.agents().await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(agents) => store.state.set_agents(agents),
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    pub(crate) fn delete(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        self.state.agents.retain(|agent| agent.id != agent_id);
        if let Some(archived) = self.archived.as_mut() {
            archived.retain(|agent| agent.id != agent_id);
        }
        cx.notify();
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(
            cx,
            move |session| async move { session.delete(&agent_id).await },
        );
    }

    pub(crate) fn rename(&mut self, agent_id: &str, name: String, cx: &mut Context<Self>) {
        let name = name.trim().chars().take(200).collect::<String>();
        if name.is_empty() {
            return;
        }
        if let Some(agent) = self
            .state
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
        {
            agent.title = Some(name.clone());
            cx.notify();
        }
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(cx, move |session| async move {
            session.rename(&agent_id, &name).await
        });
    }

    pub(crate) fn set_mode(&mut self, agent_id: &str, mode_id: String, cx: &mut Context<Self>) {
        self.state
            .patch_agent(agent_id, "currentModeId", Value::String(mode_id.clone()));
        cx.notify();
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(cx, move |session| async move {
            session.set_mode(&agent_id, &mode_id).await
        });
    }

    pub(crate) fn set_model(&mut self, agent_id: &str, model_id: String, cx: &mut Context<Self>) {
        self.state
            .patch_agent(agent_id, "model", Value::String(model_id.clone()));
        cx.notify();
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(cx, move |session| async move {
            session.set_model(&agent_id, Some(&model_id)).await
        });
    }

    pub(crate) fn set_thinking(
        &mut self,
        agent_id: &str,
        option_id: String,
        cx: &mut Context<Self>,
    ) {
        self.state.patch_agent(
            agent_id,
            "thinkingOptionId",
            Value::String(option_id.clone()),
        );
        cx.notify();
        let agent_id = agent_id.to_owned();
        self.request_reporting_errors(cx, move |session| async move {
            session.set_thinking(&agent_id, Some(&option_id)).await
        });
    }

    pub(crate) fn clear_attention(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        let needs_clearing = self
            .state
            .agents
            .iter()
            .any(|agent| agent.id == agent_id && agent_requires_attention(agent));
        if !needs_clearing || !self.connected() {
            return;
        }
        self.state
            .patch_agent(agent_id, "requiresAttention", Value::Bool(false));
        cx.notify();
        let agent_ids = vec![agent_id.to_owned()];
        // The daemon sends no reply when clearing fails, so a failure is only a timeout.
        let task = self.session_request(cx, move |session| async move {
            session.clear_attention(agent_ids).await
        });
        task.detach_and_log_err(cx);
    }

    pub(crate) fn load_commands(
        &mut self,
        cache_key: String,
        agent_id: Option<String>,
        draft: Option<DraftConfig>,
        cx: &mut Context<Self>,
    ) {
        if self.commands.contains_key(&cache_key) || self.session.is_none() {
            return;
        }
        self.commands.insert(cache_key.clone(), Vec::new());
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            session.list_commands(agent_id.as_deref(), draft).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(commands) => {
                        store.commands.insert(cache_key, commands);
                    }
                    Err(error) => {
                        log::debug!("Paseo command list unavailable: {error}");
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn directory_suggestions(
        &mut self,
        query: String,
        cwd: Option<String>,
        include_files: bool,
        include_directories: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<DirectorySuggestion>>> {
        self.session_request(cx, move |session| async move {
            session
                .directory_suggestions(
                    &query,
                    cwd.as_deref(),
                    include_files,
                    include_directories,
                    50,
                )
                .await
        })
    }

    pub(crate) fn load_archived(&mut self, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let task =
            self.session_request(
                cx,
                move |session| async move { session.archived_agents().await },
            );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(agents) => {
                        store.archived = Some(agents);
                        store.load_recovery(cx);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Asks which archived agents' workspaces can be restored, for those not asked yet.
    fn load_recovery(&mut self, cx: &mut Context<Self>) {
        let workspace_ids = self
            .archived
            .iter()
            .flatten()
            .filter_map(|agent| agent.extra.get("workspaceId").and_then(Value::as_str))
            .filter(|workspace_id| !self.recovery.contains_key(*workspace_id))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if workspace_ids.is_empty() {
            return;
        }
        let generation = self.connection_generation;
        let task = self.session_request(cx, move |session| async move {
            let mut states = Vec::new();
            for workspace_id in workspace_ids {
                // One workspace that can't be inspected shouldn't hide the others' Restore.
                match session.workspace_recovery(&workspace_id).await {
                    Ok(state) => states.push((workspace_id, state)),
                    Err(error) => log::warn!(
                        "Paseo could not inspect archived workspace {workspace_id}: {error:#}"
                    ),
                }
            }
            Ok(states)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(states) => store.recovery.extend(states),
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Restores an archived workspace with its agents, then reloads the archived list.
    pub(crate) fn restore_workspace(&mut self, workspace_id: String, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        let request_id = workspace_id.clone();
        let task = self.session_request(cx, move |session| async move {
            session.restore_workspace(&request_id).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(()) => {
                        store.recovery.remove(&workspace_id);
                        store.load_archived(cx);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.state.error = None;
        cx.notify();
    }
}

#[derive(Default)]
pub(crate) struct StoreState {
    pub agents: Vec<AgentSummary>,
    pub timeline: BTreeMap<(String, String, u64), TimelineEntry>,
    epochs: BTreeMap<String, String>,
    retired_epochs: BTreeMap<String, BTreeSet<String>>,
    pub permissions: BTreeMap<String, PermissionRequest>,
    /// Provider subagents by parent agent ID, oldest first.
    pub subagents: BTreeMap<String, Vec<ProviderSubagent>>,
    pub workspaces: BTreeMap<String, WorkspaceDescriptor>,
    /// Keyed by project ID: the `projectKey` differs between the project list and workspaces.
    pub projects: BTreeMap<String, ProjectDescriptor>,
    pub labels: Vec<WorkspaceLabel>,
    pub setup: BTreeMap<String, SetupSnapshot>,
    pub error: Option<String>,
}

impl StoreState {
    fn clear_for_connection(&mut self) {
        self.agents.clear();
        self.timeline.clear();
        self.epochs.clear();
        self.retired_epochs.clear();
        self.permissions.clear();
        self.subagents.clear();
        self.workspaces.clear();
        self.projects.clear();
        self.labels.clear();
        self.setup.clear();
        self.error = None;
    }

    /// Adds a page of workspaces. The first page replaces what an earlier connection left.
    pub fn add_workspace_page(
        &mut self,
        workspaces: Vec<WorkspaceDescriptor>,
        empty_projects: Vec<ProjectDescriptor>,
        first_page: bool,
    ) {
        if first_page {
            self.workspaces.clear();
        }
        for workspace in workspaces {
            self.workspaces.insert(workspace.id.clone(), workspace);
        }
        for project in empty_projects {
            self.projects.insert(project.id.clone(), project);
        }
    }

    pub fn set_projects(&mut self, projects: Vec<ProjectDescriptor>) {
        self.projects = projects
            .into_iter()
            .map(|project| (project.id.clone(), project))
            .collect();
    }

    fn rename_label(&mut self, label: WorkspaceLabel, previous_name: Option<String>) {
        let replaced = previous_name.as_deref().unwrap_or(&label.name).to_owned();
        if replaced != label.name {
            for workspace in self.workspaces.values_mut() {
                for name in &mut workspace.labels {
                    if *name == replaced {
                        *name = label.name.clone();
                    }
                }
            }
        }
        match self
            .labels
            .iter_mut()
            .find(|existing| existing.name == replaced)
        {
            Some(existing) => *existing = label,
            None => self.labels.push(label),
        }
    }

    fn remove_label(&mut self, name: &str) {
        self.labels.retain(|label| label.name != name);
        for workspace in self.workspaces.values_mut() {
            workspace.labels.retain(|label| label != name);
        }
    }

    pub fn subagents_for(&self, parent_agent_id: &str) -> &[ProviderSubagent] {
        self.subagents
            .get(parent_agent_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Replaces a parent's list with a fetched snapshot, keeping any record an update event made
    /// newer while the request was in flight.
    fn set_subagents(&mut self, parent_agent_id: &str, mut subagents: Vec<ProviderSubagent>) {
        if let Some(current) = self.subagents.get(parent_agent_id) {
            for subagent in &mut subagents {
                if let Some(newer) = current.iter().find(|existing| {
                    existing.id == subagent.id && existing.updated_at > subagent.updated_at
                }) {
                    *subagent = newer.clone();
                }
            }
        }
        subagents.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        self.subagents.insert(parent_agent_id.to_owned(), subagents);
    }

    fn upsert_subagent(&mut self, subagent: ProviderSubagent) {
        let subagents = self
            .subagents
            .entry(subagent.parent_agent_id.clone())
            .or_default();
        match subagents
            .iter_mut()
            .find(|existing| existing.id == subagent.id)
        {
            Some(existing) => *existing = subagent,
            None => {
                subagents.push(subagent);
                subagents.sort_by(|left, right| left.created_at.cmp(&right.created_at));
            }
        }
    }

    pub fn current_epoch(&self, agent_id: &str) -> Option<&str> {
        self.epochs.get(agent_id).map(String::as_str)
    }

    pub fn begin_epoch(&mut self, agent_id: &str, epoch: &str) -> bool {
        if self.current_epoch(agent_id) == Some(epoch) {
            return true;
        }
        if self
            .retired_epochs
            .get(agent_id)
            .is_some_and(|retired| retired.contains(epoch))
        {
            return false;
        }
        if let Some(previous) = self.epochs.insert(agent_id.to_owned(), epoch.to_owned()) {
            self.retired_epochs
                .entry(agent_id.to_owned())
                .or_default()
                .insert(previous);
            self.timeline.retain(|(id, _, _), _| id != agent_id);
        }
        true
    }

    fn upsert_agent(&mut self, agent: AgentSummary) {
        if let Some(existing) = self
            .agents
            .iter_mut()
            .find(|existing| existing.id == agent.id)
        {
            *existing = agent;
        } else {
            self.agents.push(agent);
        }
    }

    fn patch_agent(&mut self, agent_id: &str, key: &str, value: Value) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id)
            && let Some(object) = agent.extra.as_object_mut()
        {
            object.insert(key.to_owned(), value);
        }
    }

    pub fn set_agents(&mut self, agents: Vec<AgentSummary>) {
        self.permissions.clear();
        for agent in &agents {
            if let Some(requests) = agent
                .extra
                .get("pendingPermissions")
                .and_then(|value| value.as_array())
            {
                for request in requests {
                    if let Some(request_id) = request.get("id").and_then(|value| value.as_str()) {
                        let title = request
                            .get("title")
                            .or_else(|| request.get("name"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("Permission Required");
                        self.permissions.insert(
                            request_id.into(),
                            PermissionRequest {
                                agent_id: agent.id.clone(),
                                request_id: request_id.into(),
                                title: title.into(),
                                description: request
                                    .get("description")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_owned),
                                extra: request.clone(),
                            },
                        );
                    }
                }
            }
        }
        self.agents = agents;
    }

    pub fn set_history(&mut self, agent_id: &str, entries: Vec<TimelineEntry>) {
        for entry in entries {
            if entry.agent_id == agent_id {
                self.insert_projected_entry(entry);
            }
        }
    }

    pub fn insert_entry(&mut self, entry: TimelineEntry) {
        if !self.begin_epoch(&entry.agent_id, &entry.epoch) {
            return;
        }
        let start = (entry.agent_id.clone(), entry.epoch.clone(), 0);
        let end = (entry.agent_id.clone(), entry.epoch.clone(), entry.sequence);
        if self.timeline.range(start..=end).any(|(_, existing)| {
            existing.extra.get("sourceSeqRanges").is_some()
                && source_ranges(existing)
                    .iter()
                    .any(|(start, end)| *start <= entry.sequence && entry.sequence <= *end)
        }) {
            return;
        }
        self.timeline.insert(
            (entry.agent_id.clone(), entry.epoch.clone(), entry.sequence),
            entry,
        );
    }

    pub fn insert_projected_entry(&mut self, entry: TimelineEntry) {
        if !self.begin_epoch(&entry.agent_id, &entry.epoch) {
            return;
        }
        let ranges = source_ranges(&entry);
        self.timeline.retain(|_, existing| {
            existing.agent_id != entry.agent_id
                || existing.epoch != entry.epoch
                || !source_ranges(existing)
                    .iter()
                    .any(|(existing_start, existing_end)| {
                        ranges
                            .iter()
                            .any(|(start, end)| start <= existing_end && existing_start <= end)
                    })
        });
        self.timeline.insert(
            (entry.agent_id.clone(), entry.epoch.clone(), entry.sequence),
            entry,
        );
    }

    pub fn apply_event(&mut self, event: PaseoEvent) {
        match event {
            PaseoEvent::AgentsChanged(agents) => self.set_agents(agents),
            PaseoEvent::TimelineEntry(entry) => self.insert_entry(entry),
            PaseoEvent::TimelineReplaced { agent_id, epoch } => {
                self.begin_epoch(&agent_id, &epoch);
            }
            PaseoEvent::PermissionRequested(request) => {
                self.permissions.insert(request.request_id.clone(), request);
            }
            PaseoEvent::PermissionResolved { request_id } => {
                self.permissions.remove(&request_id);
            }
            PaseoEvent::SubagentUpserted(subagent) => self.upsert_subagent(subagent),
            PaseoEvent::SubagentRemoved {
                parent_agent_id,
                subagent_id,
            } => {
                if let Some(subagents) = self.subagents.get_mut(&parent_agent_id) {
                    subagents.retain(|subagent| subagent.id != subagent_id);
                }
            }
            PaseoEvent::WorkspacesSnapshot {
                workspaces,
                empty_projects,
                ..
            } => self.add_workspace_page(workspaces, empty_projects, true),
            PaseoEvent::WorkspaceUpserted(workspace) => {
                self.workspaces.insert(workspace.id.clone(), workspace);
            }
            PaseoEvent::WorkspaceRemoved {
                workspace_id,
                removed_project_id,
            } => {
                self.workspaces.remove(&workspace_id);
                self.setup.remove(&workspace_id);
                if let Some(project_id) = removed_project_id {
                    self.projects.remove(&project_id);
                }
            }
            PaseoEvent::ProjectUpserted(project) => {
                self.projects.insert(project.id.clone(), project);
            }
            PaseoEvent::ProjectRemoved { project_id } => {
                self.projects.remove(&project_id);
                self.workspaces
                    .retain(|_, workspace| workspace.project_id != project_id);
            }
            PaseoEvent::LabelsSnapshot(labels) => self.labels = labels,
            PaseoEvent::LabelUpserted {
                label,
                previous_name,
            } => self.rename_label(label, previous_name),
            PaseoEvent::LabelRemoved { name } => self.remove_label(&name),
            PaseoEvent::ScriptsChanged {
                workspace_id,
                scripts,
            } => {
                if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
                    workspace.scripts = scripts;
                }
            }
            PaseoEvent::SetupProgress {
                workspace_id,
                snapshot,
            } => {
                self.setup.insert(workspace_id, snapshot);
            }
            PaseoEvent::Disconnected { reason } | PaseoEvent::ConnectionFailed { reason } => {
                self.error = Some(reason)
            }
            PaseoEvent::Connected => self.error = None,
            PaseoEvent::ProvidersChanged(_)
            | PaseoEvent::ServerInfo(_)
            | PaseoEvent::TerminalOutput { .. }
            | PaseoEvent::TerminalExited { .. }
            | PaseoEvent::TerminalsChanged { .. }
            | PaseoEvent::DictationPartial { .. }
            | PaseoEvent::DictationFinal { .. }
            | PaseoEvent::DictationFailed { .. }
            | PaseoEvent::DaemonUpdateProgress { .. } => {}
        }
    }
}

fn source_ranges(entry: &TimelineEntry) -> Vec<(u64, u64)> {
    let ranges = entry
        .extra
        .get("sourceSeqRanges")
        .and_then(|ranges| ranges.as_array())
        .into_iter()
        .flatten()
        .filter_map(|range| {
            Some((
                range.get("startSeq")?.as_u64()?,
                range.get("endSeq")?.as_u64()?,
            ))
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        vec![(
            entry.sequence,
            entry
                .extra
                .get("seqEnd")
                .and_then(|sequence| sequence.as_u64())
                .unwrap_or(entry.sequence),
        )]
    } else {
        ranges
    }
}

/// Paseo's sidebar status buckets (`protocol/src/agent-state-bucket.ts`), in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentBucket {
    NeedsInput,
    Failed,
    Attention,
    Running,
    Done,
}

impl AgentBucket {
    pub fn label(self) -> &'static str {
        match self {
            AgentBucket::NeedsInput => "Needs input",
            AgentBucket::Failed => "Failed",
            AgentBucket::Attention => "Ready to review",
            AgentBucket::Running => "Working",
            AgentBucket::Done => "Done",
        }
    }
}

pub fn agent_bucket(agent: &AgentSummary, has_pending_permission: bool) -> AgentBucket {
    if has_pending_permission
        || agent
            .extra
            .get("pendingPermissions")
            .and_then(Value::as_array)
            .is_some_and(|pending| !pending.is_empty())
    {
        AgentBucket::NeedsInput
    } else if agent.status == "error" {
        AgentBucket::Failed
    } else if agent.status == "running" || agent.status == "initializing" {
        AgentBucket::Running
    } else if agent_requires_attention(agent) {
        AgentBucket::Attention
    } else {
        AgentBucket::Done
    }
}

pub fn agent_requires_attention(agent: &AgentSummary) -> bool {
    agent
        .extra
        .get("requiresAttention")
        .and_then(Value::as_bool)
        == Some(true)
}

pub fn agent_title(agent: &AgentSummary) -> String {
    agent
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "New agent".into())
}

pub fn agent_string<'a>(agent: &'a AgentSummary, key: &str) -> Option<&'a str> {
    agent
        .extra
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub fn agent_provider(agent: &AgentSummary) -> &str {
    agent_string(agent, "provider").unwrap_or("agent")
}

pub fn agent_updated_at(agent: &AgentSummary) -> Option<DateTime<Utc>> {
    parse_optional_timestamp(agent.extra.get("updatedAt"))
        .or_else(|| parse_optional_timestamp(agent.extra.get("createdAt")))
}

pub fn agent_turn_started_at(agent: &AgentSummary) -> Option<DateTime<Utc>> {
    parse_optional_timestamp(
        agent
            .extra
            .get("activeTurn")
            .and_then(|turn| turn.get("startedAt")),
    )
}

/// A subagent's name as Paseo shows it: its description, else its title.
pub fn subagent_title(subagent: &ProviderSubagent) -> String {
    subagent
        .description
        .as_deref()
        .or(subagent.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Subagent")
        .to_owned()
}

pub fn subagent_bucket(subagent: &ProviderSubagent) -> AgentBucket {
    match subagent.status.as_str() {
        "running" => AgentBucket::Running,
        "failed" => AgentBucket::Failed,
        _ => AgentBucket::Done,
    }
}

pub fn agent_is_running(agent: &AgentSummary) -> bool {
    matches!(agent.status.as_str(), "running" | "initializing")
}

/// The project a directory entry assigned to the agent, falling back to the directory's name.
pub fn agent_project_name(agent: &AgentSummary) -> String {
    agent
        .project
        .as_ref()
        .and_then(|project| {
            project
                .get("projectName")
                .or_else(|| project.get("projectDisplayName"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .or_else(|| {
            agent.directory.as_ref().and_then(|directory| {
                directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "No project".into())
}

pub fn agent_project_key(agent: &AgentSummary) -> String {
    agent
        .project
        .as_ref()
        .and_then(|project| project.get("projectKey").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            agent
                .directory
                .as_ref()
                .map(|directory| directory.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

fn paseo_worktree_checkout(agent: &AgentSummary) -> Option<&Value> {
    agent
        .project
        .as_ref()
        .and_then(|project| project.get("checkout"))
        .filter(|checkout| {
            checkout
                .get("isPaseoOwnedWorktree")
                .and_then(Value::as_bool)
                == Some(true)
        })
}

/// The directory of the project an agent belongs to: the main repository for an agent in a
/// Paseo-owned worktree, else its own directory. Offering a worktree as a place to start new
/// agents would put them inside another agent's checkout.
pub fn agent_project_directory(agent: &AgentSummary) -> Option<PathBuf> {
    paseo_worktree_checkout(agent)
        .and_then(|checkout| checkout.get("mainRepoRoot"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| agent.directory.clone())
}

/// The name of the Paseo-owned worktree an agent runs in, such as `prolific-snake`.
pub fn agent_worktree_name(agent: &AgentSummary) -> Option<String> {
    paseo_worktree_checkout(agent)
        .and_then(|checkout| checkout.get("worktreeRoot"))
        .and_then(Value::as_str)
        .and_then(|root| std::path::Path::new(root).file_name())
        .map(|name| name.to_string_lossy().into_owned())
}

pub fn agent_branch(agent: &AgentSummary) -> Option<String> {
    agent
        .project
        .as_ref()
        .and_then(|project| project.get("checkout"))
        .and_then(|checkout| {
            checkout
                .get("currentBranch")
                .or_else(|| checkout.get("branch"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;
    use paseo_client::TimelinePayload;
    use serde_json::json;

    fn entry(agent_id: &str, epoch: &str, sequence: u64) -> TimelineEntry {
        TimelineEntry {
            agent_id: agent_id.into(),
            epoch: epoch.into(),
            sequence,
            timestamp: "2026-09-25T00:00:00Z".into(),
            payload: TimelinePayload::Message(json!({"text": "hello"})),
            extra: json!({}),
        }
    }

    fn agent(id: &str, status: &str, extra: Value) -> AgentSummary {
        AgentSummary {
            id: id.into(),
            title: None,
            status: status.into(),
            directory: Some(PathBuf::from("/work/zaseo")),
            extra,
            project: None,
        }
    }

    #[test]
    fn stale_refresh_cannot_replace_new_host_agents() {
        let mut store = PaseoStore::default();
        store.connection_generation = 2;
        assert!(!store.apply_refresh(1, Vec::new(), vec![agent("old", "idle", json!({}))]));
        assert!(store.state.agents.is_empty());
    }

    fn subagent(id: &str, status: &str, created_at: &str) -> ProviderSubagent {
        ProviderSubagent {
            id: id.into(),
            parent_agent_id: "parent".into(),
            parent_subagent_id: None,
            provider: "claude".into(),
            title: Some("reviewer".into()),
            description: None,
            status: status.into(),
            created_at: created_at.into(),
            updated_at: created_at.into(),
            tool_call_id: Some(format!("call-{id}")),
            cwd: None,
            subtitle: None,
        }
    }

    #[test]
    fn subagent_snapshots_keep_newer_updates() {
        let mut state = StoreState::default();
        let mut finished = subagent("task", "completed", "2026-09-28T10:00:00Z");
        finished.updated_at = "2026-09-28T10:09:00Z".into();
        state.apply_event(PaseoEvent::SubagentUpserted(finished));
        let mut stale = subagent("task", "running", "2026-09-28T10:00:00Z");
        stale.updated_at = "2026-09-28T10:05:00Z".into();
        state.set_subagents(
            "parent",
            vec![stale, subagent("other", "running", "2026-09-28T10:01:00Z")],
        );
        let statuses = state
            .subagents_for("parent")
            .iter()
            .map(|subagent| (subagent.id.as_str(), subagent.status.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(statuses, vec![("task", "completed"), ("other", "running")]);
    }

    #[test]
    fn store_applies_subagent_updates() {
        let mut state = StoreState::default();
        state.set_subagents(
            "parent",
            vec![
                subagent("second", "running", "2026-09-28T10:05:00Z"),
                subagent("first", "completed", "2026-09-28T10:00:00Z"),
            ],
        );
        let ids = |state: &StoreState| {
            state
                .subagents_for("parent")
                .iter()
                .map(|subagent| (subagent.id.clone(), subagent.status.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&state),
            vec![
                ("first".to_owned(), "completed".to_owned()),
                ("second".to_owned(), "running".to_owned())
            ]
        );
        state.apply_event(PaseoEvent::SubagentUpserted(subagent(
            "second",
            "completed",
            "2026-09-28T10:05:00Z",
        )));
        state.apply_event(PaseoEvent::SubagentUpserted(subagent(
            "third",
            "running",
            "2026-09-28T10:09:00Z",
        )));
        state.apply_event(PaseoEvent::SubagentRemoved {
            parent_agent_id: "parent".into(),
            subagent_id: "first".into(),
        });
        assert_eq!(
            ids(&state),
            vec![
                ("second".to_owned(), "completed".to_owned()),
                ("third".to_owned(), "running".to_owned())
            ]
        );
        assert!(state.subagents_for("other").is_empty());
    }

    #[test]
    fn connection_request_clears_old_host_before_profile_write_completes() {
        let mut store = PaseoStore::default();
        store.state.insert_entry(entry("old", "old-epoch", 1));
        let profile = PaseoConnectionProfile {
            name: "New".into(),
            target_uri: "ws://127.0.0.1:6767/ws".into(),
            editor_ssh_uri: None,
            client_id: "new-client".into(),
        };
        let first = store.begin_connection(profile.clone());
        let second = store.begin_connection(profile);
        assert_ne!(first, second);
        assert_eq!(store.status, ConnectionStatus::Connecting);
        assert!(store.state.timeline.is_empty());
        assert!(!store.is_current_connection(first));
    }

    #[test]
    fn older_page_cannot_change_another_agents_cursor() {
        let mut store = PaseoStore::default();
        store.connection_generation = 3;
        store.paging.insert(
            "new".into(),
            AgentPaging {
                older_cursor: Some(TimelineCursor {
                    epoch: "new-epoch".into(),
                    sequence: 3,
                }),
                has_older: true,
                loading_older: false,
                loaded: true,
            },
        );
        let requested_cursor = TimelineCursor {
            epoch: "old-epoch".into(),
            sequence: 7,
        };
        let page = TimelinePage {
            epoch: "old-epoch".into(),
            entries: vec![entry("old", "old-epoch", 6)],
            start_cursor: None,
            end_cursor: None,
            has_older: false,
            has_newer: true,
        };
        assert!(!store.apply_older_page(3, "old", &requested_cursor, page));
        assert_eq!(
            store
                .paging
                .get("new")
                .and_then(|paging| paging.older_cursor.as_ref())
                .map(|cursor| cursor.sequence),
            Some(3)
        );
        assert!(store.state.timeline.is_empty());
    }

    #[test]
    fn new_epoch_replaces_only_that_agents_old_entries() {
        let mut state = StoreState::default();
        state.insert_entry(entry("a", "old", 1));
        state.insert_entry(entry("b", "other", 2));
        state.insert_entry(entry("a", "new", 1));
        state.insert_entry(entry("a", "old", 3));
        assert_eq!(state.timeline.len(), 2);
        assert!(state.timeline.contains_key(&("a".into(), "new".into(), 1)));
        assert!(
            state
                .timeline
                .contains_key(&("b".into(), "other".into(), 2))
        );
    }

    #[test]
    fn empty_replacement_discards_old_conversation() {
        let mut state = StoreState::default();
        state.insert_entry(entry("agent", "old", 1));
        state.apply_event(PaseoEvent::TimelineReplaced {
            agent_id: "agent".into(),
            epoch: "new".into(),
        });
        assert!(state.timeline.is_empty());
        assert_eq!(state.current_epoch("agent"), Some("new"));
    }

    #[test]
    fn projected_history_replaces_overlapping_live_chunks() {
        let mut state = StoreState::default();
        let mut first = entry("agent", "epoch", 1);
        first.payload = TimelinePayload::Message(json!({"text": "Good "}));
        let mut second = entry("agent", "epoch", 2);
        second.payload = TimelinePayload::Message(json!({"text": "day"}));
        state.insert_entry(first);
        state.insert_entry(second);

        let mut projected = entry("agent", "epoch", 1);
        projected.payload = TimelinePayload::Message(json!({"text": "Good day"}));
        projected.extra = json!({
            "seqEnd": 2,
            "sourceSeqRanges": [{"startSeq": 1, "endSeq": 2}]
        });
        state.set_history("agent", vec![projected]);

        assert_eq!(state.timeline.len(), 1);
        assert_eq!(
            state
                .timeline
                .values()
                .next()
                .and_then(|entry| match &entry.payload {
                    TimelinePayload::Message(value) =>
                        value.get("text").and_then(|text| text.as_str()),
                    _ => None,
                }),
            Some("Good day")
        );
    }

    #[test]
    fn projected_history_reconciles_only_its_source_ranges() {
        let mut state = StoreState::default();
        state.insert_entry(entry("agent", "epoch", 1));
        state.insert_entry(entry("agent", "epoch", 2));
        state.insert_entry(entry("agent", "epoch", 3));

        let mut projected = entry("agent", "epoch", 1);
        projected.extra = json!({
            "seqEnd": 3,
            "sourceSeqRanges": [
                {"startSeq": 1, "endSeq": 1},
                {"startSeq": 3, "endSeq": 3}
            ]
        });
        state.set_history("agent", vec![projected]);

        assert_eq!(state.timeline.len(), 2);
        assert!(
            state
                .timeline
                .contains_key(&("agent".into(), "epoch".into(), 2))
        );

        state.insert_entry(entry("agent", "epoch", 3));
        assert_eq!(state.timeline.len(), 2);
    }

    #[test]
    fn empty_and_disconnected_state_keep_a_visible_error() {
        let mut state = StoreState::default();
        assert!(state.error.is_none());
        state.apply_event(PaseoEvent::Disconnected {
            reason: "network unavailable".into(),
        });
        assert_eq!(state.error.as_deref(), Some("network unavailable"));
        state.apply_event(PaseoEvent::Connected);
        assert!(state.error.is_none());
        state.apply_event(PaseoEvent::ConnectionFailed {
            reason: "Incorrect password".into(),
        });
        assert_eq!(state.error.as_deref(), Some("Incorrect password"));
    }

    #[test]
    fn snapshot_permissions_are_available_before_live_events() {
        let mut state = StoreState::default();
        state.set_agents(vec![agent(
            "agent",
            "idle",
            json!({"pendingPermissions":[{"id":"request", "title":"Use tool"}]}),
        )]);
        assert_eq!(
            state
                .permissions
                .get("request")
                .map(|request| request.title.as_str()),
            Some("Use tool")
        );
        state.apply_event(PaseoEvent::PermissionResolved {
            request_id: "request".into(),
        });
        assert!(state.permissions.is_empty());
    }

    #[test]
    fn status_buckets_follow_paseo_priority() {
        assert_eq!(
            agent_bucket(&agent("a", "running", json!({})), true),
            AgentBucket::NeedsInput
        );
        assert_eq!(
            agent_bucket(&agent("a", "error", json!({})), false),
            AgentBucket::Failed
        );
        assert_eq!(
            agent_bucket(&agent("a", "running", json!({})), false),
            AgentBucket::Running
        );
        assert_eq!(
            agent_bucket(
                &agent("a", "idle", json!({"requiresAttention": true})),
                false
            ),
            AgentBucket::Attention
        );
        assert_eq!(
            agent_bucket(&agent("a", "idle", json!({})), false),
            AgentBucket::Done
        );
    }

    #[test]
    fn project_name_prefers_directory_entry_project() {
        let mut with_project = agent("a", "idle", json!({}));
        with_project.project = Some(json!({"projectKey":"k","projectName":"Zaseo"}));
        assert_eq!(agent_project_name(&with_project), "Zaseo");
        assert_eq!(agent_project_name(&agent("a", "idle", json!({}))), "zaseo");
    }

    fn workspace(id: &str, project_id: &str, labels: &[&str]) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            id: id.into(),
            project_id: project_id.into(),
            project_display_name: "zaseo".into(),
            project_root_path: PathBuf::from("/repo"),
            directory: PathBuf::from("/repo"),
            kind: "directory".into(),
            worktree_slug: None,
            name: id.into(),
            title: None,
            pinned_at: None,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            status: "done".into(),
            activity_at: None,
            diff_stat: None,
            scripts: Vec::new(),
            current_branch: None,
            is_paseo_worktree: false,
            extra: json!({}),
        }
    }

    fn project(id: &str, name: &str) -> ProjectDescriptor {
        ProjectDescriptor {
            id: id.into(),
            display_name: name.into(),
            custom_name: None,
            icon_revision: None,
            root_path: PathBuf::from("/repo"),
            kind: "git".into(),
        }
    }

    #[test]
    fn store_applies_workspace_updates() {
        let mut state = StoreState::default();
        state.apply_event(PaseoEvent::WorkspacesSnapshot {
            workspaces: vec![workspace("wks_1", "prj_1", &["todo"])],
            empty_projects: vec![project("prj_2", "empty")],
            next_cursor: None,
        });
        state.add_workspace_page(vec![workspace("wks_2", "prj_1", &[])], Vec::new(), false);
        assert_eq!(state.workspaces.len(), 2, "later pages add to the first");
        assert!(state.projects.contains_key("prj_2"));

        state.apply_event(PaseoEvent::LabelsSnapshot(vec![WorkspaceLabel {
            name: "todo".into(),
            color: "sky".into(),
        }]));
        state.apply_event(PaseoEvent::LabelUpserted {
            label: WorkspaceLabel {
                name: "review".into(),
                color: "sky".into(),
            },
            previous_name: Some("todo".into()),
        });
        assert_eq!(state.labels.len(), 1);
        assert_eq!(state.workspaces["wks_1"].labels, vec!["review".to_owned()]);
        state.apply_event(PaseoEvent::LabelRemoved {
            name: "review".into(),
        });
        assert!(state.labels.is_empty());
        assert!(state.workspaces["wks_1"].labels.is_empty());

        state.apply_event(PaseoEvent::WorkspaceRemoved {
            workspace_id: "wks_2".into(),
            removed_project_id: None,
        });
        state.apply_event(PaseoEvent::ProjectRemoved {
            project_id: "prj_1".into(),
        });
        assert!(
            state.workspaces.is_empty(),
            "removing a project removes its workspaces"
        );

        state.apply_event(PaseoEvent::WorkspacesSnapshot {
            workspaces: vec![workspace("wks_3", "prj_1", &[])],
            empty_projects: Vec::new(),
            next_cursor: None,
        });
        assert_eq!(
            state.workspaces.keys().collect::<Vec<_>>(),
            vec!["wks_3"],
            "a new snapshot replaces the old workspaces"
        );
    }

    #[test]
    fn store_keys_projects_by_id() {
        let mut state = StoreState::default();
        state.set_projects(vec![
            project("prj_1", "dotfiles"),
            project("prj_2", "zaseo"),
        ]);
        let mut renamed = project("prj_1", "dotfiles");
        renamed.custom_name = Some("Dots".into());
        state.apply_event(PaseoEvent::ProjectUpserted(renamed));
        assert_eq!(state.projects.len(), 2);
        assert_eq!(state.projects["prj_1"].custom_name.as_deref(), Some("Dots"));
    }

    #[test]
    fn agent_project_directory_uses_the_main_repository_for_paseo_worktrees() {
        let mut worktree_agent = agent("a", "idle", json!({}));
        worktree_agent.directory = Some(PathBuf::from("/home/sr/.paseo/worktrees/3r36fnq4/snake"));
        worktree_agent.project = Some(json!({"checkout": {
            "isPaseoOwnedWorktree": true,
            "worktreeRoot": "/home/sr/.paseo/worktrees/3r36fnq4/snake",
            "mainRepoRoot": "/home/sr/projects/work/axon",
        }}));
        assert_eq!(
            agent_project_directory(&worktree_agent),
            Some(PathBuf::from("/home/sr/projects/work/axon"))
        );

        let mut plain_agent = agent("b", "idle", json!({}));
        plain_agent.directory = Some(PathBuf::from("/home/sr/projects/personal/zaseo"));
        assert_eq!(
            agent_project_directory(&plain_agent),
            Some(PathBuf::from("/home/sr/projects/personal/zaseo"))
        );
    }

    #[gpui::test]
    fn timeline_entries_reach_only_their_timeline_views(cx: &mut gpui::TestAppContext) {
        let store = cx.new(|_| PaseoStore::default());
        let notifications = std::rc::Rc::new(std::cell::Cell::new(0));
        let timeline_events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _observer = cx.update(|cx| {
            let notifications = notifications.clone();
            cx.observe(&store, move |_, _| {
                notifications.set(notifications.get() + 1)
            })
        });
        let _subscription = cx.update(|cx| {
            let timeline_events = timeline_events.clone();
            cx.subscribe(&store, move |_, event: &StoreEvent, _| {
                if let StoreEvent::TimelineChanged(timeline_id) = event {
                    timeline_events.borrow_mut().push(timeline_id.clone());
                }
            })
        });

        store.update(cx, |store, cx| {
            store.handle_event(PaseoEvent::TimelineEntry(entry("agent", "epoch", 1)), cx)
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 0);
        assert_eq!(*timeline_events.borrow(), vec!["agent".to_owned()]);
        cx.read(|cx| assert_eq!(store.read(cx).entries_for("agent").count(), 1));

        store.update(cx, |store, cx| {
            store.handle_event(PaseoEvent::AgentsChanged(Vec::new()), cx)
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 1);
    }
}
