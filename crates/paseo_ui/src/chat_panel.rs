//! The chat panel: agent chats in their own tabs and splits between the agents list and the
//! editor, which keeps only files.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::Result;
use db::kvp::KeyValueStore;
use gpui::{
    Action as _, App, AsyncWindowContext, Axis, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Pixels, WeakEntity, Window, px,
};
use project::Project;
use serde::{Deserialize, Serialize};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ActivePaneDecorator, Member, Pane, PaneAxis, PaneGroup, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemHandle,
    pane::{self, DraggedTab, SplitMode},
};

use crate::agent_view::AgentTab;
use crate::{NewAgent, ToggleChatPanel, hosts};

const SERIALIZATION_KEY: &str = "paseo_chat_panel";

pub struct ChatPanel {
    workspace: WeakEntity<Workspace>,
    center: PaneGroup,
    active_pane: Entity<Pane>,
    /// Shared by the panel's panes, so the chat used last can be found across them.
    next_timestamp: Arc<AtomicUsize>,
    position: DockPosition,
    focus_handle: FocusHandle,
    /// What was last written, so title changes during a turn don't rewrite the same layout.
    last_saved: Option<String>,
}

pub enum ChatPanelEvent {
    /// The chat in front changed: another tab, another pane, or a tab opened or closed.
    ActiveTabChanged,
}

impl EventEmitter<PanelEvent> for ChatPanel {}
impl EventEmitter<ChatPanelEvent> for ChatPanel {}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SerializedChatPanes {
    Split {
        vertical: bool,
        flexes: Vec<f32>,
        children: Vec<SerializedChatPanes>,
    },
    Pane {
        tabs: Vec<SerializedChatTab>,
        active_tab: Option<usize>,
        focused: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct SerializedChatTab {
    agent_id: String,
    host: Option<String>,
}

/// Only chats can be dropped into the chat panel.
fn is_dragged_chat(dragged: &dyn Any) -> bool {
    dragged
        .downcast_ref::<DraggedTab>()
        .is_some_and(|tab| tab.item.downcast::<AgentTab>().is_some())
}

fn serialization_key(workspace: &Workspace) -> Option<String> {
    workspace
        .database_id()
        .map(|id| format!("{SERIALIZATION_KEY}-{}", i64::from(id)))
}

impl ChatPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let saved = serialization_key(workspace)
                .and_then(|key| KeyValueStore::global(cx).read_kvp(&key).log_err().flatten())
                .and_then(|value| serde_json::from_str::<SerializedChatPanes>(&value).log_err());
            let project = workspace.project().clone();
            let workspace = workspace.weak_handle();
            let panel = cx.new(|cx| {
                let mut panel = Self::new(workspace, project.clone(), window, cx);
                if let Some(saved) = saved {
                    panel.restore(saved, &project, window, cx);
                }
                panel
            });
            // What follows the chat in front (the title bar, the agents list, the shown Paseo
            // workspace) listens for the workspace's active item, which only covers the editor.
            cx.subscribe(&panel, |_, _, _: &ChatPanelEvent, cx| {
                cx.emit(workspace::Event::ActiveItemChanged)
            })
            .detach();
            panel
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let next_timestamp = Arc::new(AtomicUsize::new(0));
        let pane = Self::new_pane(
            workspace.clone(),
            project,
            next_timestamp.clone(),
            window,
            cx,
        );
        Self {
            workspace,
            center: PaneGroup::new(pane.clone()),
            active_pane: pane,
            next_timestamp,
            position: DockPosition::Left,
            focus_handle: cx.focus_handle(),
            last_saved: None,
        }
    }

