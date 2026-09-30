mod agent_edits;
mod agent_view;
mod attention;
mod choice_picker;
mod command_center;
mod composer;
mod connection_picker;
mod daemon;
mod dictation;
mod editor_context;
mod history;
mod last_turn;
mod sidebar;
mod store;
mod stream;
mod terminal;
mod timeline;
mod title_bar_items;
mod usage;
mod workspace_tabs;
mod workspace_tools;
mod worktrees;

pub use title_bar_items::title_bar_items;

use anyhow::{Result, anyhow};
use db::kvp::KeyValueStore;
use gpui::{
    Action, App, AppContext as _, AsyncApp, Context, Entity, Global, Pixels, Task, TaskExt, Window,
    actions, px,
};
use paseo_client::{ConnectionTarget, is_absolute_workspace_path};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{PaseoConnectionProfile, RegisterSetting, Settings};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use theme_settings::ThemeSettings;
use workspace::{
    MultiWorkspace, Pane, Toast, Workspace, item::ItemEvent, notifications::NotificationId,
};

pub use agent_edits::{AgentEditsToolbar, KeepAllEdits, KeepEdit, RejectAllEdits, RejectEdit};
pub use agent_view::{AgentTab, AgentView, agent_tab_menu};
pub use sidebar::PaseoPanel;
use store::{PaseoStore, StoreEvent};
pub use usage::UsageStatusItem;

actions!(
    paseo_ui,
    [
        /// Shows or hides the Paseo sidebar.
        TogglePanel,
        /// Opens the most recent Paseo agent, or a new agent draft.
        OpenTab,
        /// Opens the selected Paseo agent's workspace in the editor.
        OpenWorkspace,
        /// Starts a new Paseo agent in the Paseo workspace this editor workspace shows.
        NewAgent,
        /// Starts a new Paseo workspace with a new agent.
        NewAgentWorkspace,
        /// Moves focus to the message composer.
        FocusComposer,
        /// Interrupts the running agent turn.
        InterruptAgent,
        /// Archives the current agent.
        ArchiveAgent,
        /// Renames the current agent.
        RenameAgent,
        /// Copies the current agent's ID.
        CopyAgentId,
        /// Switches to the agent's next permission mode.
        CycleMode,
        /// Chooses the agent's model.
        ToggleModelPicker,
        /// Chooses the agent's permission mode.
        ToggleModePicker,
        /// Chooses the agent's thinking effort.
        ToggleThinkingPicker,
        /// Sends the message, steering a running turn.
        SendMessage,
        /// Queues the message until the running turn finishes.
        QueueMessage,
        /// Scrolls the conversation to the latest message.
        ScrollToBottom,
        /// Groups the sidebar by project or by status.
        ToggleGroupByStatus,
        /// Opens History: every agent the host keeps, active and archived.
        OpenHistory,
        /// Opens the Paseo host settings.
        ManageHosts,
        /// Reconnects to the active Paseo host.
        Reconnect,
        /// Opens the next agent in the sidebar.
        NextAgent,
        /// Opens the previous agent in the sidebar.
        PreviousAgent,
        /// Filters the sidebar's agents.
        FocusSidebarFilter,
        /// Starts a new agent that continues this agent's conversation.
        ForkAgent,
        /// Accepts the first pending permission request.
        AllowPermission,
        /// Denies the first pending permission request.
        DenyPermission,
        /// Shows each provider's plan usage on the connected host.
        OpenProviderUsage,
        /// Shows the connected host's daemon status, providers, and management actions.
        OpenDaemonStatus,
        /// Adds a directory on the Paseo host as a project.
        AddProject,
        /// Creates a directory on the Paseo host and adds it as a project.
        NewProjectDirectory,
        /// Opens the files the agent's latest finished turn changed as a diff tab.
        ReviewLastTurn,
        /// Opens a terminal on the Paseo host in the current agent's directory.
        NewTerminal,
        /// Starts or stops dictating into the message composer.
        ToggleDictation,
    ]
);

/// Opens an agent, as the command palette's agent results do.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = paseo_ui)]
#[serde(deny_unknown_fields)]
pub struct OpenAgentById {
    pub agent_id: String,
}

/// Opens one of an agent folder's daemon terminals, as the command palette's terminal results do.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = paseo_ui)]
#[serde(deny_unknown_fields)]
pub struct OpenPaseoTerminal {
    pub directory: String,
    pub terminal_id: String,
}

/// Opens the agent at a sidebar position (1-based, like Paseo's Cmd+1..9).
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = paseo_ui)]
#[serde(deny_unknown_fields)]
pub struct OpenAgentAtIndex {
    pub index: usize,
}

#[derive(Clone, RegisterSetting)]
pub(crate) struct PaseoSettings {
    pub profiles: Vec<PaseoConnectionProfile>,
    pub active_profile: String,
    pub chat: ChatSettings,
    pub sidebar: SidebarSettings,
    pub alerts: AlertSettings,
}

/// How agent chats read.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatSettings {
    pub font_family: Option<gpui::SharedString>,
    /// `None` uses the UI font size.
    pub font_size: Option<Pixels>,
    pub line_height: f32,
    pub line_length: u32,
    pub fold_finished_turns: bool,
    pub show_thinking: bool,
}

/// How the sidebar lists workspaces and agents.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarSettings {
    /// `None` keeps the choice last made in the sidebar's grouping menu.
    pub grouping: Option<settings::PaseoSidebarGrouping>,
    pub title_lines: usize,
    pub animate_status: bool,
}

/// How Zaseo tells the user an agent needs them.
#[derive(Clone, Debug, PartialEq)]
pub struct AlertSettings {
    pub toasts: bool,
    pub bell_count: bool,
    pub system_notifications: bool,
}

impl Settings for PaseoSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let paseo = content.paseo.as_ref();
        let chat = paseo.and_then(|paseo| paseo.chat.as_ref());
        let sidebar = paseo.and_then(|paseo| paseo.sidebar.as_ref());
        let alerts = paseo.and_then(|paseo| paseo.alerts.as_ref());
        Self {
            profiles: paseo
                .and_then(|paseo| paseo.profiles.clone())
                .unwrap_or_default(),
            active_profile: paseo
                .and_then(|paseo| paseo.active_profile.clone())
                .unwrap_or_default(),
            chat: ChatSettings {
                font_family: chat
                    .and_then(|chat| chat.font_family.clone())
                    .map(|family| gpui::SharedString::from(family.0)),
                font_size: chat
                    .and_then(|chat| chat.font_size)
                    .map(|size| theme_settings::clamp_font_size(px(size.0))),
                // Out-of-range values would make the chat unreadable rather than just different.
                line_height: chat
                    .and_then(|chat| chat.line_height)
                    .unwrap_or(1.55)
                    .clamp(1., 3.),
                line_length: chat
                    .and_then(|chat| chat.line_length)
                    .unwrap_or(80)
                    .clamp(40, 400),
                fold_finished_turns: chat
                    .and_then(|chat| chat.fold_finished_turns)
                    .unwrap_or(true),
                show_thinking: chat.and_then(|chat| chat.show_thinking).unwrap_or(true),
            },
            sidebar: SidebarSettings {
                grouping: sidebar.and_then(|sidebar| sidebar.grouping),
                title_lines: sidebar
                    .and_then(|sidebar| sidebar.title_lines)
                    .unwrap_or(2)
                    .clamp(1, 4) as usize,
                animate_status: sidebar
                    .and_then(|sidebar| sidebar.animate_status)
                    .unwrap_or(true),
            },
            alerts: AlertSettings {
                toasts: alerts.and_then(|alerts| alerts.toasts).unwrap_or(true),
                bell_count: alerts.and_then(|alerts| alerts.bell_count).unwrap_or(true),
                system_notifications: alerts
                    .and_then(|alerts| alerts.system_notifications)
                    .unwrap_or(true),
            },
        }
    }
}

