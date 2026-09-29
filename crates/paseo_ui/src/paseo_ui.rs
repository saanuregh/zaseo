mod agent_edits;
mod agent_view;
mod command_center;
mod composer;
mod connection_picker;
mod daemon;
mod dictation;
mod editor_context;
mod last_turn;
mod sidebar;
mod store;
mod stream;
mod terminal;
mod timeline;
mod usage;
mod workspace_tools;
mod worktrees;

use anyhow::{Result, anyhow};
use db::kvp::KeyValueStore;
use gpui::{
    Action, App, AppContext as _, Context, Entity, Global, Pixels, Task, TaskExt, Window, actions,
};
use paseo_client::{ConnectionTarget, is_absolute_workspace_path};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{PaseoConnectionProfile, RegisterSetting, Settings};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use theme_settings::ThemeSettings;
use workspace::{Toast, Workspace, notifications::NotificationId};

pub use agent_edits::{AgentEditsToolbar, KeepAllEdits, KeepEdit, RejectAllEdits, RejectEdit};
pub use agent_view::{AgentTab, AgentView};
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
        /// Opens the selected Paseo agent's directory as an editor project.
        OpenWorkspace,
        /// Starts a new Paseo agent draft.
        NewAgent,
        /// Searches Paseo commands and agents.
        ToggleCommandCenter,
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
        /// Shows archived agents in the sidebar.
        ToggleArchived,
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
    cx.set_global(GlobalPaseoStore(store));
    editor_context::init(cx);
    agent_edits::init(cx);
    cx.observe_new(
        |workspace: &mut Workspace, _window, cx: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
                workspace.toggle_panel_focus::<PaseoPanel>(window, cx);
            });
            workspace.register_action(|workspace, _: &OpenTab, window, cx| {
                open_tab(workspace, window, cx);
            });
            workspace.register_action(|workspace, _: &NewAgent, window, cx| {
                open_draft(workspace, None, window, cx);
            });
            workspace.register_action(|workspace, _: &ForkAgent, window, cx| {
                if let Some(agent_id) = current_agent_id(workspace, cx) {
                    fork_agent(workspace, &agent_id, window, cx);
                }
            });
            workspace.register_action(|workspace, _: &ToggleCommandCenter, window, cx| {
                command_center::toggle(workspace, window, cx);
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
                let agent_id = agent_id.clone();
                let handle = cx.weak_entity();
                workspace.show_toast(
                    Toast::new(
                        NotificationId::named(format!("paseo-attention-{agent_id}").into()),
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
    theme_settings::clamp_font_size(
        settings.ui_font_size(cx) + settings.buffer_font_size(cx)
            - settings.buffer_font_size_settings(),
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

/// Runs on the target workspace before the window shows it, so it appears with the agent's tab
/// already open instead of drawing its old contents first.
pub type WorkspaceInit =
    Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send>;

/// Switches the window to the editor project for an agent's workspace, opening it if needed,
/// and resolves to the workspace the agent tab belongs in, or `None` to stay put. The app
/// registers it because it owns opening local and SSH projects. `init` may be skipped (for
/// example for SSH projects), so callers still open the tab afterwards.
pub type ProjectSwitcher = Rc<
    dyn Fn(
        SelectedWorkspace,
        Option<WorkspaceInit>,
        &mut Workspace,
        &mut Window,
        &mut Context<Workspace>,
    ) -> Task<Result<Option<Entity<Workspace>>>>,
>;

fn open_agent_init(agent_id: &str, focus: bool) -> WorkspaceInit {
    let agent_id = agent_id.to_owned();
    Box::new(move |workspace, window, cx| {
        open_agent_here(workspace, &agent_id, focus, window, cx);
    })
}

struct GlobalProjectSwitcher(ProjectSwitcher);

impl Global for GlobalProjectSwitcher {}

pub fn set_project_switcher(switcher: ProjectSwitcher, cx: &mut App) {
    cx.set_global(GlobalProjectSwitcher(switcher));
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

/// Opens the agent's tab, reusing an existing one in any pane.
/// Opens an agent's tab, first switching the window to the agent's project like Paseo does.
pub fn open_agent(
    workspace: &mut Workspace,
    agent_id: &str,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let switcher = cx
        .try_global::<GlobalProjectSwitcher>()
        .map(|global| global.0.clone());
    let selected = agent_workspace(agent_id, cx).unwrap_or_else(|error| {
        log::debug!("Paseo agent {agent_id} has no project to switch to: {error}");
        None
    });
    let (Some(switcher), Some(selected)) = (switcher, selected) else {
        open_agent_here(workspace, agent_id, focus, window, cx);
        return;
    };
    let init = open_agent_init(agent_id, focus);
    let switch = switcher(selected, Some(init), workspace, window, cx);
    let agent_id = agent_id.to_owned();
    cx.spawn_in(window, async move |workspace, cx| {
        let target = match switch.await {
            Ok(target) => target,
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .show_error(error.context("Could not switch to the agent's project"), cx)
                })?;
                None
            }
        };
        match target {
            Some(target) => target.update_in(cx, |target, window, cx| {
                open_agent_here(target, &agent_id, focus, window, cx);
            }),
            None => workspace.update_in(cx, |workspace, window, cx| {
                open_agent_here(workspace, &agent_id, focus, window, cx);
            }),
        }
    })
    .detach_and_log_err(cx);
}

/// Moves a just-created agent's tab into the agent's project when the agent runs outside the
/// current one, such as in a new worktree, the way Paseo moves into a new workspace.
pub(crate) fn follow_created_agent(
    workspace: &mut Workspace,
    tab: Entity<AgentTab>,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(switcher) = cx
        .try_global::<GlobalProjectSwitcher>()
        .map(|global| global.0.clone())
    else {
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
    let switch = switcher(selected, Some(init), workspace, window, cx);
    let agent_id = agent_id.to_owned();
    cx.spawn_in(window, async move |workspace, cx| {
        let target = match switch.await {
            Ok(Some(target)) => target,
            Ok(None) => return anyhow::Ok(()),
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .show_error(error.context("Could not switch to the agent's project"), cx)
                })?;
                return Ok(());
            }
        };
        if target.downgrade() == workspace {
            return Ok(());
        }
        target.update_in(cx, |target, window, cx| {
            open_agent_here(target, &agent_id, true, window, cx)
        })?;
        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(pane) = workspace.pane_for(&tab) {
                pane.update(cx, |pane, cx| {
                    pane.remove_item(tab.entity_id(), false, true, window, cx)
                });
            }
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

pub(crate) fn open_agent_here(
    workspace: &mut Workspace,
    agent_id: &str,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AgentTab> {
    let existing = workspace
        .items_of_type::<AgentTab>(cx)
        .find(|tab| tab.read(cx).agent_id(cx).as_deref() == Some(agent_id));
    if let Some(tab) = existing {
        workspace.activate_item(&tab, true, focus, window, cx);
        if focus {
            focus_tab_composer(&tab, window, cx);
        }
        return tab;
    }
    let directory = project_directory(workspace, cx);
    let agent_id = agent_id.to_owned();
    let workspace_handle = Some(cx.weak_entity());
    let tab = cx.new(|cx| AgentTab::new(Some(agent_id), directory, workspace_handle, window, cx));
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
    let existing = workspace
        .items_of_type::<AgentTab>(cx)
        .find(|tab| tab.read(cx).agent_id(cx).as_deref() == Some(timeline_id.as_str()));
    if let Some(tab) = existing {
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
        workspace.update_in(cx, |workspace, window, cx| {
            let tab = open_draft(workspace, agent.directory.clone(), window, cx);
            let composer = tab.read(cx).view().read(cx).composer.clone();
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
        })
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

/// The agent the store records as the one the user is looking at, for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_focused_agent(cx: &App) -> Option<String> {
    store(cx).read(cx).focused_agent.clone()
}

#[cfg(test)]
mod tests {
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