    fn new_pane(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        next_timestamp: Arc<AtomicUsize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        let pane = cx.new(|cx| {
            let mut pane = Pane::new(
                workspace,
                project,
                next_timestamp,
                Some(Arc::new(|dragged, _, _| is_dragged_chat(dragged))),
                NewAgent.boxed_clone(),
                false,
                window,
                cx,
            );
            pane.set_can_navigate(false, cx);
            pane.display_nav_history_buttons(None);
            pane.set_should_display_tab_bar(|_, _| true);
            pane.set_zoom_out_on_close(false);
            // No drag-to-edge splits: a dropped tab splits the workspace's editor panes, not
            // this panel's. The split button and split actions split chats instead.
            pane.set_render_tab_bar_buttons(cx, |pane, _, cx| {
                let focus_handle = pane.focus_handle(cx);
                let agent_menu = pane
                    .active_item()
                    .and_then(|item| crate::agent_view::agent_tab_menu(item.as_ref(), cx));
                let buttons = h_flex()
                    .gap_0p5()
                    .children(agent_menu)
                    .child(
                        IconButton::new("paseo-chat-new-agent", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |_, cx| {
                                    Tooltip::for_action_in(
                                        "New Agent",
                                        &NewAgent,
                                        &focus_handle,
                                        cx,
                                    )
                                }
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(NewAgent.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        IconButton::new("paseo-chat-split", IconName::Split)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "New Agent to the Right",
                                    &pane::SplitRight::default(),
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(pane::SplitRight::default().boxed_clone(), cx)
                            }),
                    );
                (None, Some(buttons.into_any_element()))
            });
            pane
        });
        cx.subscribe_in(&pane, window, Self::handle_pane_event)
            .detach();
        pane
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    });
                }
                self.tabs_changed(cx);
            }
            pane::Event::ActivateItem { .. } | pane::Event::RemovedItem { .. } => {
                self.tabs_changed(cx)
            }
            // A draft that becomes an agent changes what's saved, but not which chat is in front.
            pane::Event::ChangeItemTitle | pane::Event::ItemPinned | pane::Event::ItemUnpinned => {
                self.serialize(cx)
            }
            pane::Event::Remove { focus_on_pane } => {
                // The last pane stays, empty, so the panel keeps its place in the layout.
                if self.center.panes().len() > 1 && self.center.remove(pane, cx).log_err().is_some()
                {
                    if self.active_pane == *pane {
                        self.active_pane = focus_on_pane
                            .clone()
                            .unwrap_or_else(|| self.center.first_pane());
                    }
                    window.focus(&self.active_pane.focus_handle(cx), cx);
                    self.tabs_changed(cx);
                }
            }
            &pane::Event::Split { direction, mode } => {
                let Some(project) = self
                    .workspace
                    .upgrade()
                    .map(|workspace| workspace.read(cx).project().clone())
                else {
                    return;
                };
                let new_pane = Self::new_pane(
                    self.workspace.clone(),
                    project,
                    self.next_timestamp.clone(),
                    window,
                    cx,
                );
                let moved_chat = match mode {
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        Some(item)
                    }
                    SplitMode::ClonePane | SplitMode::EmptyPane => None,
                };
                self.center.split(pane, &new_pane, direction, cx);
                self.active_pane = new_pane.clone();
                window.focus(&new_pane.focus_handle(cx), cx);
                match moved_chat {
                    Some(item) => new_pane.update(cx, |new_pane, cx| {
                        new_pane.add_item(item, true, true, None, window, cx)
                    }),
                    // A chat can't show twice, so a split starts another agent beside it.
                    None => window.dispatch_action(NewAgent.boxed_clone(), cx),
                }
                self.tabs_changed(cx);
            }
            pane::Event::Focus => {
                if self.active_pane != *pane {
                    self.active_pane = pane.clone();
                    self.tabs_changed(cx);
                }
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(true, cx));
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            _ => {}
        }
    }

    fn tabs_changed(&mut self, cx: &mut Context<Self>) {
        self.serialize(cx);
        cx.emit(ChatPanelEvent::ActiveTabChanged);
        cx.notify();
    }

    pub fn active_pane(&self) -> &Entity<Pane> {
        &self.active_pane
    }

    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        self.center.panes()
    }

    /// Every chat in the panel, pane by pane.
    pub fn agent_tabs(&self, cx: &App) -> Vec<Entity<AgentTab>> {
        self.center
            .panes()
            .into_iter()
            .flat_map(|pane| pane.read(cx).items_of_type::<AgentTab>())
            .collect()
    }

    /// The chat in front of the active pane.
    pub fn active_agent_tab(&self, cx: &App) -> Option<Entity<AgentTab>> {
        self.active_pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<AgentTab>())
    }

    /// Brings `tab` to the front of its pane and makes that pane the active one.
    pub fn activate_tab(
        &mut self,
        tab: &Entity<AgentTab>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_for(tab, cx) else {
            return false;
        };
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(tab) {
                pane.activate_item(index, true, focus, window, cx);
            }
        });
        // The pane announces a tab it brings to the front, but not that it became the active
        // pane.
        if self.active_pane != pane {
            self.active_pane = pane;
            self.tabs_changed(cx);
        }
        true
    }

    /// The pane holding `tab`, if the panel holds it.
    pub fn pane_for(&self, tab: &Entity<AgentTab>, cx: &App) -> Option<Entity<Pane>> {
        self.center
            .panes()
            .into_iter()
            .find(|pane| pane.read(cx).index_for_item(tab).is_some())
            .cloned()
    }

    fn serialize(&mut self, cx: &App) {
        let Some(key) = self
            .workspace
            .upgrade()
            .and_then(|workspace| serialization_key(workspace.read(cx)))
        else {
            return;
        };
        let Some(value) = serde_json::to_string(&self.serialized(&self.center.root, cx)).log_err()
        else {
            return;
        };
        if self.last_saved.as_ref() == Some(&value) {
            return;
        }
        self.last_saved = Some(value.clone());
        let kvp = KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move { kvp.write_kvp(key, value).await });
    }

    fn serialized(&self, member: &Member, cx: &App) -> SerializedChatPanes {
        match member {
            Member::Axis(PaneAxis {
                axis,
                members,
                flexes,
                ..
            }) => SerializedChatPanes::Split {
                vertical: *axis == Axis::Vertical,
                flexes: flexes.lock().clone(),
                children: members
                    .iter()
                    .map(|member| self.serialized(member, cx))
                    .collect(),
            },
            Member::Pane(pane) => {
                let pane_ref = pane.read(cx);
                let active_item = pane_ref.active_item().map(|item| item.item_id());
                let mut active_tab = None;
                let mut tabs = Vec::new();
                for item in pane_ref.items() {
                    let Some(tab) = item.downcast::<AgentTab>() else {
                        continue;
                    };
                    // Drafts and subagents aren't saved, as in the editor before.
                    let Some(agent_id) = tab.read(cx).agent_id(cx).filter(|agent_id| {
                        paseo_client::parse_subagent_timeline_id(agent_id).is_none()
                    }) else {
                        continue;
                    };
                    if Some(item.item_id()) == active_item {
                        active_tab = Some(tabs.len());
                    }
                    tabs.push(SerializedChatTab {
                        agent_id,
                        host: hosts::host_name(&tab.read(cx).view().read(cx).store, cx),
                    });
                }
                SerializedChatPanes::Pane {
                    tabs,
                    active_tab,
                    focused: *pane == self.active_pane,
                }
            }
        }
    }

    /// Runs while the workspace is being updated, so it takes the project rather than reading the
    /// workspace.
    fn restore(
        &mut self,
        saved: SerializedChatPanes,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.restored_member(saved, project, window, cx) else {
            return;
        };
        self.center = PaneGroup::with_root(root);
        if !self.center.panes().contains(&&self.active_pane) {
            self.active_pane = self.center.first_pane();
        }
    }

    /// Rebuilds a saved split, leaving out panes that had no saved chat.
    fn restored_member(
        &mut self,
        saved: SerializedChatPanes,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Member> {
        match saved {
            SerializedChatPanes::Pane {
                tabs,
                active_tab,
                focused,
            } => {
                if tabs.is_empty() {
                    return None;
                }
                let pane = Self::new_pane(
                    self.workspace.clone(),
                    project.clone(),
                    self.next_timestamp.clone(),
                    window,
                    cx,
                );
                for tab in tabs {
                    let workspace = self.workspace.clone();
                    let tab = cx.new(|cx| {
                        AgentTab::restored(tab.agent_id, tab.host, workspace, window, cx)
                    });
                    pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(tab), false, false, None, window, cx)
                    });
                }
                if let Some(active_tab) = active_tab {
                    pane.update(cx, |pane, cx| {
                        if active_tab < pane.items_len() {
                            pane.activate_item(active_tab, false, false, window, cx);
                        }
                    });
                }
                if focused {
                    self.active_pane = pane.clone();
                }
                Some(Member::Pane(pane))
            }
            SerializedChatPanes::Split {
                vertical,
                flexes,
                children,
            } => {
                let mut members = Vec::new();
                let mut kept_flexes = Vec::new();
                for (index, child) in children.into_iter().enumerate() {
                    if let Some(member) = self.restored_member(child, project, window, cx) {
                        members.push(member);
                        kept_flexes.push(flexes.get(index).copied().unwrap_or(1.));
                    }
                }
                if members.len() > 1 {
                    let axis = if vertical {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    };
                    Some(Member::Axis(PaneAxis::load(
                        axis,
                        members,
                        Some(kept_flexes),
                    )))
                } else {
                    members.pop()
                }
            }
        }
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> impl IntoElement {
        let focus_handle = self.active_pane.focus_handle(cx);
        v_flex()
            .absolute()
            .inset_0()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Label::new("No agent chats open")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("paseo-chat-panel-new-agent", "New Agent")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .key_binding(ui::KeyBinding::for_action_in(&NewAgent, &focus_handle, cx))
                    .on_click(|_, window, cx| window.dispatch_action(NewAgent.boxed_clone(), cx)),
            )
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ChatPanel {
    /// What the panel saves.
    pub fn test_saved(&self, cx: &App) -> String {
        serde_json::to_string(&self.serialized(&self.center.root, cx)).unwrap_or_default()
    }

    /// Rebuilds the panel from what it saved.
    pub fn test_restore(
        &mut self,
        saved: &str,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(saved) = serde_json::from_str(saved).log_err() {
            self.restore(saved, project, window, cx);
        }
    }
}

