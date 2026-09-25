mod connection_picker;
mod conversation;

use anyhow::{Result, anyhow};
use fs::Fs;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    IntoElement, Pixels, Render, ScrollHandle, Subscription, Task, WeakEntity, Window, actions,
    div, prelude::*, px,
};
use gpui_tokio::Tokio;
use paseo_client::{
    AgentSummary, ConnectionTarget, CreateAgent, PaseoEvent, PaseoSession, PermissionRequest,
    Provider, RuntimePassword, TimelineCursor, TimelineEntry, is_absolute_workspace_path,
};
use settings::{PaseoConnectionProfile, RegisterSetting, Settings};
use std::collections::{BTreeMap, BTreeSet};
use std::{path::PathBuf, sync::Arc};
use ui::IconName;
use ui_input::InputField;
use workspace::{
    Item, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(paseo_ui, [TogglePanel, OpenTab, OpenWorkspace]);

#[derive(Clone, RegisterSetting)]
struct PaseoSettings {
    profiles: Vec<PaseoConnectionProfile>,
    active_profile: String,
}

impl Settings for PaseoSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let paseo = content.paseo.as_ref();
        Self {
            profiles: paseo
                .and_then(|paseo| paseo.profiles.clone())
                .unwrap_or_default(),
            active_profile: paseo
                .and_then(|paseo| paseo.active_profile.clone())
                .unwrap_or_default(),
        }
    }
}

struct GlobalPaseoStore(Entity<PaseoStore>);
impl Global for GlobalPaseoStore {}

pub fn init(cx: &mut App) {
    PaseoSettings::register(cx);
    let store = cx.new(|_| PaseoStore::default());
    cx.set_global(GlobalPaseoStore(store));
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
                workspace.toggle_panel_focus::<PaseoPanel>(window, cx);
            });
            workspace.register_action(|workspace, _: &OpenTab, window, cx| {
                open_tab(workspace, window, cx);
            });
        },
    )
    .detach();
}

fn store(cx: &App) -> Entity<PaseoStore> {
    cx.global::<GlobalPaseoStore>().0.clone()
}

pub struct SelectedWorkspace {
    pub directory: PathBuf,
    pub target: ConnectionTarget,
}

pub fn selected_workspace(cx: &App) -> Result<Option<SelectedWorkspace>> {
    let store = store(cx);
    let store = store.read(cx);
    let Some(agent_id) = store.state.selected_agent.as_deref() else {
        return Ok(None);
    };
    let Some(agent) = store.state.agents.iter().find(|agent| agent.id == agent_id) else {
        return Ok(None);
    };
    let directory = agent
        .directory
        .clone()
        .ok_or_else(|| anyhow!("Selected Paseo agent has no workspace directory"))?;
    if !directory.to_str().is_some_and(is_absolute_workspace_path) {
        return Err(anyhow!("Paseo workspace directory is not absolute"));
    }
    let profile = store
        .active_profile
        .as_ref()
        .ok_or_else(|| anyhow!("No Paseo connection profile is active"))?;
    Ok(Some(SelectedWorkspace {
        directory,
        target: connection_picker::parse_target(profile)?,
    }))
}

#[derive(Default)]
struct DraftSubmission {
    pending_text: Option<String>,
}

impl DraftSubmission {
    fn begin(&mut self, text: &str) -> Option<String> {
        if text.trim().is_empty() || self.pending_text.is_some() {
            return None;
        }
        self.pending_text = Some(text.to_owned());
        Some(text.to_owned())
    }

    fn finish(&mut self, accepted: bool, current_text: &str) -> bool {
        self.pending_text
            .take()
            .is_some_and(|submitted| accepted && submitted == current_text)
    }
}

#[derive(Default)]
struct PaseoStore {
    state: StoreState,
    session: Option<Arc<PaseoSession>>,
    providers: Vec<Provider>,
    connected: bool,
    connecting: bool,
    older_cursor: Option<TimelineCursor>,
    has_older: bool,
    active_profile: Option<PaseoConnectionProfile>,
    connection_generation: u64,
    connection_task: Option<Task<()>>,
    event_task: Option<Task<()>>,
}

