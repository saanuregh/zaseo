//! Which Paseo workspace an editor workspace shows. Zed keeps one editor workspace per folder,
//! while several Paseo workspaces can share a folder, so an editor workspace shows the agent tabs
//! of one Paseo workspace at a time and keeps the others' chats hidden until they are shown again.

use std::collections::{HashMap, HashSet};

use gpui::{App, Context, Entity, EntityId, Global, Window};
use workspace::{Pane, Workspace};

use crate::agent_view::{AgentTab, AgentView};
use crate::store::{self, agent_parent_id, agent_workspace_id};

#[derive(Default)]
struct ShownWorkspace {
    paseo_workspace_id: Option<String>,
    hidden: Vec<HiddenChat>,
    /// Agents whose tab the user closed, which stay closed until the agent is opened again.
    closed_agents: HashSet<String>,
}

struct HiddenChat {
    paseo_workspace_id: String,
    view: Entity<AgentView>,
}

#[derive(Default)]
struct ShownWorkspaces(HashMap<EntityId, ShownWorkspace>);

impl Global for ShownWorkspaces {}

fn shown_mut(workspace_id: EntityId, cx: &mut App) -> &mut ShownWorkspace {
    cx.default_global::<ShownWorkspaces>()
        .0
        .entry(workspace_id)
        .or_default()
}

fn shown(workspace_id: EntityId, cx: &App) -> Option<&ShownWorkspace> {
    cx.try_global::<ShownWorkspaces>()?.0.get(&workspace_id)
}

/// Forgets a released editor workspace.
pub(crate) fn forget(workspace_id: EntityId, cx: &mut App) {
    if cx.has_global::<ShownWorkspaces>() {
        cx.global_mut::<ShownWorkspaces>().0.remove(&workspace_id);
    }
}

/// The Paseo workspace `workspace` shows, if any.
pub(crate) fn shown_paseo_workspace(workspace: &Workspace, cx: &App) -> Option<String> {
    shown(workspace.weak_handle().entity_id(), cx)?
        .paseo_workspace_id
        .clone()
}

/// The Paseo workspace a chat belongs to: its agent's, or the one its draft joins. `None` while
/// the agent isn't known yet and for drafts that start a new workspace.
fn chat_paseo_workspace(view: &AgentView, cx: &App) -> Option<String> {
    match &view.agent_id {
        Some(agent_id) => agent_paseo_workspace(agent_id, cx),
        None => view.composer.read(cx).draft_workspace_id.clone(),
    }
}

/// A tab's Paseo workspace, ignoring tabs on their way out and restored tabs whose folder hasn't
/// been checked yet, which the restored-tab check may still close.
fn tab_paseo_workspace(tab: &Entity<AgentTab>, cx: &App) -> Option<String> {
    let tab = tab.read(cx);
    if tab.leaving || tab.owner_check_pending {
        return None;
    }
    chat_paseo_workspace(tab.view().read(cx), cx)
}

/// The Paseo workspace of `agent_id`, a subagent's being its parent's, once the store knows the
/// agent.
pub(crate) fn agent_paseo_workspace(agent_id: &str, cx: &App) -> Option<String> {
    let agent_id = paseo_client::parse_subagent_timeline_id(agent_id)
        .map_or(agent_id, |(parent_agent_id, _)| parent_agent_id);
    crate::hosts::store_for_agent(agent_id, cx)?
        .read(cx)
        .agent(agent_id)
        .and_then(agent_workspace_id)
        .map(str::to_owned)
}

/// Records that the user closed an agent's tab, so it doesn't open again by itself.
pub(crate) fn tab_closed(workspace_id: EntityId, agent_id: String, cx: &mut App) {
    shown_mut(workspace_id, cx).closed_agents.insert(agent_id);
}

/// Prepares `workspace` to open `agent_id`'s tab: forgets that the user closed it and shows the
/// agent's Paseo workspace, which brings back its hidden chat.
pub(crate) fn prepare_to_open(
    workspace: &mut Workspace,
    agent_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    shown_mut(cx.entity_id(), cx).closed_agents.remove(agent_id);
    if let Some(paseo_workspace_id) = agent_paseo_workspace(agent_id, cx)
        && crate::workspace_holds_paseo_workspace(workspace, &paseo_workspace_id, cx)
    {
        show(workspace, &paseo_workspace_id, window, cx);
    }
}