impl Focusable for ChatPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The active pane focuses its chat, whose composer takes the keyboard.
        if self.active_pane.read(cx).items_len() > 0 {
            self.active_pane.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self
            .center
            .panes()
            .into_iter()
            .all(|pane| pane.read(cx).items_len() == 0);
        div()
            .id("paseo-chat-panel")
            .key_context("PaseoChatPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .child(self.center.render(
                None,
                None,
                &ActivePaneDecorator::new(&self.active_pane, &self.workspace),
                window,
                cx,
            ))
            .when(empty, |panel| panel.child(self.render_empty_state(cx)))
    }
}

impl Panel for ChatPanel {
    fn persistent_name() -> &'static str {
        "PaseoChatPanel"
    }

    fn panel_key() -> &'static str {
        "PaseoChatPanel"
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
        px(520.)
    }

    // The status bar's layout buttons show and hide the chat panel, so the dock's own panel
    // button would repeat them.
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Agent Chats")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleChatPanel)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn is_zoomed(&self, _: &Window, cx: &App) -> bool {
        self.active_pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| pane.set_zoomed(zoomed, cx));
        }
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }
}

/// The chat panel of `workspace`, once its panels are loaded.
pub(crate) fn chat_panel(workspace: &Workspace, cx: &App) -> Option<Entity<ChatPanel>> {
    workspace.panel::<ChatPanel>(cx)
}