impl PaseoStore {
    fn is_current_connection(&self, generation: u64) -> bool {
        self.connection_generation == generation
    }

    fn begin_connection(&mut self, profile: PaseoConnectionProfile) -> u64 {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.connecting = true;
        self.connected = false;
        self.connection_task = None;
        self.event_task = None;
        self.session = None;
        self.providers.clear();
        self.state.clear_for_connection();
        self.older_cursor = None;
        self.has_older = false;
        self.active_profile = Some(profile);
        self.connection_generation
    }

    fn apply_refresh(
        &mut self,
        generation: u64,
        providers: Vec<Provider>,
        agents: Vec<AgentSummary>,
    ) -> bool {
        if self.connection_generation != generation {
            return false;
        }
        self.providers = providers;
        self.state.set_agents(agents);
        true
    }

    fn apply_older_page(
        &mut self,
        generation: u64,
        agent_id: &str,
        requested_cursor: &TimelineCursor,
        page: paseo_client::TimelinePage,
    ) -> bool {
        if self.connection_generation != generation
            || self.state.selected_agent.as_deref() != Some(agent_id)
            || self.older_cursor.as_ref() != Some(requested_cursor)
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
            || page
                .end_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.epoch != requested_cursor.epoch)
        {
            return false;
        }
        for entry in page.entries {
            self.state.insert_projected_entry(entry);
        }
        self.older_cursor = page.start_cursor;
        self.has_older = page.has_older;
        true
    }

    fn connect(
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
                self.connecting = false;
                self.state.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let password = password
            .filter(|password| !password.is_empty())
            .map(RuntimePassword::new);
        let connection = Tokio::spawn_result(
            cx,
            paseo_client::connect(target, password, profile.client_id),
        );
        self.connection_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if store.connection_generation != generation {
                    return;
                }
                match result {
                    Ok((session, events)) => {
                        let session = Arc::new(session);
                        store.session = Some(session);
                        store.connected = true;
                        store.connecting = false;
                        store.state.error = None;
                        store.event_task = Some(cx.spawn(async move |this, cx| {
                            while let Ok(event) = events.recv().await {
                                if this
                                    .update(cx, |store, cx| {
                                        if store.connection_generation != generation {
                                            return;
                                        }
                                        match &event {
                                            PaseoEvent::Connected => {
                                                store.connected = true;
                                                store.refresh(cx);
                                            }
                                            PaseoEvent::Disconnected { .. } => {
                                                store.connected = false
                                            }
                                            _ => {}
                                        }
                                        store.state.apply_event(event);
                                        cx.notify();
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }));
                    }
                    Err(error) => {
                        store.connecting = false;
                        store.state.error = Some(error.to_string());
                    }
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before connection update: {error}");
            }
        }));
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let generation = self.connection_generation;
        let task = Tokio::spawn_result(cx, async move {
            Ok((session.providers(None).await?, session.agents().await?))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if store.connection_generation != generation {
                    return;
                }
                match result {
                    Ok((providers, agents)) => {
                        store.apply_refresh(generation, providers, agents);
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before refresh: {error}");
            }
        })
        .detach();
    }

    fn select_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        self.state.selected_agent = Some(agent_id.clone());
        self.older_cursor = None;
        self.has_older = false;
        let Some(session) = self.session.clone() else {
            cx.notify();
            return;
        };
        let generation = self.connection_generation;
        let requested_agent_id = agent_id.clone();
        let task = Tokio::spawn_result(
            cx,
            async move { session.select_agent_page(&agent_id).await },
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if store.connection_generation != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        if !store.state.begin_epoch(&requested_agent_id, &page.epoch) {
                            return;
                        }
                        store.state.set_history(&requested_agent_id, page.entries);
                        if store.state.selected_agent.as_deref()
                            == Some(requested_agent_id.as_str())
                        {
                            store.older_cursor = page.start_cursor;
                            store.has_older = page.has_older;
                        }
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before history update: {error}");
            }
        })
        .detach();
        cx.notify();
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(agent_id), Some(cursor)) = (
            self.session.clone(),
            self.state.selected_agent.clone(),
            self.older_cursor.clone(),
        ) else {
            return;
        };
        let generation = self.connection_generation;
        let requested_agent_id = agent_id.clone();
        let requested_cursor = cursor.clone();
        let task = Tokio::spawn_result(cx, async move {
            session.timeline_before(&agent_id, &cursor).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if store.connection_generation != generation {
                    return;
                }
                match result {
                    Ok(page) => {
                        store.apply_older_page(
                            generation,
                            &requested_agent_id,
                            &requested_cursor,
                            page,
                        );
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before pagination update: {error}");
            }
        })
        .detach();
    }

    fn create_agent(
        &mut self,
        provider: String,
        model: Option<String>,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let generation = self.connection_generation;
        let request = CreateAgent {
            provider,
            model,
            directory,
            title: None,
            initial_prompt: None,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
        };
        let task = Tokio::spawn_result(cx, async move { session.create(request).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update(cx, |store, cx| {
                if store.connection_generation != generation {
                    return;
                }
                match result {
                    Ok(agent) => {
                        store.state.selected_agent = Some(agent.id.clone());
                        if let Some(existing) = store
                            .state
                            .agents
                            .iter_mut()
                            .find(|existing| existing.id == agent.id)
                        {
                            *existing = agent;
                        } else {
                            store.state.agents.push(agent);
                        }
                    }
                    Err(error) => store.state.error = Some(error.to_string()),
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before creation update: {error}");
            }
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(agent_id)) =
            (self.session.clone(), self.state.selected_agent.clone())
        else {
            return;
        };
        let task = Tokio::spawn_result(cx, async move { session.cancel(&agent_id).await });
        self.handle_result(task, cx);
    }

    fn answer_permission(&mut self, request_id: String, allow: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let generation = self.connection_generation;
        let task = Tokio::spawn_result(cx, async move {
            session
                .answer_permission(&request_id, allow)
                .await
                .map(|()| request_id)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update(cx, |store, cx| {
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
            }) {
                log::debug!("Paseo view closed before permission update: {error}");
            }
        })
        .detach();
    }

    fn handle_result(&mut self, task: Task<Result<()>>, cx: &mut Context<Self>) {
        let generation = self.connection_generation;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = result {
                if let Err(error) = this.update(cx, |store, cx| {
                    if !store.is_current_connection(generation) {
                        return;
                    }
                    store.state.error = Some(error.to_string());
                    cx.notify();
                }) {
                    log::debug!("Paseo view closed before action result: {error}");
                }
            }
        })
        .detach();
    }
}