/// Shows `paseo_workspace_id`'s agent tabs in `workspace` and hides the chats of every other
/// Paseo workspace, keeping them to show again later.
pub(crate) fn show(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_id = cx.entity_id();
    shown_mut(workspace_id, cx).paseo_workspace_id = Some(paseo_workspace_id.to_owned());
    let to_hide = crate::chat_panel::agent_tabs(workspace, cx)
        .into_iter()
        .filter_map(|tab| {
            let chat_workspace = tab_paseo_workspace(&tab, cx)?;
            (chat_workspace != paseo_workspace_id).then_some((tab, chat_workspace))
        })
        .collect::<Vec<_>>();
    for (tab, chat_workspace) in to_hide {
        let view = tab.read(cx).view().clone();
        detach_tab(workspace, &tab, window, cx);
        shown_mut(workspace_id, cx).hidden.push(HiddenChat {
            paseo_workspace_id: chat_workspace,
            view,
        });
    }
    let hidden = std::mem::take(&mut shown_mut(workspace_id, cx).hidden);
    let (revealed, still_hidden): (Vec<_>, Vec<_>) = hidden
        .into_iter()
        .partition(|chat| chat.paseo_workspace_id == paseo_workspace_id);
    shown_mut(workspace_id, cx).hidden = still_hidden;
    let pane = crate::chat_panel::chat_pane(workspace, cx);
    for chat in revealed {
        add_without_activating(&pane, chat.view, window, cx);
    }
    open_missing_tabs(workspace, paseo_workspace_id, window, cx);
}

/// Removes a tab from its pane for a move or a hide, which isn't the user closing it.
pub(crate) fn detach_tab(
    workspace: &mut Workspace,
    tab: &Entity<AgentTab>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    tab.update(cx, |tab, _| tab.leaving = true);
    if let Some(pane) = crate::chat_panel::pane_for_tab(workspace, tab, cx) {
        pane.update(cx, |pane, cx| {
            pane.remove_item(tab.entity_id(), false, true, window, cx)
        });
    }
}

fn add_without_activating(
    pane: &Entity<Pane>,
    view: Entity<AgentView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tab = AgentTab::for_view(view, cx.weak_entity(), false, cx);
    pane.update(cx, |pane, cx| {
        pane.add_item_inner(Box::new(tab), false, false, false, None, window, cx)
    });
}

/// Opens a tab for each of the Paseo workspace's agents that has none, like Paseo does: agents
/// another agent of the workspace started stay in the sidebar, and so do tabs the user closed.
fn open_missing_tabs(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tabs = crate::chat_panel::agent_tabs(workspace, cx);
    // A sent draft becomes its new agent's tab once the daemon answers, and the agent can reach
    // the store first.
    let creating = tabs.iter().any(|tab| {
        let view = tab.read(cx).view().read(cx);
        let composer = view.composer.read(cx);
        view.agent_id.is_none()
            && composer.is_creating()
            && composer.draft_workspace_id.as_deref() == Some(paseo_workspace_id)
    });
    if creating {
        return;
    }
    let open = tabs
        .iter()
        .filter_map(|tab| tab.read(cx).agent_id(cx))
        .collect::<HashSet<_>>();
    let closed = {
        let shown = shown_mut(cx.entity_id(), cx);
        // Moving a tab between panes removes it too, which isn't closing it.
        shown
            .closed_agents
            .retain(|agent_id| !open.contains(agent_id));
        shown.closed_agents.clone()
    };
    let Some(store) = crate::hosts::store_for_workspace(paseo_workspace_id, cx) else {
        return;
    };
    let missing = {
        let store = store.read(cx);
        let workspace_of = store
            .state
            .agents()
            .iter()
            .filter_map(|agent| Some((agent.id.as_str(), agent_workspace_id(agent)?)))
            .collect::<HashMap<_, _>>();
        let mut agents = store
            .state
            .agents()
            .iter()
            .filter(|agent| agent_workspace_id(agent) == Some(paseo_workspace_id))
            .filter(|agent| {
                agent_parent_id(agent).is_none_or(|parent_id| {
                    workspace_of.get(parent_id).copied() != Some(paseo_workspace_id)
                })
            })
            .filter(|agent| !open.contains(&agent.id) && !closed.contains(&agent.id))
            .collect::<Vec<_>>();
        agents.sort_by_cached_key(|agent| store::agent_string(agent, "createdAt"));
        agents
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>()
    };
    if missing.is_empty() {
        return;
    }
    let pane = crate::chat_panel::chat_pane(workspace, cx);
    for agent_id in missing {
        let tab = crate::new_agent_tab(workspace, &agent_id, window, cx);
        pane.update(cx, |pane, cx| {
            pane.add_item_inner(Box::new(tab), false, false, false, None, window, cx)
        });
    }
}