/// Every chat open in `workspace`: the chat panel's, then any still in the editor because they
/// opened or were restored before the chat panel loaded.
pub(crate) fn agent_tabs(workspace: &Workspace, cx: &App) -> Vec<Entity<AgentTab>> {
    let mut tabs = chat_panel(workspace, cx)
        .map(|panel| panel.read(cx).agent_tabs(cx))
        .unwrap_or_default();
    tabs.extend(workspace.items_of_type::<AgentTab>(cx));
    tabs
}

/// The chat in front of `workspace`'s chat panel, or of the editor before the panel loads.
pub(crate) fn active_agent_tab(workspace: &Workspace, cx: &App) -> Option<Entity<AgentTab>> {
    match chat_panel(workspace, cx) {
        Some(panel) => panel.read(cx).active_agent_tab(cx),
        None => workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<AgentTab>()),
    }
}

/// The pane holding `tab`, in the chat panel or the editor.
pub(crate) fn pane_for_tab(
    workspace: &Workspace,
    tab: &Entity<AgentTab>,
    cx: &App,
) -> Option<Entity<Pane>> {
    chat_panel(workspace, cx)
        .and_then(|panel| panel.read(cx).pane_for(tab, cx))
        .or_else(|| workspace.pane_for(tab))
}

/// Whether the editor's panes take `dragged`: every tab but a chat, which belongs in the chat
/// panel.
pub fn editor_accepts_drop(dragged: &dyn Any) -> bool {
    !is_dragged_chat(dragged)
}