pub fn open_tab(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(tab) = workspace.item_of_type::<PaseoTab>(cx) {
        workspace.activate_item(&tab, true, true, window, cx);
        return;
    }
    let fs = workspace.project().read(cx).fs().clone();
    let directory = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
    let tab = cx.new(|cx| {
        let view = cx.new(|cx| PaseoView::new(fs, directory, window, cx));
        PaseoTab { view }
    });
    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
}

pub struct PaseoPanel {
    view: Entity<PaseoView>,
    position: DockPosition,
}

impl PaseoPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        Task::ready(workspace.update_in(&mut cx, |workspace, window, cx| {
            let fs = workspace.project().read(cx).fs().clone();
            let directory = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
            cx.new(|cx| {
                let view = cx.new(|cx| PaseoView::new(fs, directory, window, cx));
                Self {
                    view,
                    position: DockPosition::Right,
                }
            })
        }))
    }
}

impl EventEmitter<PanelEvent> for PaseoPanel {}
impl Focusable for PaseoPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }
}
impl Render for PaseoPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.view.clone()
    }
}
impl Panel for PaseoPanel {
    fn persistent_name() -> &'static str {
        "Paseo"
    }
    fn panel_key() -> &'static str {
        "PaseoPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(400.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Paseo")
    }
    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(TogglePanel)
    }
    fn activation_priority(&self) -> u32 {
        4
    }
}

pub struct PaseoTab {
    view: Entity<PaseoView>,
}
impl EventEmitter<()> for PaseoTab {}
impl Focusable for PaseoTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }
}
impl Render for PaseoTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.view.clone()
    }
}
impl Item for PaseoTab {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> gpui::SharedString {
        "Paseo".into()
    }
}