impl PaseoSettings {
    pub fn active(&self) -> Option<&PaseoConnectionProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.name == self.active_profile)
            .or_else(|| self.profiles.first())
    }
}

struct GlobalPaseoStore(Entity<PaseoStore>);
impl Global for GlobalPaseoStore {}

const CLIENT_ID_KEY: &str = "paseo_client_id";
const PREFERENCES_KEY: &str = "paseo_create_agent_preferences";

pub fn init(cx: &mut App) {
    PaseoSettings::register(cx);
    workspace::register_serializable_item::<AgentTab>(cx);
    let store = cx.new(|cx| {
        let mut store = PaseoStore::default();
        store.load_archived_subagents(cx);
        store.load_reviewed_edits(cx);
        store
    });
    init_system_notifications(&store, cx);
    cx.set_global(GlobalPaseoStore(store));
    editor_context::init(cx);
    agent_edits::init(cx);
    command_center::init_palette_source(cx);
    cx.observe_new(
        |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            cx.observe(&crate::store(cx), |workspace, _, cx| {
                close_restored_tabs_of_other_workspaces(workspace, cx)
            })
            .detach();
            cx.subscribe_self(|workspace, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ItemAdded { .. }) {
                    close_restored_tabs_of_other_workspaces(workspace, cx);
                }
            })
            .detach();
            let workspace_id = cx.entity_id();
            cx.on_release(move |_, cx| workspace_tabs::forget(workspace_id, cx))
                .detach();
            if let Some(window) = window {
                cx.observe_in(&crate::store(cx), window, |workspace, _, window, cx| {
                    workspace_tabs::refresh(workspace, window, cx)
                })
                .detach();
                cx.subscribe_in(
                    &cx.entity(),
                    window,
                    |workspace, _, event: &workspace::Event, window, cx| {
                        if matches!(
                            event,
                            workspace::Event::ItemAdded { .. }
                                | workspace::Event::ActiveItemChanged
                        ) {
                            workspace_tabs::refresh(workspace, window, cx);
                        }
                    },
                )
                .detach();
                cx.subscribe_in(
                    &crate::store(cx),
                    window,
                    |workspace, _, event: &StoreEvent, window, cx| {
                        if let StoreEvent::WorkspaceRemoved {
                            workspace_id,
                            worktree_directory,
                        } = event
                        {
                            paseo_workspace_removed(
                                workspace,
                                workspace_id,
                                worktree_directory.as_deref(),
                                window,
                                cx,
                            );
                        }
                    },
                )
                .detach();
            }
            workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
                workspace.toggle_panel_focus::<PaseoPanel>(window, cx);
            });
            workspace.register_action(|workspace, _: &OpenTab, window, cx| {
                open_tab(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &NewAgent, window, cx| {
                new_agent(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &NewAgentWorkspace, window, cx| {
                open_draft(workspace, None, window, cx);
            });
            workspace.register_action(|workspace, _: &ForkAgent, window, cx| {
                if let Some(agent_id) = current_agent_id(workspace, cx) {
                    fork_agent(workspace, &agent_id, window, cx);
                }
            });
            workspace.register_action(|workspace, action: &OpenAgentById, window, cx| {
                open_agent(workspace, &action.agent_id, true, window, cx);
            });
            workspace.register_action(|workspace, action: &OpenPaseoTerminal, window, cx| {
                let info = terminal::terminals_for(&action.directory, cx)
                    .into_iter()
                    .find(|info| info.id == action.terminal_id);
                match info {
                    Some(info) => terminal::open_terminal(
                        workspace,
                        info,
                        action.directory.clone(),
                        window,
                        cx,
                    ),
                    None => log::info!("Paseo terminal {} is gone", action.terminal_id),
                }
            });
            workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
                match terminal::agent_directory(current_agent_id(workspace, cx), cx) {
                    Some(directory) => terminal::new_terminal(directory, window, cx),
                    None => workspace.show_error(
                        anyhow!("Open a Paseo agent to start a terminal in its directory"),
                        cx,
                    ),
                }
            });
            workspace.register_action(|workspace, _: &ReviewLastTurn, window, cx| {
                last_turn::open_last_turn(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &AddProject, window, cx| {
                workspace_tools::add_project(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &NewProjectDirectory, window, cx| {
                workspace_tools::new_project_directory(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenHistory, window, cx| {
                history::open_history(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenDaemonStatus, window, cx| {
                daemon::open_daemon_status(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenProviderUsage, window, cx| {
                usage::open_usage(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &ManageHosts, window, cx| {
                connection_picker::open_hosts(workspace, window, cx);
            });
            workspace.register_action(|workspace, action: &OpenAgentAtIndex, window, cx| {
                sidebar::open_agent_at_index(workspace, action.index, window, cx);
            });
            workspace.register_action(|workspace, _: &NextAgent, window, cx| {
                sidebar::open_adjacent_agent(workspace, 1, window, cx);
            });
            workspace.register_action(|workspace, _: &PreviousAgent, window, cx| {
                sidebar::open_adjacent_agent(workspace, -1, window, cx);
            });
            workspace.register_action(|_, _: &Reconnect, _, cx| {
                auto_connect(true, cx);
            });
            let paseo_store = crate::store(cx);
            cx.subscribe(&paseo_store, |workspace, _, event: &StoreEvent, cx| {
                let StoreEvent::NeedsAttention { agent_id, message } = event else {
                    return;
                };
                if !PaseoSettings::get_global(cx).alerts.toasts {
                    return;
                }
                let agent_id = agent_id.clone();
                let handle = cx.weak_entity();
                workspace.show_toast(
                    Toast::new(
                        // One id for every agent, so a new alert replaces the last instead of
                        // stacking; the sidebar's bell lists them all.
                        NotificationId::named("paseo-attention".into()),
                        message.clone(),
                    )
                    .on_click("Open", move |window, cx| {
                        if let Err(error) = handle.update(cx, |workspace, cx| {
                            open_agent(workspace, &agent_id, true, window, cx)
                        }) {
                            log::debug!("Paseo workspace closed: {error}");
                        }
                    })
                    .autohide(),
                    cx,
                );
            })
            .detach();
        },
    )
    .detach();
}

/// Prefixes the agent ID in a Paseo system notification's tag.
const SYSTEM_NOTIFICATION_TAG: &str = "paseo-agent:";

/// Raises a system notification for an agent that needs the user while no Zaseo window has focus,
/// and opens that agent when the user clicks it.
fn init_system_notifications(store: &Entity<PaseoStore>, cx: &mut App) {
    cx.subscribe(store, |store, event: &StoreEvent, cx| {
        let StoreEvent::NeedsAttention { agent_id, message } = event else {
            return;
        };
        // A user looking at Zaseo already sees the toast and the bell.
        if cx.active_window().is_some()
            || !PaseoSettings::get_global(cx).alerts.system_notifications
        {
            return;
        }
        let project = store
            .read(cx)
            .agent(agent_id)
            .map(store::agent_project_name)
            .unwrap_or_default();
        cx.show_system_notification(gpui::SystemNotification {
            tag: format!("{SYSTEM_NOTIFICATION_TAG}{agent_id}").into(),
            title: message.clone().into(),
            body: project.into(),
            actions: Vec::new(),
        });
    })
    .detach();
    cx.on_system_notification_response(|response, cx| {
        if let Some(agent_id) = response.tag.strip_prefix(SYSTEM_NOTIFICATION_TAG) {
            open_agent_from_notification(agent_id, cx);
        }
    });
}

fn open_agent_from_notification(agent_id: &str, cx: &mut App) {
    let Some(window) = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<MultiWorkspace>())
        })
    else {
        log::info!("No Zaseo window is open to show the Paseo agent {agent_id}");
        return;
    };
    let result = window.update(cx, |multi_workspace, window, cx| {
        window.activate_window();
        multi_workspace
            .workspace()
            .clone()
            .update(cx, |workspace, cx| {
                open_agent(workspace, agent_id, true, window, cx)
            });
    });
    if let Err(error) = result {
        log::debug!("Zaseo window closed before opening a Paseo agent: {error}");
    }
}

/// The agent in the active tab, else the one most recently focused in a Paseo view.
pub(crate) fn current_agent_id(workspace: &Workspace, cx: &App) -> Option<String> {
    workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<AgentTab>())
        .and_then(|tab| tab.read(cx).agent_id(cx))
        .or_else(|| store(cx).read(cx).focused_agent.clone())
}

/// Connects to the active host at app startup. Kept out of `init` so tests never reach a daemon.
pub fn connect_on_startup(cx: &mut App) {
    auto_connect(false, cx);
}

/// The UI font size shifted by the buffer zoom, so Ctrl +/- zooms the chat along with the editors.
pub(crate) fn chat_font_size(cx: &App) -> Pixels {
    let settings = ThemeSettings::get_global(cx);
    let base = PaseoSettings::get_global(cx)
        .chat
        .font_size
        .unwrap_or_else(|| settings.ui_font_size(cx));
    theme_settings::clamp_font_size(
        base + settings.buffer_font_size(cx) - settings.buffer_font_size_settings(),
    )
}

/// A spinner beside a muted message, for a tab still waiting on the host.
pub(crate) fn render_loading(message: impl Into<gpui::SharedString>) -> gpui::AnyElement {
    use ui::{CommonAnimationExt as _, prelude::*};
    h_flex()
        .gap_2()
        .py_4()
        .child(
            Icon::new(IconName::LoadCircle)
                .size(IconSize::Small)
                .color(Color::Muted)
                .with_rotate_animation(2),
        )
        .child(Label::new(message.into()).color(Color::Muted))
        .into_any_element()
}

pub(crate) fn store(cx: &App) -> Entity<PaseoStore> {
    cx.global::<GlobalPaseoStore>().0.clone()
}

/// A stable client ID for profiles saved without one, so reconnects keep daemon-side ownership.
pub(crate) fn client_id_for(profile: &PaseoConnectionProfile, cx: &App) -> String {
    if !profile.client_id.trim().is_empty() {
        return profile.client_id.clone();
    }
    let kvp = KeyValueStore::global(cx);
    match kvp.read_kvp(CLIENT_ID_KEY) {
        Ok(Some(client_id)) if !client_id.is_empty() => client_id,
        _ => {
            let client_id = uuid::Uuid::new_v4().to_string();
            let stored = client_id.clone();
            db::write_and_log(cx, move || async move {
                kvp.write_kvp(CLIENT_ID_KEY.to_string(), stored).await
            });
            client_id
        }
    }
}

/// Connects to the active profile, like Paseo's desktop app does with its local daemon.
pub(crate) fn auto_connect(force: bool, cx: &mut App) {
    let Some(mut profile) = PaseoSettings::get_global(cx).active().cloned() else {
        return;
    };
    profile.client_id = client_id_for(&profile, cx);
    let store = store(cx);
    store.update(cx, |store, cx| {
        if !force && store.status != store::ConnectionStatus::Disconnected {
            return;
        }
        let generation = store.begin_connection(profile.clone());
        store.connect(profile, None, generation, cx);
    });
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProviderPreference {
    pub model: Option<String>,
    pub mode: Option<String>,
    #[serde(default)]
    pub thinking_by_model: HashMap<String, String>,
}

/// Last new-agent choices, like Paseo's `@paseo:create-agent-preferences`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CreatePreferences {
    pub provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderPreference>,
    pub directory: Option<String>,
    /// Paseo's "Isolation" choice: start new agents in their own git worktree.
    #[serde(default)]
    pub new_worktree: bool,
}

impl CreatePreferences {
    pub fn load(cx: &App) -> Self {
        KeyValueStore::global(cx)
            .read_kvp(PREFERENCES_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, cx: &App) {
        let json = match serde_json::to_string(self) {
            Ok(json) => json,
            Err(error) => {
                log::error!("Failed to serialize Paseo preferences: {error}");
                return;
            }
        };
        let kvp = KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            kvp.write_kvp(PREFERENCES_KEY.to_string(), json).await
        });
    }
}

pub struct SelectedWorkspace {
    pub directory: PathBuf,
    pub target: ConnectionTarget,
}

/// The directory and host of the agent most recently focused in a Paseo view.
pub fn selected_workspace(cx: &App) -> Result<Option<SelectedWorkspace>> {
    let Some(agent_id) = store(cx).read(cx).focused_agent.clone() else {
        return Ok(None);
    };
    agent_workspace(&agent_id, cx)
}

/// The directory and host of an agent, or `None` when the agent isn't known.
pub fn agent_workspace(agent_id: &str, cx: &App) -> Result<Option<SelectedWorkspace>> {
    let store = store(cx);
    let store = store.read(cx);
    let Some(agent) = store.agent(agent_id) else {
        return Ok(None);
    };
    let directory = agent
        .directory
        .clone()
        .ok_or_else(|| anyhow!("Selected Paseo agent has no workspace directory"))?;
    directory_workspace(directory, cx).map(Some)
}

/// The directory and host of a Paseo folder on the active connection.
fn directory_workspace(directory: PathBuf, cx: &App) -> Result<SelectedWorkspace> {
    if !directory.to_str().is_some_and(is_absolute_workspace_path) {
        return Err(anyhow!("Paseo workspace directory is not absolute"));
    }
    let store = store(cx).read(cx);
    let profile = store
        .active_profile
        .as_ref()
        .ok_or_else(|| anyhow!("No Paseo connection profile is active"))?;
    Ok(SelectedWorkspace {
        directory,
        target: connection_picker::parse_target(profile)?,
    })
}

/// Runs on the target workspace before the window shows it, so it appears with the agent's tab
/// already open instead of drawing its old contents first.
pub type WorkspaceInit =
    Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send>;

/// How a project switch treats the windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchMode {
    /// Shows the workspace, bringing its window forward.
    Activate,
    /// Finds or opens the workspace without changing what any window shows.
    Background,
}

/// Finds the editor workspace that holds an agent's directory, in any window with the current
/// one first, and opens it in the current window when none does. Resolves to that workspace,
/// which may be the current one, or fails when it can't be opened. The app registers it because
/// it owns opening local and SSH projects. `init` may be skipped (for example for SSH projects),
/// so callers still open the tab afterwards.
pub type ProjectSwitcher = Rc<
    dyn Fn(
        SelectedWorkspace,
        Option<WorkspaceInit>,
        SwitchMode,
        &mut Workspace,
        &mut Window,
        &mut Context<Workspace>,
    ) -> Task<Result<Entity<Workspace>>>,
>;

/// Whether a workspace holds an agent's directory, on the same host.
pub type WorkspaceOwnsAgent = Rc<dyn Fn(&SelectedWorkspace, &Workspace, &App) -> bool>;

fn open_agent_init(agent_id: &str, focus: bool) -> WorkspaceInit {
    let agent_id = agent_id.to_owned();
    Box::new(move |workspace, window, cx| {
        open_agent_here(workspace, &agent_id, focus, window, cx);
    })
}

#[derive(Clone)]
struct GlobalProjectSwitcher {
    switch: ProjectSwitcher,
    owns: WorkspaceOwnsAgent,
}

impl Global for GlobalProjectSwitcher {}

pub fn set_project_switcher(switch: ProjectSwitcher, owns: WorkspaceOwnsAgent, cx: &mut App) {
    cx.set_global(GlobalProjectSwitcher { switch, owns });
}

fn project_switcher(cx: &App) -> Result<GlobalProjectSwitcher> {
    cx.try_global::<GlobalProjectSwitcher>()
        .cloned()
        .ok_or_else(|| anyhow!("Zaseo can't open Paseo workspaces in this window"))
}

/// The editor project's directory, only when the daemon runs on this machine and can use it.
fn project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    if !store(cx).read(cx).is_local_host() {
        return None;
    }
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

/// The workspace's tab for an agent, in any pane.
fn agent_tab_in(workspace: &Workspace, agent_id: &str, cx: &App) -> Option<Entity<AgentTab>> {
    workspace
        .items_of_type::<AgentTab>(cx)
        .find(|tab| tab.read(cx).agent_id(cx).as_deref() == Some(agent_id))
}

/// Opens an agent's tab in the agent's own workspace, switching the window (or bringing another
/// window forward) to it like Paseo does. A workspace only ever shows its own agents, so when the
/// agent's workspace can't be opened this shows the error and stays.
pub fn open_agent(
    workspace: &mut Workspace,
    agent_id: &str,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if agent_tab_in(workspace, agent_id, cx).is_some() {
        open_agent_here(workspace, agent_id, focus, window, cx);
        return;
    }
    let switched = project_switcher(cx).and_then(|switcher| {
        let selected = agent_workspace(agent_id, cx)?
            .ok_or_else(|| anyhow!("Paseo hasn't loaded this agent yet"))?;
        let init = open_agent_init(agent_id, focus);
        Ok((switcher.switch)(
            selected,
            Some(init),
            SwitchMode::Activate,
            workspace,
            window,
            cx,
        ))
    });
    let switch = match switched {
        Ok(switch) => switch,
        Err(error) => {
            let error = error.context("Could not open the agent's workspace in the editor");
            show_open_error(workspace, &error, cx);
            return;
        }
    };
    let agent_id = agent_id.to_owned();
    cx.spawn_in(window, async move |workspace, cx| match switch.await {
        Ok(target) => update_in_own_window(&target, cx, |target, window, cx| {
            open_agent_here(target, &agent_id, focus, window, cx);
        }),
        Err(error) => workspace.update(cx, |workspace, cx| {
            let error = error.context("Could not open the agent's workspace in the editor");
            show_open_error(workspace, &error, cx)
        }),
    })
    .detach_and_log_err(cx);
}

/// Shows a just-created agent's Paseo workspace, and moves its tab into the agent's project when
/// the agent runs outside the current one, such as in a new worktree, the way Paseo moves into a
/// new workspace.
pub(crate) fn follow_created_agent(
    workspace: &mut Workspace,
    tab: Entity<AgentTab>,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace_owns_agent(workspace, agent_id, cx)
        && let Some(paseo_workspace_id) = workspace_tabs::agent_paseo_workspace(agent_id, cx)
    {
        workspace_tabs::show(workspace, &paseo_workspace_id, window, cx);
    }
    let Ok(switcher) = project_switcher(cx) else {
        return;
    };
    let selected = match agent_workspace(agent_id, cx) {
        Ok(Some(selected)) => selected,
        Ok(None) => return,
        Err(error) => {
            log::debug!("Paseo agent {agent_id} has no project to move to: {error}");
            return;
        }
    };
    let init = open_agent_init(agent_id, true);
    let switch = (switcher.switch)(
        selected,
        Some(init),
        SwitchMode::Activate,
        workspace,
        window,
        cx,
    );
    let agent_id = agent_id.to_owned();
    cx.spawn_in(window, async move |workspace, cx| {
        let target = match switch.await {
            Ok(target) => target,
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    let error = error.context("Could not open the agent's workspace in the editor");
                    show_open_error(workspace, &error, cx)
                })?;
                return anyhow::Ok(());
            }
        };
        if target.downgrade() == workspace {
            return Ok(());
        }
        update_in_own_window(&target, cx, |target, window, cx| {
            open_agent_here(target, &agent_id, true, window, cx)
        })?;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace_tabs::detach_tab(workspace, &tab, window, cx)
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Keeps Paseo the same across a window's project switch: chats left in a workspace with no
/// folders, sidebar width, and the sidebar left behind, which no longer sees the pointer leave
/// and would hold its reorders.
pub fn workspace_switched(
    previous: &Entity<Workspace>,
    active: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    move_tabs_out_of_empty_workspace(previous, active, window, cx);
    carry_sidebar_size(previous, active, window, cx);
    if let Some(panel) = previous.read(cx).panel::<PaseoPanel>(cx) {
        panel.update(cx, |panel, cx| panel.set_pointer_inside(false, cx));
    }
}

/// Shows why an agent or folder couldn't open, with every cause in the error's chain: a
/// workspace error toast shows only the outermost context.
fn show_open_error(workspace: &mut Workspace, error: &anyhow::Error, cx: &mut Context<Workspace>) {
    workspace.show_error(anyhow!("{error:#}"), cx);
}

/// Whether `workspace` holds `agent_id`'s folder. `false` while the store doesn't know the agent.
pub(crate) fn workspace_owns_agent(workspace: &Workspace, agent_id: &str, cx: &App) -> bool {
    let Ok(switcher) = project_switcher(cx) else {
        return false;
    };
    matches!(
        agent_workspace(agent_id, cx),
        Ok(Some(selected)) if (switcher.owns)(&selected, workspace, cx)
    )
}

/// The folder a Paseo workspace works in, from its descriptor or, before that loads, its agents.
fn paseo_workspace_directory(paseo_workspace_id: &str, cx: &App) -> Option<PathBuf> {
    let store = store(cx).read(cx);
    store
        .state
        .workspaces
        .get(paseo_workspace_id)
        .map(|descriptor| descriptor.directory.clone())
        .or_else(|| {
            store
                .state
                .agents
                .iter()
                .find(|agent| store::agent_workspace_id(agent) == Some(paseo_workspace_id))
                .and_then(|agent| agent.directory.clone())
        })
}

/// Whether `workspace` holds the folder a Paseo workspace works in, on the same host.
pub(crate) fn workspace_holds_paseo_workspace(
    workspace: &Workspace,
    paseo_workspace_id: &str,
    cx: &App,
) -> bool {
    let Ok(switcher) = project_switcher(cx) else {
        return false;
    };
    paseo_workspace_directory(paseo_workspace_id, cx)
        .and_then(|directory| directory_workspace(directory, cx).ok())
        .is_some_and(|selected| (switcher.owns)(&selected, workspace, cx))
}

/// Closes restored tabs whose agent belongs to another workspace, checking each tab once the
/// store knows its agent. Tabs restore before the daemon's agent list arrives, so this runs both
/// when a tab is added and when the store changes.
fn close_restored_tabs_of_other_workspaces(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Ok(switcher) = project_switcher(cx) else {
        return;
    };
    let pending = workspace
        .items_of_type::<AgentTab>(cx)
        .filter(|tab| tab.read(cx).owner_check_pending)
        .collect::<Vec<_>>();
    for tab in pending {
        let owned = match tab
            .read(cx)
            .agent_id(cx)
            .map(|agent_id| agent_workspace(&agent_id, cx))
        {
            Some(Ok(None)) => continue,
            Some(Ok(Some(selected))) => (switcher.owns)(&selected, workspace, cx),
            Some(Err(error)) => {
                log::debug!("Keeping a restored Paseo tab with no editor folder: {error}");
                true
            }
            None => true,
        };
        tab.update(cx, |tab, cx| {
            tab.owner_check_pending = false;
            if !owned {
                tab.leaving = true;
                cx.emit(ItemEvent::CloseItem);
            }
        });
    }
}

/// Moves the chats out of a workspace with no folders when the window switches away from it:
/// they have no folder to stay with, and Zed closes that workspace once a folder opens in it.
/// Each agent goes to its own workspace, found or opened without switching the window; drafts
/// with no agent yet, and agents whose workspace can't open, go to `target`.
///
/// Each chat moves as a new tab around the same view: a workspace subscribes to a tab each time
/// it is added and drops those subscriptions only when the tab is released.
fn move_tabs_out_of_empty_workspace(
    source: &Entity<Workspace>,
    target: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    if !source.read(cx).root_paths(cx).is_empty() {
        return;
    }
    let (moved, source_active_view) = source.update(cx, |source, cx| {
        let tabs = source.items_of_type::<AgentTab>(cx).collect::<Vec<_>>();
        let source_active_view = source
            .active_item(cx)
            .and_then(|item| item.downcast::<AgentTab>())
            .map(|tab| tab.read(cx).view().clone());
        for tab in &tabs {
            workspace_tabs::detach_tab(source, tab, window, cx);
        }
        let views = tabs
            .iter()
            .map(|tab| {
                let tab = tab.read(cx);
                (tab.view().clone(), tab.owner_check_pending)
            })
            .collect::<Vec<_>>();
        (views, source_active_view)
    });
    if moved.is_empty() {
        return;
    }
    let switcher = project_switcher(cx).ok();
    let mut stay = Vec::new();
    // One switch per folder, so two agents from a folder that isn't open don't open it twice.
    let mut homes: Vec<(SelectedWorkspace, Vec<Entity<AgentView>>)> = Vec::new();
    for (view, owner_check_pending) in moved {
        let home = switcher.as_ref().and_then(|switcher| {
            let agent_id = view.read(cx).agent_id.clone()?;
            let selected = agent_workspace(&agent_id, cx).ok().flatten()?;
            (!(switcher.owns)(&selected, target.read(cx), cx)).then_some(selected)
        });
        let Some(selected) = home else {
            stay.push((view, owner_check_pending));
            continue;
        };
        match homes
            .iter_mut()
            .find(|(home, _)| home.directory == selected.directory)
        {
            Some((_, views)) => views.push(view),
            None => homes.push((selected, vec![view])),
        }
    }
    if let Some(switcher) = switcher {
        for (selected, views) in homes {
            let switch = target.update(cx, |target, cx| {
                (switcher.switch)(selected, None, SwitchMode::Background, target, window, cx)
            });
            let target = target.clone();
            window
                .spawn(cx, async move |cx| {
                    let (home, error) = match switch.await {
                        Ok(home) => (home, None),
                        Err(error) => (target, Some(error)),
                    };
                    update_in_own_window(&home, cx, |home, window, cx| {
                        if let Some(error) = error {
                            let error =
                                error.context("Could not open the agent's workspace in the editor");
                            show_open_error(home, &error, cx);
                        }
                        let pane = home.active_pane().clone();
                        for view in views {
                            add_moved_chat(home, &pane, view, false, window, cx);
                        }
                    })
                })
                .detach_and_log_err(cx);
        }
    }
    if stay.is_empty() {
        return;
    }
    target.update(cx, |target, cx| {
        let pane = target.active_pane().clone();
        for (view, owner_check_pending) in stay {
            add_moved_chat(target, &pane, view, owner_check_pending, window, cx);
        }
        pane.update(cx, |pane, cx| {
            let index = source_active_view.and_then(|view| {
                pane.items().position(|item| {
                    item.downcast::<AgentTab>()
                        .is_some_and(|tab| tab.read(cx).view() == &view)
                })
            });
            if let Some(index) = index {
                pane.activate_item(index, false, false, window, cx);
            }
        });
    });
}

/// Adds a moved chat to `pane`. When `workspace` already has a tab for its agent, the one
/// without unsent text gives way; if both have some, both stay open.
fn add_moved_chat(
    workspace: &mut Workspace,
    pane: &Entity<Pane>,
    view: Entity<AgentView>,
    owner_check_pending: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = view
        .read(cx)
        .agent_id
        .clone()
        .and_then(|agent_id| agent_tab_in(workspace, &agent_id, cx));
    if let Some(existing) = existing {
        if !view.read(cx).has_unsent_text(cx) {
            return;
        }
        if !existing.read(cx).view().read(cx).has_unsent_text(cx) {
            workspace_tabs::detach_tab(workspace, &existing, window, cx);
        }
    }
    add_chat_view(pane, view, owner_check_pending, window, cx);
}

/// Runs `update` on a workspace inside the window that shows it, which may not be the one the
/// caller runs in.
fn update_in_own_window<R>(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncApp,
    update: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
) -> Result<R> {
    let window = cx
        .update(|cx| {
            cx.windows().into_iter().find_map(|window| {
                let window = window.downcast::<MultiWorkspace>()?;
                window
                    .read(cx)
                    .ok()?
                    .workspaces()
                    .any(|candidate| candidate == workspace)
                    .then_some(window)
            })
        })
        .ok_or_else(|| anyhow!("The Paseo agent's workspace is no longer open"))?;
    window.update(cx, |_, window, cx| {
        workspace.update(cx, |workspace, cx| update(workspace, window, cx))
    })
}

fn add_chat_view(
    pane: &Entity<Pane>,
    view: Entity<AgentView>,
    owner_check_pending: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tab = AgentTab::for_view(view, cx.weak_entity(), owner_check_pending, cx);
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(tab), false, false, None, window, cx)
    });
}

/// The Paseo sidebar size last set, shared by every project because Zed saves dock sizes per
/// workspace.
struct SharedSidebarSize(workspace::dock::PanelSizeState);

impl Global for SharedSidebarSize {}

/// Records `workspace`'s Paseo sidebar size as the shared one.
pub(crate) fn remember_sidebar_size(workspace: &Entity<Workspace>, cx: &mut App) {
    let workspace = workspace.read(cx);
    let size = workspace.panel::<PaseoPanel>(cx).and_then(|panel| {
        workspace
            .all_docks()
            .into_iter()
            .find_map(|dock| dock.read(cx).stored_panel_size_state(&panel))
    });
    if let Some(size) = size {
        cx.set_global(SharedSidebarSize(size));
    }
}

fn carry_sidebar_size(
    source: &Entity<Workspace>,
    target: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    remember_sidebar_size(source, cx);
    target.update(cx, |target, cx| apply_sidebar_size(target, window, cx));
}

/// Applies the shared Paseo sidebar size, such as once a workspace's panels are added.
pub fn apply_sidebar_size(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(size) = cx.try_global::<SharedSidebarSize>().map(|shared| shared.0) {
        workspace.set_panel_size_state::<PaseoPanel>(size, window, cx);
    }
}

pub(crate) fn new_agent_tab(
    workspace: &Workspace,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let directory = project_directory(workspace, cx);
    let workspace_handle = Some(cx.weak_entity());
    let agent_id = agent_id.to_owned();
    cx.new(|cx| AgentTab::new(Some(agent_id), directory, workspace_handle, window, cx))
}

pub(crate) fn open_agent_here(
    workspace: &mut Workspace,
    agent_id: &str,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    workspace_tabs::prepare_to_open(workspace, agent_id, window, cx);
    if let Some(tab) = agent_tab_in(workspace, agent_id, cx) {
        workspace.activate_item(&tab, true, focus, window, cx);
        if focus {
            focus_tab_composer(&tab, window, cx);
        }
        return tab;
    }
    let tab = new_agent_tab(workspace, agent_id, window, cx);
    workspace.add_item_to_active_pane(Box::new(tab.clone()), None, focus, window, cx);
    if focus {
        focus_tab_composer(&tab, window, cx);
    }
    tab
}

/// Opens a provider subagent's conversation in a read-only agent tab, reusing an open one.
pub(crate) fn open_subagent(
    workspace: &mut Workspace,
    parent_agent_id: &str,
    subagent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let timeline_id = paseo_client::subagent_timeline_id(parent_agent_id, subagent_id);
    if let Some(tab) = agent_tab_in(workspace, &timeline_id, cx) {
        workspace.activate_item(&tab, true, true, window, cx);
        return;
    }
    let workspace_handle = Some(cx.weak_entity());
    let tab = cx.new(|cx| AgentTab::new(Some(timeline_id), None, workspace_handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
}

pub fn open_draft(
    workspace: &mut Workspace,
    directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let directory = directory.or_else(|| project_directory(workspace, cx));
    let workspace_handle = Some(cx.weak_entity());
    let tab = cx.new(|cx| AgentTab::new(None, directory, workspace_handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(tab.clone()), None, true, window, cx);
    focus_tab_composer(&tab, window, cx);
    tab
}

/// Starts a draft whose agent joins `paseo_workspace_id`, or starts a new Paseo workspace when
/// that is `None`, and shows that Paseo workspace here.
pub(crate) fn open_draft_joining(
    workspace: &mut Workspace,
    directory: Option<PathBuf>,
    paseo_workspace_id: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let tab = open_draft(workspace, directory, window, cx);
    if let Some(paseo_workspace_id) = paseo_workspace_id {
        join_paseo_workspace(workspace, &tab, &paseo_workspace_id, window, cx);
    }
    tab
}

fn join_paseo_workspace(
    workspace: &mut Workspace,
    tab: &Entity<AgentTab>,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let composer = tab.read(cx).view().read(cx).composer.clone();
    composer.update(cx, |composer, cx| {
        composer.draft_workspace_id = Some(paseo_workspace_id.to_owned());
        cx.notify();
    });
    workspace_tabs::show(workspace, paseo_workspace_id, window, cx);
}

/// Starts a new agent in the Paseo workspace this editor workspace shows, like Paseo's new tab,
/// or a new Paseo workspace when it shows none.
pub fn new_agent(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    match workspace_tabs::shown_paseo_workspace(workspace, cx) {
        Some(paseo_workspace_id) => {
            new_agent_in_paseo_workspace(workspace, &paseo_workspace_id, window, cx)
                .detach_and_log_err(cx);
        }
        None => {
            open_draft(workspace, None, window, cx);
        }
    }
}

/// Starts a new agent in a Paseo workspace, in its folder's editor workspace.
pub(crate) fn new_agent_in_paseo_workspace(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<AgentTab>>> {
    let Some(directory) = paseo_workspace_directory(paseo_workspace_id, cx) else {
        let error = anyhow!("Paseo hasn't loaded this workspace yet");
        show_open_error(workspace, &error, cx);
        return Task::ready(Err(error));
    };
    let draft = open_draft_in(workspace, directory, window, cx);
    let paseo_workspace_id = paseo_workspace_id.to_owned();
    cx.spawn_in(window, async move |_, cx| {
        let tab = draft.await?;
        let target = tab
            .read_with(cx, |tab, cx| tab.view().read(cx).workspace.clone())
            .and_then(|workspace| workspace.upgrade())
            .ok_or_else(|| anyhow!("The draft's workspace is no longer open"))?;
        update_in_own_window(&target, cx, |target, window, cx| {
            join_paseo_workspace(target, &tab, &paseo_workspace_id, window, cx)
        })?;
        Ok(tab)
    })
}

/// Shows a Paseo workspace's agent tabs in its folder's editor workspace and opens its most
/// recent agent, like selecting a workspace in Paseo.
pub fn open_paseo_workspace_tabs(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let recent = {
        let store = store(cx);
        let store = store.read(cx);
        store
            .state
            .agents
            .iter()
            .filter(|agent| store::agent_workspace_id(agent) == Some(paseo_workspace_id))
            .max_by_key(|agent| store::agent_updated_at(agent))
            .map(|agent| agent.id.clone())
    };
    match recent {
        Some(agent_id) => open_agent(workspace, &agent_id, true, window, cx),
        None => new_agent_in_paseo_workspace(workspace, paseo_workspace_id, window, cx)
            .detach_and_log_err(cx),
    }
}

/// Drops an archived Paseo workspace's chats from `workspace`, and closes `workspace` when it was
/// that workspace's own worktree, whose folder goes away with it.
fn paseo_workspace_removed(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    worktree_directory: Option<&std::path::Path>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let showed = workspace_tabs::paseo_workspace_removed(workspace, paseo_workspace_id, window, cx);
    let own_worktree = worktree_directory.is_some_and(|directory| {
        workspace
            .root_paths(cx)
            .iter()
            .all(|root| **root == *directory)
    }) && !workspace.root_paths(cx).is_empty();
    if !showed || !own_worktree || workspace.items_of_type::<AgentTab>(cx).next().is_some() {
        return;
    }
    let this = cx.entity();
    // Deferred because removing reads this workspace, which is mid-update.
    window.defer(cx, move |window, cx| {
        let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() else {
            return;
        };
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace
                .remove([this], workspace::RemovalIntent::CloseProject, window, cx)
                .detach_and_log_err(cx)
        });
    });
}

/// Starts a new agent draft for `directory` in that folder's own workspace, switching to it (or
/// bringing its window forward) when it isn't this one, and resolves to the draft's tab. Errors
/// are shown on this workspace, which never holds another folder's draft.
pub fn open_draft_in(
    workspace: &mut Workspace,
    directory: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<AgentTab>>> {
    let switched = project_switcher(cx).and_then(|switcher| {
        let selected = directory_workspace(directory.clone(), cx)?;
        if (switcher.owns)(&selected, workspace, cx) {
            return Ok(None);
        }
        Ok(Some((switcher.switch)(
            selected,
            None,
            SwitchMode::Activate,
            workspace,
            window,
            cx,
        )))
    });
    let switch = match switched {
        Ok(None) => return Task::ready(Ok(open_draft(workspace, Some(directory), window, cx))),
        Ok(Some(switch)) => switch,
        Err(error) => {
            let error = error.context("Could not open the folder's workspace in the editor");
            show_open_error(workspace, &error, cx);
            return Task::ready(Err(error));
        }
    };
    cx.spawn_in(window, async move |workspace, cx| match switch.await {
        Ok(target) => update_in_own_window(&target, cx, |target, window, cx| {
            open_draft(target, Some(directory), window, cx)
        }),
        Err(error) => {
            let error = error.context("Could not open the folder's workspace in the editor");
            workspace.update(cx, |workspace, cx| show_open_error(workspace, &error, cx))?;
            Err(error)
        }
    })
}

/// Opens a draft that starts from the agent's conversation, like Paseo's "Fork in a new tab".
pub fn fork_agent(
    _workspace: &mut Workspace,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store = store(cx);
    let Some(agent) = store.read(cx).agent(agent_id).cloned() else {
        return;
    };
    let task = store.update(cx, |store, cx| store.fork_context(agent_id, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let attachment = match task.await {
            Ok(attachment) => attachment,
            Err(error) => {
                workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
                return anyhow::Ok(());
            }
        };
        let paseo_workspace_id = store::agent_workspace_id(&agent).map(str::to_owned);
        let tab = match (paseo_workspace_id, agent.directory.clone()) {
            (Some(paseo_workspace_id), _) => {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        new_agent_in_paseo_workspace(workspace, &paseo_workspace_id, window, cx)
                    })?
                    .await?
            }
            (None, Some(directory)) => {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        open_draft_in(workspace, directory, window, cx)
                    })?
                    .await?
            }
            (None, None) => workspace.update_in(cx, |workspace, window, cx| {
                open_draft(workspace, None, window, cx)
            })?,
        };
        tab.update(cx, |tab, cx| {
            let composer = tab.view().read(cx).composer.clone();
            composer.update(cx, |composer, cx| {
                let value = |key: &str| {
                    agent
                        .extra
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                composer.prefill_fork(
                    attachment,
                    store::agent_title(&agent),
                    composer::AgentChoices {
                        provider: Some(store::agent_provider(&agent).to_owned()),
                        model: value("model"),
                        thinking: value("thinkingOptionId"),
                        mode: value("currentModeId"),
                    },
                    cx,
                );
            });
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn focus_tab_composer(tab: &Entity<AgentTab>, window: &mut Window, cx: &mut App) {
    let handle = tab.read(cx).composer_focus_handle(cx);
    window.focus(&handle, cx);
}

pub fn open_tab(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<AgentTab>(cx).next();
    if let Some(tab) = existing {
        workspace.activate_item(&tab, true, true, window, cx);
        focus_tab_composer(&tab, window, cx);
        return;
    }
    if let Some(paseo_workspace_id) = workspace_tabs::shown_paseo_workspace(workspace, cx) {
        open_paseo_workspace_tabs(workspace, &paseo_workspace_id, window, cx);
        return;
    }
    let recent = {
        let store = store(cx);
        let store = store.read(cx);
        store
            .state
            .agents
            .iter()
            .max_by_key(|agent| store::agent_updated_at(agent))
            .map(|agent| agent.id.clone())
    };
    match recent {
        Some(agent_id) => open_agent(workspace, &agent_id, true, window, cx),
        None => {
            open_draft(workspace, None, window, cx);
        }
    }
}

/// Adds a local agent that edited `path`, replacing `old_text` with `new_text`, for tests of
/// features built on agent timelines.
#[cfg(any(test, feature = "test-support"))]
pub fn test_add_agent_edit(
    agent_id: &str,
    directory: &std::path::Path,
    path: &std::path::Path,
    old_text: &str,
    new_text: &str,
    cx: &mut App,
) {
    let store = store(cx);
    store.update(cx, |store, cx| {
        store.active_profile = Some(PaseoConnectionProfile {
            name: "Local".into(),
            target_uri: "ws://127.0.0.1:6767/ws".into(),
            editor_ssh_uri: None,
            client_id: "test-client".into(),
        });
        let mut agents = store.state.agents.clone();
        agents.push(paseo_client::AgentSummary {
            id: agent_id.into(),
            title: Some("Test agent".into()),
            status: "idle".into(),
            directory: Some(directory.to_path_buf()),
            project: None,
            extra: serde_json::json!({"updatedAt": "2026-09-28T10:00:00Z"}),
        });
        store.state.set_agents(agents);
        store.state.insert_entry(paseo_client::TimelineEntry {
            agent_id: agent_id.into(),
            epoch: "epoch".into(),
            sequence: 1,
            timestamp: "2026-09-28T10:00:00Z".into(),
            payload: paseo_client::TimelinePayload::Tool(serde_json::json!({
                "type": "tool_call",
                "callId": "call-1",
                "name": "edit",
                "status": "completed",
                "detail": {
                    "type": "edit",
                    "filePath": path.display().to_string(),
                    "oldString": old_text,
                    "newString": new_text,
                },
                "error": null,
            })),
            extra: serde_json::json!({}),
        });
        cx.notify();
    });
}

/// Adds a pending permission request, such as an agent's question, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_add_permission(request: paseo_client::PermissionRequest, cx: &mut App) {
    store(cx).update(cx, |store, cx| {
        store
            .state
            .permissions
            .insert(request.request_id.clone(), request);
        cx.notify();
    });
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_add_agent(agent: paseo_client::AgentSummary, cx: &mut App) {
    store(cx).update(cx, |store, cx| {
        store.state.agents.push(agent);
        cx.notify();
    });
}

/// Connects the store to a daemon on this machine, as far as opening workspaces goes, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_use_local_host(cx: &mut App) {
    store(cx).update(cx, |store, cx| {
        store.active_profile = Some(PaseoConnectionProfile {
            name: "Local".into(),
            target_uri: "ws://127.0.0.1:6767/ws".into(),
            editor_ssh_uri: None,
            client_id: "test-client".into(),
        });
        cx.notify();
    });
}

/// Opens an agent's tab in `workspace` without finding its own workspace, the way a draft that
/// became an agent in place ends up there, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_open_agent_here(
    workspace: &mut Workspace,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_agent_here(workspace, agent_id, false, window, cx);
}

/// Adds a Paseo workspace working in `directory`, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_add_paseo_workspace(
    workspace_id: &str,
    directory: &std::path::Path,
    worktree: bool,
    cx: &mut App,
) {
    store(cx).update(cx, |store, cx| {
        store.state.workspaces.insert(
            workspace_id.to_owned(),
            paseo_client::WorkspaceDescriptor {
                id: workspace_id.to_owned(),
                project_id: "prj_test".into(),
                project_display_name: "test".into(),
                project_root_path: directory.to_path_buf(),
                directory: directory.to_path_buf(),
                kind: if worktree { "worktree" } else { "directory" }.into(),
                worktree_slug: None,
                name: workspace_id.to_owned(),
                title: None,
                pinned_at: None,
                labels: Vec::new(),
                status: "done".into(),
                activity_at: None,
                diff_stat: None,
                scripts: Vec::new(),
                current_branch: None,
                is_paseo_worktree: worktree,
                extra: serde_json::Value::Null,
            },
        );
        cx.notify();
    });
}

/// Archives a Paseo workspace and its agents the way the daemon announces it, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_archive_paseo_workspace(workspace_id: &str, cx: &mut App) {
    store(cx).update(cx, |store, cx| {
        store
            .state
            .agents
            .retain(|agent| store::agent_workspace_id(agent) != Some(workspace_id));
        store.handle_event(
            paseo_client::PaseoEvent::WorkspaceRemoved {
                workspace_id: workspace_id.to_owned(),
                removed_project_id: None,
            },
            cx,
        );
    });
}

/// The Paseo workspace a draft tab's agent will join, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_draft_workspace_id(tab: &Entity<AgentTab>, cx: &App) -> Option<String> {
    tab.read(cx)
        .view()
        .read(cx)
        .composer
        .read(cx)
        .draft_workspace_id
        .clone()
}

/// Opens a tab the way restoring a workspace does, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_restore_agent_tab(
    workspace: &mut Workspace,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.weak_entity();
    let agent_id = agent_id.to_owned();
    let tab = cx.new(|cx| AgentTab::restored(agent_id, workspace_handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(tab), None, false, window, cx);
}

/// Records `agent_id` as the agent last focused in a Paseo view, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_set_focused_agent(agent_id: &str, cx: &mut App) {
    store(cx).update(cx, |store, cx| {
        store.set_focused_agent(agent_id.to_owned(), cx)
    });
}

/// The store's error banner, which a request sent with no daemon connection sets, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_store_error(cx: &App) -> Option<String> {
    store(cx).read(cx).state.error.clone()
}

