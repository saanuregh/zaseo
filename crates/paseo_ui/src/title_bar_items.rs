//! Paseo's part of the title bar: the active tab's agent after the project name, and how many
//! agents run or need the user on the right, visible even while the sidebar is closed.

use gpui::{AnyView, App, AppContext as _, Context, Entity, Subscription, WeakEntity, Window};
use ui::{CommonAnimationExt as _, Tooltip, prelude::*};
use workspace::Workspace;

use crate::attention::HostActivity;
use crate::sidebar::AgentAlert;
use crate::store::AgentBucket;

/// The views to put after the project name and at the start of the title bar's right side.
pub fn title_bar_items(workspace: &Entity<Workspace>, cx: &mut App) -> (AnyView, AnyView) {
    let agent = cx.new(|cx| TitleBarAgent::new(workspace, cx));
    let status = cx.new(|cx| TitleBarStatus::new(workspace.downgrade(), cx));
    (agent.into(), status.into())
}

struct TitleBarAgent {
    workspace: WeakEntity<Workspace>,
    /// The active tab's agent title and bucket, kept so a host change that doesn't touch them
    /// doesn't re-render the title bar.
    shown: Option<(String, AgentBucket)>,
    _subscriptions: Vec<Subscription>,
}

impl TitleBarAgent {
    fn new(workspace: &Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&crate::hosts::registry(cx), |agent, _, cx| {
                agent.update_shown(cx)
            }),
            cx.subscribe(workspace, |agent, _, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    agent.update_shown(cx);
                }
            }),
        ];
        // The title bar is built while the workspace is being updated, so its tab is read after.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Err(error) = this.update(cx, |agent, cx| agent.update_shown(cx)) {
                log::debug!("Paseo title bar released: {error}");
            }
        });
        Self {
            workspace: workspace.downgrade(),
            shown: None,
            _subscriptions: subscriptions,
        }
    }

    fn active_agent(&self, cx: &App) -> Option<(String, AgentBucket)> {
        let agent_id = self.workspace.upgrade().and_then(|workspace| {
            crate::chat_panel::active_agent_tab(workspace.read(cx), cx)
                .and_then(|tab| tab.read(cx).agent_id(cx))
        })?;
        let store = crate::hosts::store_for_agent(&agent_id, cx)?;
        let store = store.read(cx);
        let agent = store.agent(&agent_id)?;
        Some((store.display_title(agent), store.bucket(agent)))
    }

    fn update_shown(&mut self, cx: &mut Context<Self>) {
        let shown = self.active_agent(cx);
        if shown != self.shown {
            self.shown = shown;
            cx.notify();
        }
    }
}

impl Render for TitleBarAgent {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some((title, bucket)) = self.shown.clone() else {
            return div().into_any_element();
        };
        let color = AgentAlert::for_bucket(bucket).map_or(Color::Muted, AgentAlert::border_color);
        h_flex()
            .gap_1()
            .min_w_0()
            .child(
                Icon::new(IconName::ChevronRight)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_w_0()
                    .max_w(rems(24.))
                    .child(Label::new(title).size(LabelSize::Small).truncate()),
            )
            .child(Label::new("·").size(LabelSize::Small).color(Color::Muted))
            .child(
                Label::new(bucket.label())
                    .size(LabelSize::Small)
                    .color(color),
            )
            .into_any_element()
    }
}

struct TitleBarStatus {
    workspace: WeakEntity<Workspace>,
    /// Read when a host changes, not in `render`, which the spinner's animation runs every
    /// frame.
    activity: HostActivity,
    _subscriptions: [Subscription; 2],
}

impl TitleBarStatus {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let subscriptions = [
            cx.observe(&crate::hosts::registry(cx), |status, _, cx| {
                let activity = crate::hosts::activity(cx);
                if activity != status.activity {
                    status.activity = activity;
                    cx.notify();
                }
            }),
            // The bell's count can be turned off in settings.
            cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify()),
        ];
        Self {
            workspace,
            activity: crate::hosts::activity(cx),
            _subscriptions: subscriptions,
        }
    }
}

impl Render for TitleBarStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.activity.running;
        h_flex()
            .gap_1()
            .when(running > 0, |this| {
                this.child(
                    h_flex()
                        .id("paseo-title-bar-running")
                        .gap_1()
                        .px_1()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                        .child(
                            Label::new(format!("{running} running"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text("Paseo agents working now")),
                )
            })
            .child(crate::attention::attention_bell(
                "paseo-title-bar-attention",
                self.workspace.clone(),
                self.activity.attention,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(id: &str, status: &str) -> paseo_client::AgentSummary {
        crate::store::test_agent(id, status, json!({}))
    }

    #[test]
    fn title_bar_counts_running_agents() {
        let mut store = crate::store::PaseoStore::default();
        store.state.test_set_agents(vec![
            agent("working", "running"),
            agent("starting", "initializing"),
            agent("idle", "idle"),
            agent("failed", "error"),
        ]);
        assert_eq!(HostActivity::of_stores([&store]).running, 2);
    }
}