struct PaseoView {
    store: Entity<PaseoStore>,
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    profile_name_input: Entity<InputField>,
    target_input: Entity<InputField>,
    editor_ssh_input: Entity<InputField>,
    password_input: Entity<InputField>,
    directory_input: Entity<InputField>,
    prompt_input: Entity<InputField>,
    draft_submission: DraftSubmission,
    seen_connection_generation: u64,
    selected_provider: Option<String>,
    selected_model: Option<String>,
    timeline_scroll: ScrollHandle,
    markdown: BTreeMap<(String, String, u64), (String, Entity<markdown::Markdown>)>,
    _store_observer: Subscription,
}

impl PaseoView {
    fn new(
        fs: Arc<dyn Fs>,
        directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = store(cx);
        let seen_connection_generation = store.read(cx).connection_generation;
        let observer = cx.observe(&store, |_view, _, cx| cx.notify());
        let profile_name_input = cx.new(|cx| InputField::new(window, cx, "Profile name"));
        let target_input =
            cx.new(|cx| InputField::new(window, cx, "ws://127.0.0.1:6767/ws or ssh://host"));
        let editor_ssh_input = cx.new(|cx| InputField::new(window, cx, "Optional editor SSH URI"));
        let password_input = cx.new(|cx| InputField::new(window, cx, "Password").masked(true));
        let directory_input = cx.new(|cx| InputField::new(window, cx, "Agent workspace directory"));
        if let Some(directory) =
            directory.and_then(|directory| directory.to_str().map(str::to_owned))
        {
            directory_input.update(cx, |input, cx| input.set_text(&directory, window, cx));
        }
        let prompt_input = cx.new(|cx| InputField::new(window, cx, "Message Paseo agent"));
        let active_profile = PaseoSettings::get_global(cx)
            .profiles
            .iter()
            .find(|profile| profile.name == PaseoSettings::get_global(cx).active_profile)
            .cloned();
        if let Some(profile) = active_profile {
            profile_name_input.update(cx, |input, cx| input.set_text(&profile.name, window, cx));
            target_input.update(cx, |input, cx| {
                input.set_text(&profile.target_uri, window, cx)
            });
            if let Some(editor_ssh_uri) = &profile.editor_ssh_uri {
                editor_ssh_input.update(cx, |input, cx| input.set_text(editor_ssh_uri, window, cx));
            }
        }
        Self {
            store,
            focus_handle: cx.focus_handle(),
            fs,
            profile_name_input,
            target_input,
            editor_ssh_input,
            password_input,
            directory_input,
            prompt_input,
            draft_submission: DraftSubmission::default(),
            seen_connection_generation,
            selected_provider: None,
            selected_model: None,
            timeline_scroll: ScrollHandle::new(),
            markdown: BTreeMap::new(),
            _store_observer: observer,
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = self.password_input.read(cx).text(cx);
        self.password_input
            .update(cx, |input, cx| input.clear(window, cx));
        let profile = match self.profile_from_inputs(cx) {
            Ok(profile) => profile,
            Err(error) => {
                self.set_error(error.to_string(), cx);
                return;
            }
        };
        let generation = self.store.update(cx, |store, cx| {
            let generation = store.begin_connection(profile.clone());
            cx.notify();
            generation
        });
        let completion = self.persist_profile(profile.clone(), cx);
        let store = self.store.clone();
        cx.spawn(async move |_, cx| match completion.await {
            Ok(Ok(())) => store.update(cx, |store, cx| {
                store.connect(profile, Some(password), generation, cx)
            }),
            Ok(Err(error)) => store.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                store.connecting = false;
                store.state.error = Some(error.to_string());
                cx.notify();
            }),
            Err(error) => store.update(cx, |store, cx| {
                if !store.is_current_connection(generation) {
                    return;
                }
                store.connecting = false;
                store.state.error = Some(error.to_string());
                cx.notify();
            }),
        })
        .detach();
    }

    fn save_profile(&mut self, cx: &mut Context<Self>) {
        let profile = match self.profile_from_inputs(cx) {
            Ok(profile) => profile,
            Err(error) => {
                self.set_error(error.to_string(), cx);
                return;
            }
        };
        let completion = self.persist_profile(profile, cx);
        let store = self.store.clone();
        cx.spawn(async move |_, cx| {
            let error = match completion.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            store.update(cx, |store, cx| {
                store.state.error = error;
                cx.notify();
            });
        })
        .detach();
    }

    fn profile_from_inputs(&self, cx: &App) -> Result<PaseoConnectionProfile> {
        let name = self.profile_name_input.read(cx).text(cx);
        if name.trim().is_empty() {
            return Err(anyhow!("Profile name is required"));
        }
        let target_uri = self.target_input.read(cx).text(cx);
        let editor_ssh_uri = self.editor_ssh_input.read(cx).text(cx);
        let existing = PaseoSettings::get_global(cx)
            .profiles
            .iter()
            .find(|profile| profile.name == name)
            .cloned();
        let profile = PaseoConnectionProfile {
            name,
            target_uri,
            editor_ssh_uri: (!editor_ssh_uri.trim().is_empty()).then_some(editor_ssh_uri),
            client_id: existing
                .map(|profile| profile.client_id)
                .filter(|client_id| !client_id.is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        };
        connection_picker::parse_target(&profile)?;
        Ok(profile)
    }

    fn persist_profile(
        &self,
        profile: PaseoConnectionProfile,
        cx: &App,
    ) -> futures::channel::oneshot::Receiver<Result<()>> {
        settings::update_settings_file_with_completion(self.fs.clone(), cx, move |settings, _| {
            let paseo = settings.paseo.get_or_insert_default();
            let profiles = paseo.profiles.get_or_insert_default();
            if let Some(existing) = profiles
                .iter_mut()
                .find(|existing| existing.name == profile.name)
            {
                *existing = profile.clone();
            } else {
                profiles.push(profile.clone());
            }
            paseo.active_profile = Some(profile.name);
        })
    }

    fn set_error(&self, error: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.state.error = Some(error);
            cx.notify();
        });
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = {
            let store = self.store.read(cx);
            store
                .session
                .clone()
                .zip(store.state.selected_agent.clone())
                .map(|(session, agent_id)| (session, agent_id, store.connection_generation))
        };
        let Some((session, agent_id, generation)) = connection else {
            self.set_error("Select a connected Paseo agent before sending".into(), cx);
            return;
        };
        let Some(text) = self
            .draft_submission
            .begin(&self.prompt_input.read(cx).text(cx))
        else {
            return;
        };
        let message_id = uuid::Uuid::new_v4().to_string();
        let task = Tokio::spawn_result(cx, async move {
            session.send(&agent_id, &text, &message_id).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            if let Err(error) = this.update_in(cx, |view, window, cx| {
                let current_text = view.prompt_input.read(cx).text(cx);
                let current_generation = view.store.read(cx).connection_generation;
                let accepted = result.is_ok() && current_generation == generation;
                if view.draft_submission.finish(accepted, &current_text) {
                    view.prompt_input.update(cx, |input, cx| input.clear(window, cx));
                }
                if current_generation != generation {
                    view.set_error("Paseo connection changed before send completed; check the agent timeline before retrying".into(), cx);
                } else if let Err(error) = result {
                    view.set_error(error.to_string(), cx);
                }
                cx.notify();
            }) {
                log::debug!("Paseo view closed before send result: {error}");
            }
        }).detach();
    }

    fn create_agent(&mut self, cx: &mut Context<Self>) {
        self.sync_selection(cx);
        let Some(provider) = self.selected_provider.clone() else {
            return;
        };
        let directory = PathBuf::from(self.directory_input.read(cx).text(cx));
        if !directory.to_str().is_some_and(is_absolute_workspace_path) {
            self.store.update(cx, |store, cx| {
                store.state.error = Some("Agent directory must be absolute".into());
                cx.notify();
            });
            return;
        }
        self.store.update(cx, |store, cx| {
            store.create_agent(provider, self.selected_model.clone(), directory, cx)
        });
    }

    fn sync_selection(&mut self, cx: &App) {
        let store = self.store.read(cx);
        if self.seen_connection_generation != store.connection_generation {
            self.seen_connection_generation = store.connection_generation;
            self.selected_provider = None;
            self.selected_model = None;
            self.markdown.clear();
        }
        let Some(provider_id) = self.selected_provider.as_deref() else {
            return;
        };
        let Some(provider) = store
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        else {
            self.selected_provider = None;
            self.selected_model = None;
            return;
        };
        if self
            .selected_model
            .as_deref()
            .is_some_and(|model| !model_is_selectable(provider, model))
        {
            self.selected_model = None;
        }
    }
}

