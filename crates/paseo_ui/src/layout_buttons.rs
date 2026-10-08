//! Status bar buttons that show and hide each area of the window, each at the edge on the side of
//! the area it controls: the agents list, chat panel and editor on the left, the right dock on the
//! right.

use gpui::{App, AppContext as _, Context, Entity, Subscription, WeakEntity, Window};
use ui::{Divider, Indicator, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

pub struct LayoutButtons {
    workspace: WeakEntity<Workspace>,
    side: Side,
    /// What the buttons show, kept so changes elsewhere in the workspace or on a host re-render
    /// them only when an area is shown or hidden.
    shown: Shown,
    /// The window's agents list state. The status bar is built before the workspace joins its
    /// window, so this starts once it has.
    multi_workspace_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Shown {
    agents: bool,
    /// A hidden list can't show its bell, so its button does.
    agents_need_you: bool,
    chat: bool,
    editor: bool,
    right_dock: bool,
}

/// The buttons for the status bar's far left and far right.
pub fn layout_buttons(
    workspace: &Entity<Workspace>,
    cx: &mut App,
) -> (Entity<LayoutButtons>, Entity<LayoutButtons>) {
    (
        cx.new(|cx| LayoutButtons::new(workspace, Side::Left, cx)),
        cx.new(|cx| LayoutButtons::new(workspace, Side::Right, cx)),
    )
}

impl LayoutButtons {
    fn new(workspace: &Entity<Workspace>, side: Side, cx: &mut Context<Self>) -> Self {
        // The status bar is built while the workspace is being updated, so its docks are read
        // after.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Err(error) = this.update(cx, |buttons, cx| buttons.observe_docks(cx)) {
                log::debug!("Paseo layout buttons released: {error}");
            }
        });
        let mut subscriptions = Vec::new();
        if side == Side::Left {
            subscriptions.push(cx.observe(workspace, |buttons, _, cx| buttons.refresh(cx)));
            subscriptions.push(cx.observe(&crate::hosts::registry(cx), |buttons, _, cx| {
                buttons.refresh(cx)
            }));
        }
        Self {
            workspace: workspace.downgrade(),
            side,
            shown: Shown::default(),
            multi_workspace_subscription: None,
            _subscriptions: subscriptions,
        }
    }

    fn observe_docks(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace = workspace.read(cx);
        let docks = match self.side {
            // The chat panel can be moved to either side dock.
            Side::Left => workspace
                .all_docks()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>(),
            Side::Right => vec![workspace.right_dock().clone()],
        };
        for dock in &docks {
            self._subscriptions
                .push(cx.observe(dock, |buttons, _, cx| buttons.refresh(cx)));
        }
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let shown = match self.side {
            Side::Left => {
                let multi_workspace = workspace
                    .read(cx)
                    .multi_workspace()
                    .and_then(|multi_workspace| multi_workspace.upgrade());
                if self.multi_workspace_subscription.is_none()
                    && let Some(multi_workspace) = &multi_workspace
                {
                    self.multi_workspace_subscription =
                        Some(cx.observe(multi_workspace, |buttons, _, cx| buttons.refresh(cx)));
                }
                let agents = multi_workspace
                    .is_some_and(|multi_workspace| multi_workspace.read(cx).sidebar_open());
                let workspace = workspace.read(cx);
                Shown {
                    agents,
                    agents_need_you: !agents && crate::hosts::activity(cx).attention.count > 0,
                    chat: crate::chat_panel::chat_panel_shown(workspace, cx),
                    editor: !workspace.editor_area_hidden(),
                    right_dock: false,
                }
            }
            Side::Right => Shown {
                right_dock: workspace.read(cx).right_dock().read(cx).is_open(),
                ..Shown::default()
            },
        };
        if shown != self.shown {
            self.shown = shown;
            cx.notify();
        }
    }
}

fn layout_button(
    id: &'static str,
    icon: IconName,
    shown: bool,
    name: &'static str,
    action: Box<dyn gpui::Action>,
) -> IconButton {
    let tooltip_action = action.boxed_clone();
    let verb = if shown { "Hide" } else { "Show" };
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .toggle_state(shown)
        .aria_label(format!("{verb} {name}"))
        .tooltip(move |_, cx| {
            Tooltip::for_action(format!("{verb} {name}"), tooltip_action.as_ref(), cx)
        })
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

impl Render for LayoutButtons {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.shown;
        let divider = || Divider::vertical().color(ui::DividerColor::Border);
        if self.side == Side::Right {
            return h_flex().gap_0p5().child(divider()).child(layout_button(
                "paseo-layout-right-dock",
                if shown.right_dock {
                    IconName::ThreadsSidebarRightOpen
                } else {
                    IconName::ThreadsSidebarRightClosed
                },
                shown.right_dock,
                "Right Dock",
                Box::new(workspace::ToggleRightDock),
            ));
        }
        h_flex()
            .gap_0p5()
            .child(
                layout_button(
                    "paseo-layout-agents",
                    if shown.agents {
                        IconName::ThreadsSidebarLeftOpen
                    } else {
                        IconName::ThreadsSidebarLeftClosed
                    },
                    shown.agents,
                    "Agents",
                    Box::new(crate::TogglePanel),
                )
                .when(shown.agents_need_you, |button| {
                    button
                        .indicator(Indicator::dot().color(Color::Accent))
                        .indicator_border_color(Some(cx.theme().colors().status_bar_background))
                }),
            )
            .child(layout_button(
                "paseo-layout-chat",
                IconName::Chat,
                shown.chat,
                "Chat",
                Box::new(crate::ToggleChatPanel),
            ))
            .child(layout_button(
                "paseo-layout-editor",
                IconName::FileCode,
                shown.editor,
                "Editor",
                Box::new(workspace::ToggleEditorArea),
            ))
            .child(divider())
    }
}

impl StatusItemView for LayoutButtons {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    // The window's layout controls are always shown.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
