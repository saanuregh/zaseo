//! Paseo's part of the title bar: the active tab's agent after the project name, and how many
//! agents run or need the user on the right, visible even while the sidebar is closed.

use gpui::{AnyView, App, AppContext as _, Context, Entity, Subscription, WeakEntity, Window};
use ui::{CommonAnimationExt as _, Tooltip, prelude::*};
use workspace::Workspace;

use crate::agent_view::AgentTab;
use crate::sidebar::AgentAlert;
use crate::store::{AgentBucket, PaseoStore};

/// The views to put after the project name and at the start of the title bar's right side.
pub fn title_bar_items(workspace: &Entity<Workspace>, cx: &mut App) -> (AnyView, AnyView) {
    let store = crate::store(cx);
    let agent = cx.new(|cx| TitleBarAgent::new(store.clone(), workspace, cx));
    let status = cx.new(|cx| TitleBarStatus::new(store, workspace.downgrade(), cx));
    (agent.into(), status.into())
}

struct TitleBarAgent {
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl TitleBarAgent {
    fn new(
        store: Entity<PaseoStore>,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(workspace, |_, _, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    cx.notify();
                }
            }),
        ];
        Self {
            store,
            workspace: workspace.downgrade(),
            _subscriptions: subscriptions,
        }
    }
}

impl Render for TitleBarAgent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent_id = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .active_item(cx)
                .and_then(|item| item.downcast::<AgentTab>())
                .and_then(|tab| tab.read(cx).agent_id(cx))
        });
        let store = self.store.read(cx);
        let Some((title, bucket)) = agent_id.and_then(|agent_id| {
            let agent = store.agent(&agent_id)?;
            Some((store.display_title(agent), store.bucket(agent)))
        }) else {
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
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    _subscription: Subscription,
}

impl TitleBarStatus {
    fn new(
        store: Entity<PaseoStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            workspace,
            _subscription: subscription,
        }
    }
}

fn running_agent_count(store: &PaseoStore) -> usize {
    store
        .state
        .agents
        .iter()
        .filter(|agent| store.bucket(agent) == AgentBucket::Running)
        .count()
}

impl Render for TitleBarStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = running_agent_count(self.store.read(cx));
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
                self.store.clone(),
                self.workspace.clone(),
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
        let mut store = PaseoStore::default();
        store.state.agents = vec![
            agent("working", "running"),
            agent("starting", "initializing"),
            agent("idle", "idle"),
            agent("failed", "error"),
        ];
        assert_eq!(running_agent_count(&store), 2);
    }
}