fn model_is_selectable(provider: &Provider, model_id: &str) -> bool {
    provider
        .extra
        .get("models")
        .and_then(|models| models.as_array())
        .is_some_and(|models| {
            models.iter().any(|model| {
                model.get("id").and_then(|id| id.as_str()) == Some(model_id)
                    && model.get("isSelectable").and_then(|value| value.as_bool()) != Some(false)
            })
        })
}

impl Focusable for PaseoView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for PaseoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_selection(cx);
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(connection_picker::render(self, window, cx))
            .child(conversation::render(self, window, cx))
    }
}

#[derive(Default)]
struct StoreState {
    agents: Vec<AgentSummary>,
    selected_agent: Option<String>,
    timeline: BTreeMap<(String, String, u64), TimelineEntry>,
    epochs: BTreeMap<String, String>,
    retired_epochs: BTreeMap<String, BTreeSet<String>>,
    permissions: BTreeMap<String, PermissionRequest>,
    error: Option<String>,
}

impl StoreState {
    fn clear_for_connection(&mut self) {
        self.agents.clear();
        self.selected_agent = None;
        self.timeline.clear();
        self.epochs.clear();
        self.retired_epochs.clear();
        self.permissions.clear();
        self.error = None;
    }

    fn current_epoch(&self, agent_id: &str) -> Option<&str> {
        self.epochs.get(agent_id).map(String::as_str)
    }