/// The agent the store records as the one the user is looking at, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_focused_agent(cx: &App) -> Option<String> {
    store(cx).read(cx).focused_agent.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn system_notifications_only_while_unfocused(cx: &mut gpui::TestAppContext) {
        let agent = |status: &str| paseo_client::AgentSummary {
            id: "agent".into(),
            title: Some("Fix login".into()),
            status: status.into(),
            directory: None,
            project: None,
            extra: serde_json::json!({}),
        };
        let store = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_app_identity("local.zaseo.test", "Zaseo");
            let store = cx.new(|_| PaseoStore::default());
            init_system_notifications(&store, cx);
            store
        });
        let window = cx.add_empty_window();
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        let finish = |cx: &mut gpui::VisualTestContext| {
            store.update(cx, |store, cx| {
                store.handle_event(
                    paseo_client::PaseoEvent::AgentsChanged(vec![agent("running")]),
                    cx,
                );
                // The agent the user last looked at still alerts once they have left Zaseo.
                store.focused_agent = Some("agent".into());
                store.handle_event(
                    paseo_client::PaseoEvent::AgentsChanged(vec![agent("idle")]),
                    cx,
                );
            });
            cx.run_until_parked();
        };

        finish(window);
        assert!(window.shown_system_notifications().is_empty());

        window.deactivate_window();
        finish(window);
        let shown = window.shown_system_notifications();
        assert_eq!(
            shown
                .iter()
                .map(|notification| (notification.tag.as_ref(), notification.title.as_ref()))
                .collect::<Vec<_>>(),
            [("paseo-agent:agent", "“Fix login” finished")]
        );
    }

    #[gpui::test]
    fn paseo_settings_defaults_match_todays_behaviour(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            let paseo = PaseoSettings::get_global(cx);
            assert_eq!(
                paseo.chat,
                ChatSettings {
                    font_family: None,
                    font_size: None,
                    line_height: 1.55,
                    line_length: 80,
                    fold_finished_turns: true,
                    show_thinking: true,
                }
            );
            assert_eq!(
                paseo.sidebar,
                SidebarSettings {
                    grouping: None,
                    title_lines: 2,
                    animate_status: true,
                }
            );
            assert_eq!(
                paseo.alerts,
                AlertSettings {
                    toasts: true,
                    bell_count: true,
                    system_notifications: true,
                }
            );
        });
    }

    #[gpui::test]
    fn paseo_settings_keep_readable_values(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut store = settings::SettingsStore::test(cx);
            store
                .set_user_settings(
                    r#"{"paseo": {"chat": {"line_height": 9, "line_length": 5, "font_size": 17},
                        "sidebar": {"title_lines": 0, "grouping": "status"}}}"#,
                    cx,
                )
                .result()
                .expect("valid settings");
            cx.set_global(store);
            let paseo = PaseoSettings::get_global(cx);
            assert_eq!(paseo.chat.line_height, 3.);
            assert_eq!(paseo.chat.font_size, Some(px(17.)));
            assert_eq!(
                chat_font_size(cx),
                px(17.),
                "the setting replaces the UI font size"
            );
            assert_eq!(paseo.chat.line_length, 40);
            assert_eq!(paseo.sidebar.title_lines, 1);
            assert_eq!(
                paseo.sidebar.grouping,
                Some(settings::PaseoSidebarGrouping::Status)
            );
        });
    }

    #[test]
    fn bundled_paseo_theme_parses() {
        let family = theme_settings::deserialize_user_theme(include_bytes!(
            "../../../assets/themes/paseo/paseo.json"
        ));
        let names = family
            .map(|family| {
                family
                    .themes
                    .into_iter()
                    .map(|theme| theme.name)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.to_string());
        assert_eq!(
            names,
            Ok(vec!["Paseo Dark".to_owned(), "Paseo Light".to_owned()])
        );
    }
}