/// Keeps `workspace` showing one Paseo workspace as tabs change and agents load: a newly active
/// agent tab of another workspace shows that workspace, a workspace showing none takes the one of
/// its first agent tab, and the shown workspace gets tabs for agents it gains. Only Paseo
/// workspaces working in this workspace's folders are shown.
pub(crate) fn refresh(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let current = shown(cx.entity_id(), cx).and_then(|shown| shown.paseo_workspace_id.clone());
    let tabs = crate::chat_panel::agent_tabs(workspace, cx);
    // Most editor workspaces hold no Paseo chats, and this runs on every change on any host.
    if current.is_none() && tabs.is_empty() {
        return;
    }
    let active = crate::chat_panel::active_agent_tab(workspace, cx)
        .and_then(|tab| tab_paseo_workspace(&tab, cx));
    let Some(target) = active
        .or_else(|| current.clone())
        .or_else(|| tabs.iter().find_map(|tab| tab_paseo_workspace(tab, cx)))
    else {
        return;
    };
    if !crate::workspace_holds_paseo_workspace(workspace, &target, cx) {
        return;
    }
    let needs_show = current.as_ref() != Some(&target)
        || tabs.iter().any(|tab| {
            tab_paseo_workspace(tab, cx).is_some_and(|chat_workspace| chat_workspace != target)
        });
    if needs_show {
        show(workspace, &target, window, cx);
    } else {
        open_missing_tabs(workspace, &target, window, cx);
    }
}

/// Drops the chats of an archived Paseo workspace. When `workspace` showed it, it shows the
/// Paseo workspace hidden most recently instead, if any. Returns whether `workspace` showed it.
pub(crate) fn paseo_workspace_removed(
    workspace: &mut Workspace,
    paseo_workspace_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let workspace_id = cx.entity_id();
    let showed = {
        let shown = shown_mut(workspace_id, cx);
        shown
            .hidden
            .retain(|chat| chat.paseo_workspace_id != paseo_workspace_id);
        let showed = shown.paseo_workspace_id.as_deref() == Some(paseo_workspace_id);
        if showed {
            shown.paseo_workspace_id = None;
        }
        showed
    };
    let archived_agents = crate::hosts::stores(cx)
        .iter()
        .flat_map(|store| store.read(cx).archived.iter().flatten())
        .filter(|agent| agent_workspace_id(agent) == Some(paseo_workspace_id))
        .map(|agent| agent.id.clone())
        .collect::<HashSet<_>>();
    let tabs = crate::chat_panel::agent_tabs(workspace, cx);
    for tab in tabs {
        let (agent_id, pending) = {
            let tab = tab.read(cx);
            (tab.agent_id(cx), tab.owner_check_pending)
        };
        let Some(agent_id) = agent_id else {
            // A draft of the archived workspace keeps its text and starts a new workspace.
            let composer = tab.read(cx).view().read(cx).composer.clone();
            composer.update(cx, |composer, cx| {
                if composer.draft_workspace_id.as_deref() == Some(paseo_workspace_id) {
                    composer.draft_workspace_id = None;
                    cx.notify();
                }
            });
            continue;
        };
        let belonged = match agent_paseo_workspace(&agent_id, cx) {
            Some(chat_workspace) => chat_workspace == paseo_workspace_id,
            // The agents of an archived workspace leave the store's list, so an unknown agent
            // belonged to the workspace this showed, unless its restored tab isn't checked yet.
            None => {
                archived_agents.contains(&agent_id)
                    || (showed
                        && !pending
                        && crate::hosts::store_for_agent(&agent_id, cx).is_none())
            }
        };
        if belonged {
            detach_tab(workspace, &tab, window, cx);
        }
    }
    if showed {
        let next = shown(workspace_id, cx)
            .and_then(|shown| shown.hidden.last())
            .map(|chat| chat.paseo_workspace_id.clone());
        if let Some(next) = next {
            show(workspace, &next, window, cx);
        }
    }
    showed
}