    fn begin_epoch(&mut self, agent_id: &str, epoch: &str) -> bool {
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

    fn set_agents(&mut self, agents: Vec<AgentSummary>) {
        self.permissions.clear();
        for agent in &agents {
            if let Some(requests) = agent
                .extra
                .get("pendingPermissions")
                .and_then(|value| value.as_array())
            {
                for request in requests {
                    if let (Some(request_id), Some(title)) = (
                        request.get("id").and_then(|value| value.as_str()),
                        request
                            .get("title")
                            .or_else(|| request.get("name"))
                            .and_then(|value| value.as_str()),
                    ) {
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

    fn set_history(&mut self, agent_id: &str, entries: Vec<TimelineEntry>) {
        for entry in entries {
            if entry.agent_id == agent_id {
                self.insert_projected_entry(entry);
            }
        }
    }

    fn insert_entry(&mut self, entry: TimelineEntry) {
        if !self.begin_epoch(&entry.agent_id, &entry.epoch) {
            return;
        }
        if self.timeline.values().any(|existing| {
            existing.agent_id == entry.agent_id
                && existing.epoch == entry.epoch
                && existing.extra.get("sourceSeqRanges").is_some()
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

    fn insert_projected_entry(&mut self, entry: TimelineEntry) {
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

    fn apply_event(&mut self, event: PaseoEvent) {
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
            PaseoEvent::Disconnected { reason } => self.error = Some(reason),
            PaseoEvent::Connected => self.error = None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_client::{TimelinePage, TimelinePayload};
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

    #[test]
    fn draft_survives_rejection_and_newer_typing() {
        let mut draft = DraftSubmission::default();
        assert_eq!(draft.begin("first"), Some("first".into()));
        assert_eq!(draft.begin("first"), None);
        assert!(!draft.finish(false, "first"));
        assert_eq!(draft.begin("first"), Some("first".into()));
        assert!(!draft.finish(true, "first plus more"));
        assert_eq!(
            draft.begin("first plus more"),
            Some("first plus more".into())
        );
        assert!(draft.finish(true, "first plus more"));
    }

    #[test]
    fn model_choice_must_exist_on_current_provider() {
        let provider = Provider {
            id: "codex".into(),
            label: None,
            status: "ready".into(),
            extra: json!({"models":[
                {"id":"current"},
                {"id":"disabled","isSelectable":false}
            ]}),
        };
        assert!(model_is_selectable(&provider, "current"));
        assert!(!model_is_selectable(&provider, "old-host-model"));
        assert!(!model_is_selectable(&provider, "disabled"));
    }

    #[test]
    fn stale_refresh_cannot_replace_new_host_agents() {
        let mut store = PaseoStore::default();
        store.connection_generation = 2;
        assert!(!store.apply_refresh(
            1,
            Vec::new(),
            vec![AgentSummary {
                id: "old".into(),
                title: None,
                status: "idle".into(),
                directory: None,
                extra: json!({}),
            }]
        ));
        assert!(store.state.agents.is_empty());
    }

    #[test]
    fn connection_request_clears_old_host_before_profile_write_completes() {
        let mut store = PaseoStore::default();
        store.state.selected_agent = Some("old".into());
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
        assert!(store.connecting);
        assert!(store.state.selected_agent.is_none());
        assert!(store.state.timeline.is_empty());
        assert!(!store.is_current_connection(first));
    }

    #[test]
    fn older_page_cannot_change_another_agents_cursor() {
        let mut store = PaseoStore::default();
        store.connection_generation = 3;
        store.state.selected_agent = Some("new".into());
        store.older_cursor = Some(TimelineCursor {
            epoch: "new-epoch".into(),
            sequence: 3,
        });
        let requested_cursor = TimelineCursor {
            epoch: "old-epoch".into(),
            sequence: 7,
        };
        let page = TimelinePage {
            epoch: "old-epoch".into(),
            entries: vec![entry("old", "old-epoch", 6)],
            start_cursor: Some(TimelineCursor {
                epoch: "old-epoch".into(),
                sequence: 6,
            }),
            end_cursor: None,
            has_older: false,
            has_newer: true,
        };
        assert!(!store.apply_older_page(3, "old", &requested_cursor, page));
        assert_eq!(
            store.older_cursor.as_ref().map(|cursor| cursor.sequence),
            Some(3)
        );
        assert!(store.state.timeline.is_empty());
    }

    #[test]
    fn windows_remote_workspaces_are_absolute() {
        assert!(is_absolute_workspace_path(r"C:\repo\agent"));
        assert!(is_absolute_workspace_path(r"\\server\share\agent"));
        assert!(!is_absolute_workspace_path(r"repo\agent"));
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
        state.selected_agent = Some("agent".into());
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
        assert!(state.agents.is_empty());
        assert!(state.selected_agent.is_none());
        assert!(state.error.is_none());
        state.apply_event(PaseoEvent::Disconnected {
            reason: "network unavailable".into(),
        });
        assert_eq!(state.error.as_deref(), Some("network unavailable"));
        state.apply_event(PaseoEvent::Connected);
        assert!(state.error.is_none());
    }

    #[test]
    fn live_entries_and_history_share_one_timeline() {
        let mut state = StoreState::default();
        state.selected_agent = Some("agent".into());
        let entry = TimelineEntry {
            agent_id: "agent".into(),
            epoch: "epoch".into(),
            sequence: 12,
            timestamp: "2026-09-25T00:00:00Z".into(),
            payload: TimelinePayload::Message(json!({"text": "hello"})),
            extra: json!({}),
        };
        state.apply_event(PaseoEvent::TimelineEntry(entry.clone()));
        let newer = TimelineEntry {
            sequence: 13,
            ..entry.clone()
        };
        state.apply_event(PaseoEvent::TimelineEntry(newer));
        state.set_history("agent", vec![entry]);
        assert_eq!(state.timeline.len(), 2);
    }

    #[test]
    fn snapshot_permissions_are_available_before_live_events() {
        let mut state = StoreState::default();
        state.set_agents(vec![AgentSummary {
            id: "agent".into(),
            title: None,
            status: "idle".into(),
            directory: None,
            extra: json!({"pendingPermissions":[{"id":"request", "title":"Use tool"}]}),
        }]);
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
}
