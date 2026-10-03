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
mod hosts;
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
    Action, App, AppContext as _, AsyncApp, Context, Entity, Global, Pixels, Task, TaskExt,
    WeakEntity, Window, actions, px,
};
use paseo_client::{ConnectionTarget, is_absolute_workspace_path};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{PaseoConnectionProfile, RegisterSetting, Settings};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use theme_settings::ThemeSettings;
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, Pane, Toast, Workspace, item::ItemEvent, notifications::NotificationId,
};

pub use agent_edits::{AgentEditsToolbar, KeepAllEdits, KeepEdit, RejectAllEdits, RejectEdit};
#[cfg(any(test, feature = "test-support"))]
pub use agent_edits::{test_locating_agent_edits, test_refresh_agent_edits};
pub use agent_view::{AgentTab, AgentView, agent_tab_menu};
use hosts::HostsEvent;
pub use sidebar::PaseoPanel;
use store::PaseoStore;
#[cfg(any(test, feature = "test-support"))]
pub use timeline::{FileEdit, reverse_edits_tracking};
pub use usage::UsageStatusItem;
// Defined with the shared actions so the Welcome page, which can't depend on this crate, can
// offer them.
pub use zed_actions::paseo::{NewAgent, NewAgentWorkspace};

actions!(
    paseo_ui,
    [
        /// Shows or hides the Paseo sidebar.
        TogglePanel,
        /// Opens the most recent Paseo agent, or a new agent draft.
        OpenTab,
        /// Opens the selected Paseo agent's workspace in the editor.
        OpenWorkspace,
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
        /// Reconnects to every Paseo host.
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

/// Opens the sidebar row at a position (1-based, like Paseo's Cmd+1..9): a workspace, or an agent
/// outside one.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = paseo_ui, deprecated_aliases = ["paseo_ui::OpenAgentAtIndex"])]
#[serde(deny_unknown_fields)]
pub struct OpenSidebarRowAtIndex {
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
                    .unwrap_or(1)
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

const CLIENT_ID_KEY: &str = "paseo_client_id";
const PREFERENCES_KEY: &str = "paseo_create_agent_preferences";

pub fn init(cx: &mut App) {
    PaseoSettings::register(cx);
    workspace::register_serializable_item::<AgentTab>(cx);
    hosts::init(cx);
    init_system_notifications(cx);
    editor_context::init(cx);
    agent_edits::init(cx);
    command_center::init_palette_source(cx);
    cx.observe_new(
        |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            follow_paseo_changes(window, cx);
            register_workspace_actions(workspace);
            show_attention_toasts(cx);
        },
    )
    .detach();
}

/// Keeps a workspace's chats in step with every host and with its own tabs.
fn follow_paseo_changes(window: Option<&mut Window>, cx: &mut Context<Workspace>) {
    cx.subscribe_self(|workspace, event: &workspace::Event, cx| {
        if matches!(event, workspace::Event::ItemAdded { .. }) {
            close_restored_tabs_of_other_workspaces(workspace, cx);
        }
    })
    .detach();
    let workspace_id = cx.entity_id();
    cx.on_release(move |_, cx| workspace_tabs::forget(workspace_id, cx))
        .detach();
    let Some(window) = window else {
        cx.observe(&hosts::registry(cx), |workspace, _, cx| {
            close_restored_tabs_of_other_workspaces(workspace, cx)
        })
        .detach();
        return;
    };
    // One observer for every host change, so each change walks the workspace's tabs once.
    cx.observe_in(&hosts::registry(cx), window, |workspace, _, window, cx| {
        close_restored_tabs_of_other_workspaces(workspace, cx);
        reopen_tabs_on_their_hosts(workspace, window, cx);
        workspace_tabs::refresh(workspace, window, cx)
    })
    .detach();
    cx.subscribe_in(
        &cx.entity(),
        window,
        |workspace, _, event: &workspace::Event, window, cx| {
            if matches!(
                event,
                workspace::Event::ItemAdded { .. } | workspace::Event::ActiveItemChanged
            ) {
                workspace_tabs::refresh(workspace, window, cx);
            }
        },
    )
    .detach();
    cx.subscribe_in(
        &hosts::registry(cx),
        window,
        |workspace, _, event: &HostsEvent, window, cx| {
            if let HostsEvent::WorkspaceRemoved {
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

fn register_workspace_actions(workspace: &mut Workspace) {
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
        workspace_tools::open_new_workspace(workspace, None, window, cx);
    });
    workspace.register_action(|workspace, _: &ForkAgent, window, cx| {
        if let Some((store, agent_id)) = current_agent(workspace, cx) {
            fork_agent(workspace, store, &agent_id, window, cx);
        }
    });
    workspace.register_action(|workspace, action: &OpenAgentById, window, cx| {
        open_agent(workspace, &action.agent_id, true, window, cx);
    });
    workspace.register_action(|workspace, action: &OpenPaseoTerminal, window, cx| {
        let found = hosts::store_for_terminal(&action.terminal_id, cx).and_then(|store| {
            let info = terminal::terminals_for(&store, &action.directory, cx)
                .into_iter()
                .find(|info| info.id == action.terminal_id)?;
            Some((store, info))
        });
        match found {
            Some((store, info)) => terminal::open_terminal(
                workspace,
                store,
                info,
                action.directory.clone(),
                window,
                cx,
            ),
            None => log::info!("Paseo terminal {} is gone", action.terminal_id),
        }
    });
    workspace.register_action(|workspace, _: &NewTerminal, window, cx| {
        match terminal::agent_directory(current_agent(workspace, cx), cx) {
            Some((store, directory)) => terminal::new_terminal(store, directory, window, cx),
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
    workspace.register_action(|workspace, action: &OpenSidebarRowAtIndex, window, cx| {
        sidebar::open_row_at_index(workspace, action.index, window, cx);
    });
    workspace.register_action(|workspace, _: &NextAgent, window, cx| {
        sidebar::open_adjacent_agent(workspace, 1, window, cx);
    });
    workspace.register_action(|workspace, _: &PreviousAgent, window, cx| {
        sidebar::open_adjacent_agent(workspace, -1, window, cx);
    });
    workspace.register_action(|_, _: &Reconnect, _, cx| {
        hosts::connect_all(true, cx);
    });
}

/// Shows a toast when an agent on any host needs the user.
fn show_attention_toasts(cx: &mut Context<Workspace>) {
    cx.subscribe(
        &hosts::registry(cx),
        |workspace, _, event: &HostsEvent, cx| {
            let HostsEvent::NeedsAttention {
                agent_id, message, ..
            } = event
            else {
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
        },
    )
    .detach();
}

/// Prefixes the agent ID in a Paseo system notification's tag.
const SYSTEM_NOTIFICATION_TAG: &str = "paseo-agent:";

/// Raises a system notification for an agent that needs the user while no Zaseo window has focus,
/// and opens that agent when the user clicks it.
fn init_system_notifications(cx: &mut App) {
    cx.subscribe(&hosts::registry(cx), |_, event: &HostsEvent, cx| {
        let HostsEvent::NeedsAttention {
            store,
            agent_id,
            message,
        } = event
        else {
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
pub(crate) fn current_agent(
    workspace: &Workspace,
    cx: &App,
) -> Option<(Entity<PaseoStore>, String)> {
    workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<AgentTab>())
        .and_then(|tab| {
            let view = tab.read(cx).view().read(cx);
            Some((view.store.clone(), view.agent_id.clone()?))
        })
        .or_else(|| {
            let store = hosts::focused_store(cx)?;
            let agent_id = store.read(cx).focused_agent.clone()?;
            Some((store, agent_id))
        })
}

/// Runs `update` on `workspace` once the current update finishes, logging if the workspace closed
/// first. Event handlers and menu entries run while the workspace, its tabs or the window root may
/// be mid-update, and opening or moving tabs reads all of them.
pub(crate) fn defer_workspace_update(
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    update: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    window.defer(cx, move |window, cx| {
        if let Err(error) = workspace.update(cx, |workspace, cx| update(workspace, window, cx)) {
            log::debug!("Paseo workspace closed: {error}");
        }
    });
}

/// Connects to every host at app startup. Kept out of `init` so tests never reach a daemon.
pub fn connect_on_startup(cx: &mut App) {
    hosts::start_connecting(cx);
}

/// Zed's label size (seven eighths of the UI font size), shifted by the buffer zoom so Ctrl +/-
/// zooms the chat along with the editors. The UI font size itself reads larger than the panels
/// around the chat, whose labels use the smaller size.
pub(crate) fn chat_font_size(cx: &App) -> Pixels {
    let settings = ThemeSettings::get_global(cx);
    let base = PaseoSettings::get_global(cx)
        .chat
        .font_size
        .unwrap_or_else(|| (settings.ui_font_size(cx) * 0.875).round());
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

/// `text` with its first character in upper case, for ids shown as names.
pub(crate) fn capitalize_first(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// An empty or unavailable state: a muted message, an optional hint, and an optional action.
pub(crate) fn render_message(
    title: impl Into<gpui::SharedString>,
    hint: Option<gpui::SharedString>,
    action: Option<gpui::AnyElement>,
) -> gpui::AnyElement {
    use ui::prelude::*;
    v_flex()
        .debug_selector(|| "paseo-message".into())
        .gap_1()
        .py_4()
        .child(Label::new(title.into()).color(Color::Muted))
        .when_some(hint, |this, hint| {
            this.child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
        })
        .when_some(action, |this, action| {
            this.child(div().pt_1().child(action))
        })
        .into_any_element()
}

/// A one-line red error inside a card or under a field, where a full error card would be too much.
pub(crate) fn render_inline_error(message: impl Into<gpui::SharedString>) -> gpui::AnyElement {
    use ui::prelude::*;
    Label::new(message.into())
        .size(LabelSize::Small)
        .color(Color::Error)
        .into_any_element()
}

pub(crate) fn render_error(
    title: impl Into<gpui::SharedString>,
    error: impl Into<gpui::SharedString>,
    retry: Option<gpui::AnyElement>,
    cx: &App,
) -> gpui::AnyElement {
    use ui::prelude::*;
    v_flex()
        .debug_selector(|| "paseo-error".into())
        .p_4()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().status().error_border)
        .bg(cx.theme().status().error_background)
        .child(Label::new(title.into()).weight(gpui::FontWeight::SEMIBOLD))
        .child(Label::new(error.into()).size(LabelSize::Small))
        .when_some(retry, |this, retry| this.child(retry))
        .into_any_element()
}

/// A stable client ID for profiles saved without one, so reconnects keep daemon-side ownership.
pub(crate) fn client_id_for(profile: &PaseoConnectionProfile, cx: &App) -> String {
    if !profile.client_id.trim().is_empty() {
        return profile.client_id.clone();
    }
    let kvp = KeyValueStore::global(cx);
    match stored_client_id(kvp.read_kvp(CLIENT_ID_KEY)) {
        StoredClientId::Found(client_id) => client_id,
        StoredClientId::Missing => {
            let client_id = uuid::Uuid::new_v4().to_string();
            let stored = client_id.clone();
            db::write_and_log(cx, move || async move {
                kvp.write_kvp(CLIENT_ID_KEY.to_string(), stored).await
            });
            client_id
        }
        // Writing a new ID would replace the stable one the store couldn't read this time.
        StoredClientId::Unreadable(error) => {
            log::error!(
                "Could not read the Paseo client ID, using one for this session: {error:#}"
            );
            session_client_id()
        }
    }
}

#[derive(Debug, PartialEq)]
enum StoredClientId {
    Found(String),
    Missing,
    Unreadable(String),
}

fn stored_client_id(read: Result<Option<String>>) -> StoredClientId {
    match read {
        Ok(Some(client_id)) if !client_id.is_empty() => StoredClientId::Found(client_id),
        Ok(_) => StoredClientId::Missing,
        Err(error) => StoredClientId::Unreadable(format!("{error:#}")),
    }
}

/// One client ID for the whole session, so every host and reconnect shares it while the stored
/// one can't be read.
fn session_client_id() -> String {
    static SESSION_CLIENT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION_CLIENT_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
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
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).log_err())
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
    let Some(agent_id) = hosts::focused_agent(cx) else {
        return Ok(None);
    };
    agent_workspace(&agent_id, cx)
}

/// The directory and host of an agent, or `None` when the agent isn't known.
pub fn agent_workspace(agent_id: &str, cx: &App) -> Result<Option<SelectedWorkspace>> {
    let Some(store) = hosts::store_for_agent(agent_id, cx) else {
        return Ok(None);
    };
    let store = store.read(cx);
    let Some(agent) = store.agent(agent_id) else {
        return Ok(None);
    };
    let directory = agent
        .directory
        .clone()
        .ok_or_else(|| anyhow!("Selected Paseo agent has no workspace directory"))?;
    directory_workspace(directory, store).map(Some)
}

/// The directory and host of a Paseo folder on `store`'s host.
fn directory_workspace(directory: PathBuf, store: &PaseoStore) -> Result<SelectedWorkspace> {
    if !directory.to_str().is_some_and(is_absolute_workspace_path) {
        return Err(anyhow!("Paseo workspace directory is not absolute"));
    }
    let profile = store
        .active_profile
        .as_ref()
        .ok_or_else(|| anyhow!("No Paseo host is set up"))?;
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

/// The editor project's directory, only when `store`'s daemon runs on this machine and can use it.
fn project_directory(workspace: &Workspace, store: &PaseoStore, cx: &App) -> Option<PathBuf> {
    if !store.is_local_host() {
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
fn paseo_workspace_directory(paseo_workspace_id: &str, store: &PaseoStore) -> Option<PathBuf> {
    store
        .state
        .workspaces
        .get(paseo_workspace_id)
        .map(|descriptor| descriptor.directory.clone())
        .or_else(|| {
            store
                .state
                .agents()
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
    let Some(store) = hosts::store_for_workspace(paseo_workspace_id, cx) else {
        return false;
    };
    let store = store.read(cx);
    paseo_workspace_directory(paseo_workspace_id, store)
        .and_then(|directory| directory_workspace(directory, store).ok())
        .is_some_and(|selected| (switcher.owns)(&selected, workspace, cx))
}

/// Reopens chats on their agent's host once that host lists the agent. A tab saved before hosts
/// were recorded, or opened before its host's agent list arrived, starts on the default host,
/// which has no such agent.
fn reopen_tabs_on_their_hosts(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let misplaced = workspace
        .items_of_type::<AgentTab>(cx)
        .filter_map(|tab| {
            let view = tab.read(cx).view().read(cx);
            let agent_id = view.agent_id.clone()?;
            if view.store.read(cx).agent(&agent_id).is_some() {
                return None;
            }
            let owner = hosts::store_for_agent(&agent_id, cx)?;
            owner
                .read(cx)
                .agent(&agent_id)
                .is_some()
                .then_some((tab, agent_id))
        })
        .collect::<Vec<_>>();
    for (tab, agent_id) in misplaced {
        let Some(pane) = workspace.pane_for(&tab) else {
            continue;
        };
        let (index, previously_active) = {
            let pane = pane.read(cx);
            (pane.index_for_item(&tab), pane.active_item())
        };
        let replacement = new_agent_tab(workspace, &agent_id, window, cx);
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(replacement), false, false, index, window, cx);
            if let Some(previously_active) =
                previously_active.filter(|item| item.item_id() != tab.entity_id())
                && let Some(index) = pane.index_for_item(previously_active.as_ref())
            {
                pane.activate_item(index, false, false, window, cx);
            }
        });
        workspace_tabs::detach_tab(workspace, &tab, window, cx);
    }
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
    let (moved, source_active_view) = detach_every_chat(source, window, cx);
    if moved.is_empty() {
        return;
    }
    let switcher = project_switcher(cx).ok();
    let (homes, stay) = sort_moved_chats(moved, switcher.as_ref(), target, cx);
    if let Some(switcher) = switcher {
        move_chats_home(&switcher, homes, target, window, cx);
    }
    if !stay.is_empty() {
        keep_moved_chats(target, stay, source_active_view, window, cx);
    }
}

/// A moved chat's view, and whether its restored tab still waits for its folder check.
type MovedChat = (Entity<AgentView>, bool);

/// Removes every chat from `source`, returning them and the view of its active chat, if any.
fn detach_every_chat(
    source: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> (Vec<MovedChat>, Option<Entity<AgentView>>) {
    source.update(cx, |source, cx| {
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
    })
}

/// Splits moved chats into those with a folder of their own to go to, grouped by folder, and
/// those that stay with `target`.
fn sort_moved_chats(
    moved: Vec<MovedChat>,
    switcher: Option<&GlobalProjectSwitcher>,
    target: &Entity<Workspace>,
    cx: &App,
) -> (
    Vec<(SelectedWorkspace, Vec<Entity<AgentView>>)>,
    Vec<MovedChat>,
) {
    let mut stay = Vec::new();
    // One switch per folder, so two agents from a folder that isn't open don't open it twice.
    let mut homes: Vec<(SelectedWorkspace, Vec<Entity<AgentView>>)> = Vec::new();
    for (view, owner_check_pending) in moved {
        let home = switcher.and_then(|switcher| {
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
    (homes, stay)
}

/// Opens each folder's workspace without switching the window, and adds its chats there, or to
/// `target` when it can't open.
fn move_chats_home(
    switcher: &GlobalProjectSwitcher,
    homes: Vec<(SelectedWorkspace, Vec<Entity<AgentView>>)>,
    target: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
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

/// Adds the chats that stay to `target`'s active pane, keeping the one that was active active.
fn keep_moved_chats(
    target: &Entity<Workspace>,
    stay: Vec<MovedChat>,
    source_active_view: Option<Entity<AgentView>>,
    window: &mut Window,
    cx: &mut App,
) {
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
    let store = hosts::store_for_agent(agent_id, cx).unwrap_or_else(|| hosts::default_store(cx));
    let directory = project_directory(workspace, store.read(cx), cx);
    let workspace_handle = Some(cx.weak_entity());
    let agent_id = agent_id.to_owned();
    cx.new(|cx| {
        AgentTab::on_host(
            store,
            Some(agent_id),
            directory,
            workspace_handle,
            window,
            cx,
        )
    })
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

/// Starts a draft on the default host.
pub fn open_draft(
    workspace: &mut Workspace,
    directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let store = hosts::default_store(cx);
    open_draft_on(workspace, store, directory, window, cx)
}

/// Starts a draft whose agent will run on `store`'s host.
pub(crate) fn open_draft_on(
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let directory = directory.or_else(|| project_directory(workspace, store.read(cx), cx));
    let workspace_handle = Some(cx.weak_entity());
    let tab = cx.new(|cx| AgentTab::on_host(store, None, directory, workspace_handle, window, cx));
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
    let store = paseo_workspace_id
        .as_deref()
        .and_then(|paseo_workspace_id| hosts::store_for_workspace(paseo_workspace_id, cx))
        .unwrap_or_else(|| hosts::default_store(cx));
    let tab = open_draft_on(workspace, store, directory, window, cx);
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
    let found = hosts::store_for_workspace(paseo_workspace_id, cx).and_then(|store| {
        let directory = paseo_workspace_directory(paseo_workspace_id, store.read(cx))?;
        Some((directory, store))
    });
    let Some((directory, store)) = found else {
        let error = anyhow!("Paseo hasn't loaded this workspace yet");
        show_open_error(workspace, &error, cx);
        return Task::ready(Err(error));
    };
    let draft = open_draft_in_on(workspace, store, directory, window, cx);
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
        let Some(store) = hosts::store_for_workspace(paseo_workspace_id, cx) else {
            return;
        };
        let store = store.read(cx);
        store
            .state
            .agents()
            .iter()
            .filter(|agent| store::agent_workspace_id(agent) == Some(paseo_workspace_id))
            .max_by_key(|agent| store::agent_updated_at(agent))
            .map(|agent| agent.id.clone())
    };
    match recent {
        Some(agent_id) => open_agent(workspace, &agent_id, true, window, cx),
        None => {
            if let Some(tab) = draft_tab_in(workspace, paseo_workspace_id, cx) {
                workspace.activate_item(&tab, true, true, window, cx);
                focus_tab_composer(&tab, window, cx);
                return;
            }
            let this_workspace = cx.weak_entity();
            let paseo_workspace_id = paseo_workspace_id.to_owned();
            // Deferred because looking through the window's other workspaces, and activating
            // one, reads this workspace and the window root, which may be mid-update.
            window.defer(cx, move |window, cx| {
                let multi_workspace = window.root::<MultiWorkspace>().flatten();
                // The workspace may have left the window before this ran, and a draft there
                // would be unreachable.
                let Some(this_workspace) = this_workspace.upgrade().filter(|this_workspace| {
                    multi_workspace.as_ref().is_none_or(|multi_workspace| {
                        multi_workspace
                            .read(cx)
                            .workspaces()
                            .any(|workspace| workspace == this_workspace)
                    })
                }) else {
                    return;
                };
                let elsewhere = multi_workspace.and_then(|multi_workspace| {
                    multi_workspace
                        .read(cx)
                        .workspaces()
                        .filter(|other| **other != this_workspace)
                        .find_map(|other| {
                            let tab = draft_tab_in(other.read(cx), &paseo_workspace_id, cx)?;
                            Some((multi_workspace.clone(), other.clone(), tab))
                        })
                });
                match elsewhere {
                    Some((multi_workspace, owner, tab)) => {
                        multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.activate(owner.clone(), None, window, cx)
                        });
                        owner.update(cx, |owner, cx| {
                            owner.activate_item(&tab, true, true, window, cx);
                        });
                        focus_tab_composer(&tab, window, cx);
                    }
                    None => this_workspace.update(cx, |workspace, cx| {
                        new_agent_in_paseo_workspace(workspace, &paseo_workspace_id, window, cx)
                            .detach_and_log_err(cx)
                    }),
                }
            });
        }
    }
}

/// The open draft that will join `paseo_workspace_id`, so opening that empty workspace again
/// shows it instead of starting another.
fn draft_tab_in(
    workspace: &Workspace,
    paseo_workspace_id: &str,
    cx: &App,
) -> Option<Entity<AgentTab>> {
    workspace.items_of_type::<AgentTab>(cx).find(|tab| {
        let view = tab.read(cx).view().read(cx);
        view.agent_id.is_none()
            && view.composer.read(cx).draft_workspace_id.as_deref() == Some(paseo_workspace_id)
    })
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
    let store = hosts::default_store(cx);
    open_draft_in_on(workspace, store, directory, window, cx)
}

/// `open_draft_in` for a folder on `store`'s host.
pub(crate) fn open_draft_in_on(
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    directory: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<AgentTab>>> {
    let switched = project_switcher(cx).and_then(|switcher| {
        let selected = directory_workspace(directory.clone(), store.read(cx))?;
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
        Ok(None) => {
            return Task::ready(Ok(open_draft_on(
                workspace,
                store,
                Some(directory),
                window,
                cx,
            )));
        }
        Ok(Some(switch)) => switch,
        Err(error) => {
            let error = error.context("Could not open the folder's workspace in the editor");
            show_open_error(workspace, &error, cx);
            return Task::ready(Err(error));
        }
    };
    cx.spawn_in(window, async move |workspace, cx| match switch.await {
        Ok(target) => update_in_own_window(&target, cx, |target, window, cx| {
            open_draft_on(target, store, Some(directory), window, cx)
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
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if paseo_client::parse_subagent_timeline_id(agent_id).is_some() {
        workspace.show_error(
            anyhow!("A subagent can't be forked. Fork its parent agent instead."),
            cx,
        );
        return;
    }
    let Some(agent) = store.read(cx).agent(agent_id).cloned() else {
        workspace.show_error(anyhow!("This agent is no longer on its host"), cx);
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
                let store = store.clone();
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        open_draft_in_on(workspace, store, directory, window, cx)
                    })?
                    .await?
            }
            (None, None) => {
                let store = store.clone();
                workspace.update_in(cx, |workspace, window, cx| {
                    open_draft_on(workspace, store, None, window, cx)
                })?
            }
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
    let recent = hosts::stores(cx)
        .iter()
        .flat_map(|store| store.read(cx).state.agents().iter())
        .max_by_key(|agent| store::agent_updated_at(agent))
        .map(|agent| agent.id.clone());
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
    let store = hosts::default_store(cx);
    store.update(cx, |store, cx| {
        store.active_profile = Some(PaseoConnectionProfile {
            name: "Local".into(),
            target_uri: "ws://127.0.0.1:6767/ws".into(),
            editor_ssh_uri: None,
            client_id: "test-client".into(),
        });
        let mut agents = store.state.agents().to_vec();
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
    hosts::default_store(cx).update(cx, |store, cx| {
        store
            .state
            .permissions
            .insert(request.request_id.clone(), request);
        cx.notify();
    });
}

/// The globals the Paseo views need outside a workspace, for tests and benchmarks.
#[cfg(any(test, feature = "test-support"))]
pub fn test_init(cx: &mut App) {
    if !cx.has_global::<settings::SettingsStore>() {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
    }
    cx.set_global(db::AppDatabase::test_new());
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);
    PaseoSettings::register(cx);
    hosts::init(cx);
}

/// An idle agent in `/work/project`, updated at a fixed time, for tests and benchmarks.
#[cfg(any(test, feature = "test-support"))]
pub fn test_agent(id: &str, title: &str, status: &str) -> paseo_client::AgentSummary {
    paseo_client::AgentSummary {
        id: id.into(),
        title: Some(title.into()),
        status: status.into(),
        directory: Some(std::path::PathBuf::from("/work/project")),
        project: Some(serde_json::json!({"projectName": "project"})),
        extra: serde_json::json!({"updatedAt": "2026-10-02T10:00:00Z"}),
    }
}

/// The chat of `agent_id` on the default host, outside any workspace, for tests and benchmarks.
#[cfg(any(test, feature = "test-support"))]
pub fn test_chat(agent_id: &str, window: &mut Window, cx: &mut Context<AgentView>) -> AgentView {
    AgentView::on_host(
        hosts::default_store(cx),
        Some(agent_id.into()),
        None,
        None,
        window,
        cx,
    )
}

/// How many rows a chat shows, for checking a benchmark's setup.
#[cfg(any(test, feature = "test-support"))]
pub fn test_chat_rows(chat: &Entity<AgentView>, cx: &App) -> usize {
    chat.read(cx).rows.len()
}

/// Adds `entries` to the default host's timelines the way the daemon streams them.
#[cfg(any(test, feature = "test-support"))]
pub fn test_stream_entries(entries: Vec<paseo_client::TimelineEntry>, cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
        let agent_ids = entries
            .iter()
            .map(|entry| entry.agent_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for entry in entries {
            store.state.insert_entry(entry);
        }
        for agent_id in agent_ids {
            cx.emit(store::StoreEvent::TimelineChanged(agent_id));
        }
    });
}

/// Updates or adds one agent on the default host as a daemon `agent_update` does.
#[cfg(any(test, feature = "test-support"))]
pub fn test_upsert_agent(agent: paseo_client::AgentSummary, cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
        store.handle_event(paseo_client::PaseoEvent::AgentUpserted(agent), cx)
    });
}

/// Lists `agents` on the default host in place of what it had, for tests and benchmarks.
#[cfg(any(test, feature = "test-support"))]
pub fn test_set_agents(agents: Vec<paseo_client::AgentSummary>, cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
        store.state.set_agents(agents);
        cx.notify();
    });
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_add_agent(agent: paseo_client::AgentSummary, cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
        store.state.upsert_agent(agent);
        cx.notify();
    });
}

/// Lists `agent` on the host with profile name `host`, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_add_agent_on(host: &str, agent: paseo_client::AgentSummary, cx: &mut App) {
    hosts::store_named(host, cx).update(cx, |store, cx| {
        store.state.upsert_agent(agent);
        cx.notify();
    });
}

/// Connects the store to a daemon on this machine, as far as opening workspaces goes, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_use_local_host(cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
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
    hosts::default_store(cx).update(cx, |store, cx| {
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
    hosts::default_store(cx).update(cx, |store, cx| {
        store
            .state
            .retain_agents(|agent| store::agent_workspace_id(agent) != Some(workspace_id));
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

/// Whether `workspace` shows the New Workspace window, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_new_workspace_open(workspace: &Workspace, cx: &App) -> bool {
    workspace
        .active_modal::<workspace_tools::NewWorkspaceModal>(cx)
        .is_some()
}

/// The New Workspace window's message, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_new_workspace_text(workspace: &Entity<Workspace>, cx: &App) -> Option<String> {
    let modal = workspace
        .read(cx)
        .active_modal::<workspace_tools::NewWorkspaceModal>(cx)?;
    Some(modal.read(cx).composer_for_test().read(cx).text(cx))
}

/// Types `text` into the New Workspace window's message, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_new_workspace_set_text(
    workspace: &Entity<Workspace>,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let modal = workspace
        .read(cx)
        .active_modal::<workspace_tools::NewWorkspaceModal>(cx);
    if let Some(modal) = modal {
        let composer = modal.read(cx).composer_for_test();
        composer.update(cx, |composer, cx| composer.set_text(text, window, cx));
    }
}

/// Finishes the New Workspace window's draft the way its first message creating `agent_id`
/// does, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_new_workspace_agent_created(
    workspace: &Entity<Workspace>,
    agent_id: &str,
    cx: &mut App,
) {
    let modal = workspace
        .read(cx)
        .active_modal::<workspace_tools::NewWorkspaceModal>(cx);
    if let Some(modal) = modal {
        let composer = modal.read(cx).composer_for_test();
        composer.update(cx, |composer, cx| {
            composer.finish_creating_for_test(agent_id.to_owned(), cx)
        });
    }
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
    let tab = cx.new(|cx| AgentTab::restored(agent_id, None, workspace_handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(tab), None, false, window, cx);
}

/// Records `agent_id` as the agent last focused in a Paseo view, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_set_focused_agent(agent_id: &str, cx: &mut App) {
    hosts::default_store(cx).update(cx, |store, cx| {
        store.set_focused_agent(agent_id.to_owned(), cx)
    });
}

/// The store's error banner, which a request sent with no daemon connection sets, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_store_error(cx: &App) -> Option<String> {
    hosts::default_store(cx).read(cx).state.error.clone()
}

/// The agent the store records as the one the user is looking at, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_focused_agent(cx: &App) -> Option<String> {
    hosts::focused_agent(cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_client_id_is_not_replaced() {
        assert_eq!(
            stored_client_id(Ok(Some("stable".into()))),
            StoredClientId::Found("stable".into())
        );
        assert_eq!(stored_client_id(Ok(None)), StoredClientId::Missing);
        assert_eq!(
            stored_client_id(Ok(Some(String::new()))),
            StoredClientId::Missing
        );
        assert!(matches!(
            stored_client_id(Err(anyhow!("database is locked"))),
            StoredClientId::Unreadable(_)
        ));
        assert_eq!(
            session_client_id(),
            session_client_id(),
            "every host shares one ID for the session"
        );
    }

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
            PaseoSettings::register(cx);
            hosts::init(cx);
            init_system_notifications(cx);
            hosts::default_store(cx)
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
                    title_lines: 1,
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

    #[gpui::test]
    fn chat_font_size_defaults_below_the_ui_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut store = settings::SettingsStore::test(cx);
            store
                .set_user_settings(r#"{"ui_font_size": 16}"#, cx)
                .result()
                .expect("valid settings");
            cx.set_global(store);
            assert_eq!(
                chat_font_size(cx),
                px(14.),
                "chat prose matches Zed's label size, seven eighths of the UI font size"
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