/// The pane new chats open in. Before the panels load, the editor's active pane, whose chats
/// move into the chat panel once it exists.
pub(crate) fn chat_pane(workspace: &Workspace, cx: &App) -> Entity<Pane> {
    chat_panel(workspace, cx)
        .map(|panel| panel.read(cx).active_pane().clone())
        .unwrap_or_else(|| workspace.active_pane().clone())
}

/// Adds `tab` to the chat panel's active pane, showing the panel when the tab is activated.
pub(crate) fn add_agent_tab(
    workspace: &mut Workspace,
    tab: Box<dyn ItemHandle>,
    activate: bool,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if activate {
        workspace.reveal_panel::<ChatPanel>(window, cx);
    }
    let pane = chat_pane(workspace, cx);
    pane.update(cx, |pane, cx| {
        pane.add_item_inner(tab, activate, focus, activate, None, window, cx)
    });
}

/// Brings `tab` to the front, showing the chat panel. Returns whether `workspace` holds it.
pub(crate) fn activate_agent_tab(
    workspace: &mut Workspace,
    tab: &Entity<AgentTab>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if let Some(panel) = chat_panel(workspace, cx)
        && panel.read(cx).pane_for(tab, cx).is_some()
    {
        workspace.reveal_panel::<ChatPanel>(window, cx);
        return panel.update(cx, |panel, cx| panel.activate_tab(tab, focus, window, cx));
    }
    workspace.activate_item(tab, true, focus, window, cx)
}

/// Moves chats out of the editor into the chat panel: tabs restored by builds before the chat
/// panel, chats opened before it loaded, and tabs Zed's own commands reopen there.
pub fn move_chats_into_chat_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if chat_panel(workspace, cx).is_none() {
        return;
    }
    let active_item = workspace.active_item(cx).map(|item| item.item_id());
    let tabs = workspace.items_of_type::<AgentTab>(cx).collect::<Vec<_>>();
    for tab in tabs {
        let (view, owner_check_pending, agent_id) = {
            let tab = tab.read(cx);
            (
                tab.view().clone(),
                tab.owner_check_pending,
                tab.agent_id(cx),
            )
        };
        let was_active = active_item == Some(tab.entity_id());
        crate::workspace_tabs::detach_tab(workspace, &tab, window, cx);
        // The saved chat panel may already hold the agent, restored apart from the editor.
        let open_in_panel = agent_id.and_then(|agent_id| {
            chat_panel(workspace, cx)?
                .read(cx)
                .agent_tabs(cx)
                .into_iter()
                .find(|open| open.read(cx).agent_id(cx).as_deref() == Some(agent_id.as_str()))
        });
        match open_in_panel {
            Some(open) => {
                if was_active {
                    activate_agent_tab(workspace, &open, false, window, cx);
                }
            }
            None => {
                let moved = AgentTab::for_view(view, cx.weak_entity(), owner_check_pending, cx);
                add_agent_tab(workspace, Box::new(moved), was_active, false, window, cx);
            }
        }
    }
}

/// Shows the chat panel, or hides it.
pub(crate) fn toggle_chat_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if chat_panel_shown(workspace, cx) {
        workspace.close_panel::<ChatPanel>(window, cx);
    } else {
        workspace.reveal_panel::<ChatPanel>(window, cx);
        if let Some(panel) = chat_panel(workspace, cx) {
            window.focus(&panel.focus_handle(cx), cx);
        }
    }
}

/// Whether `workspace` shows its chat panel.
pub(crate) fn chat_panel_shown(workspace: &Workspace, cx: &App) -> bool {
    let Some(panel) = chat_panel(workspace, cx) else {
        return false;
    };
    workspace.all_docks().into_iter().any(|dock| {
        let dock = dock.read(cx);
        dock.is_open()
            && dock
                .visible_panel()
                .is_some_and(|visible| visible.panel_id() == panel.entity_id())
    })
}
