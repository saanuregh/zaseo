//! The agents that need the user, gathered in one list behind the sidebar's bell instead of one
//! toast per agent that disappears.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, WeakEntity, Window, prelude::*,
};
use settings::Settings as _;
use ui::{IconButton, Indicator, PopoverMenu, Tooltip, prelude::*};
use workspace::Workspace;

use crate::store::{
    AgentBucket, PaseoStore, agent_attention_since, agent_last_error, agent_project_name,
    agent_requires_attention, agent_updated_at,
};
use crate::timeline::format_relative;

/// Why an agent is in the inbox, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AttentionReason {
    NeedsInput,
    Failed,
    Finished,
}

impl AttentionReason {
    pub(crate) fn color(self) -> Color {
        match self {
            Self::NeedsInput => Color::Warning,
            Self::Failed => Color::Error,
            Self::Finished => Color::Accent,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NeedsInput => "Needs input",
            Self::Failed => "Failed",
            Self::Finished => "Finished",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttentionEntry {
    pub agent_id: String,
    pub reason: AttentionReason,
    pub title: String,
    pub project: String,
    /// Since when the agent has needed the user, else its last update.
    pub since: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// The agents that need the user: those waiting for input, then those that failed or finished
/// and aren't read yet, most recent first within each by when the daemon says they started
/// needing the user. A failure already seen stays out of the list.
pub(crate) fn attention_entries(store: &PaseoStore) -> Vec<AttentionEntry> {
    let mut entries = store
        .state
        .agents
        .iter()
        .filter_map(|agent| {
            let reason = match store.bucket(agent) {
                AgentBucket::NeedsInput => AttentionReason::NeedsInput,
                AgentBucket::Failed if agent_requires_attention(agent) => AttentionReason::Failed,
                AgentBucket::Attention => AttentionReason::Finished,
                _ => return None,
            };
            Some(AttentionEntry {
                agent_id: agent.id.clone(),
                reason,
                title: store.display_title(agent),
                project: agent_project_name(agent),
                since: agent_attention_since(agent).or_else(|| agent_updated_at(agent)),
                error: (reason == AttentionReason::Failed)
                    .then(|| agent_last_error(agent).map(str::to_owned))
                    .flatten(),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.reason
            .cmp(&right.reason)
            .then_with(|| right.since.cmp(&left.since))
    });
    entries
}

/// The bell that opens the list of agents that need the user, with their count in the most
/// urgent one's colour.
pub(crate) fn attention_bell(
    id: &'static str,
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> AnyElement {
    let entries = attention_entries(store.read(cx));
    let open_inbox = move |_window: &mut Window, cx: &mut App| {
        let (store, workspace) = (store.clone(), workspace.clone());
        Some(cx.new(|cx| AttentionInbox::new(store, workspace, cx)))
    };
    let menu = PopoverMenu::new(id)
        .anchor(gpui::Anchor::TopRight)
        .menu(open_inbox);
    let show_count = crate::PaseoSettings::get_global(cx).alerts.bell_count;
    let Some(most_urgent) = entries.first() else {
        return menu
            .trigger_with_tooltip(
                IconButton::new("paseo-attention-bell", IconName::Bell)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text("Nothing needs you"),
            )
            .into_any_element();
    };
    let color = most_urgent.reason.color();
    if !show_count {
        return menu
            .trigger_with_tooltip(
                IconButton::new("paseo-attention-bell", IconName::BellDot)
                    .icon_size(IconSize::Small)
                    .icon_color(color),
                Tooltip::text("Agents need you"),
            )
            .into_any_element();
    }
    let tooltip = match entries.len() {
        1 => "1 agent needs you".to_owned(),
        count => format!("{count} agents need you"),
    };
    menu.trigger_with_tooltip(
        ui::Button::new("paseo-attention-bell", entries.len().to_string())
            .label_size(LabelSize::Small)
            .color(color)
            .start_icon(
                Icon::new(IconName::BellDot)
                    .size(IconSize::Small)
                    .color(color),
            ),
        Tooltip::text(tooltip),
    )
    .into_any_element()
}

/// The popover listing [`attention_entries`]; a row opens its agent.
pub(crate) struct AttentionInbox {
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl AttentionInbox {
    pub(crate) fn new(
        store: Entity<PaseoStore>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            workspace,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    fn open(&mut self, agent_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                crate::open_agent(workspace, agent_id, true, window, cx)
            });
        }
        cx.emit(DismissEvent);
    }

    fn mark_read(&mut self, agent_ids: Vec<String>, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            for agent_id in &agent_ids {
                store.clear_attention(agent_id, cx);
            }
        });
    }

    fn render_entry(
        &self,
        index: usize,
        entry: AttentionEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let open_agent = entry.agent_id.clone();
        let read_agent = entry.agent_id.clone();
        let when = entry.since.map(|since| format_relative(since, Utc::now()));
        let details = [
            Some(entry.reason.label().to_owned()),
            Some(entry.project.clone()),
            when,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" • ");
        h_flex()
            .id(("paseo-attention-entry", index))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .items_start()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .on_click(cx.listener(move |inbox, _, window, cx| inbox.open(&open_agent, window, cx)))
            .child(
                div()
                    .pt_1p5()
                    .child(Indicator::dot().color(entry.reason.color())),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(Label::new(entry.title)),
                    )
                    .child(
                        Label::new(details)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(entry.error, |this, error| {
                        this.child(
                            div().line_clamp(2).text_ellipsis().child(
                                Label::new(error).size(LabelSize::Small).color(Color::Error),
                            ),
                        )
                    }),
            )
            .when(entry.reason != AttentionReason::NeedsInput, |this| {
                this.child(
                    IconButton::new(("paseo-attention-read", index), IconName::Check)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Mark as read"))
                        .on_click(cx.listener(move |inbox, _, _, cx| {
                            cx.stop_propagation();
                            inbox.mark_read(vec![read_agent.clone()], cx);
                        })),
                )
            })
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for AttentionInbox {}

impl Focusable for AttentionInbox {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AttentionInbox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = attention_entries(self.store.read(cx));
        let unread = entries
            .iter()
            .filter(|entry| entry.reason != AttentionReason::NeedsInput)
            .map(|entry| entry.agent_id.clone())
            .collect::<Vec<_>>();
        v_flex()
            .key_context("PaseoAttentionInbox")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(rems(24.))
            .max_h(rems(28.))
            .p_1()
            .elevation_2(cx)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(
                        Label::new("Needs you")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(!unread.is_empty(), |this| {
                        this.child(
                            ui::Button::new("paseo-attention-read-all", "Mark all read")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |inbox, _, _, cx| {
                                    inbox.mark_read(unread.clone(), cx)
                                })),
                        )
                    }),
            )
            .child(
                v_flex()
                    .id("paseo-attention-entries")
                    .overflow_y_scroll()
                    .when(entries.is_empty(), |this| {
                        this.child(
                            div().px_2().py_2().child(
                                Label::new("Nothing needs you right now").color(Color::Muted),
                            ),
                        )
                    })
                    .children(
                        entries
                            .into_iter()
                            .enumerate()
                            .map(|(index, entry)| self.render_entry(index, entry, cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(id: &str, status: &str, extra: serde_json::Value) -> paseo_client::AgentSummary {
        crate::store::test_agent(id, status, extra)
    }

    #[test]
    fn agent_status_fields_read_the_snapshot() {
        let flagged = agent(
            "a",
            "idle",
            json!({
                "lastError": "Rate limited",
                "providerUnavailable": true,
                "attentionTimestamp": "2026-09-30T10:00:00Z",
            }),
        );
        let plain = agent("b", "idle", json!({}));
        assert_eq!(
            crate::store::agent_last_error(&flagged),
            Some("Rate limited")
        );
        assert!(crate::store::agent_provider_unavailable(&flagged));
        assert_eq!(
            agent_attention_since(&flagged).map(|since| since.to_rfc3339()),
            Some("2026-09-30T10:00:00+00:00".to_owned())
        );
        assert_eq!(crate::store::agent_last_error(&plain), None);
        assert!(!crate::store::agent_provider_unavailable(&plain));
        assert_eq!(agent_attention_since(&plain), None);
    }

    #[test]
    fn attention_lists_input_failed_then_finished() {
        let mut store = PaseoStore::default();
        store.state.agents = vec![
            agent(
                "finished-old",
                "idle",
                // Updated last, but it has needed the user longest.
                json!({
                    "requiresAttention": true,
                    "updatedAt": "2026-09-30T00:00:09Z",
                    "attentionTimestamp": "2026-09-30T00:00:01Z",
                }),
            ),
            agent(
                "finished-new",
                "idle",
                json!({"requiresAttention": true, "updatedAt": "2026-09-30T00:00:05Z"}),
            ),
            agent("failed-seen", "error", json!({})),
            agent(
                "failed",
                "idle",
                json!({"requiresAttention": true, "attentionReason": "error", "lastError": "Rate limited"}),
            ),
            agent(
                "waiting",
                "running",
                json!({"pendingPermissions": [{"id": "request-1"}]}),
            ),
            agent("idle", "idle", json!({})),
            agent("running", "running", json!({})),
        ];
        let entries = attention_entries(&store);
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.agent_id == "failed")
                .and_then(|entry| entry.error.as_deref()),
            Some("Rate limited")
        );
        let order = entries
            .into_iter()
            .map(|entry| (entry.agent_id, entry.reason))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                ("waiting".to_owned(), AttentionReason::NeedsInput),
                ("failed".to_owned(), AttentionReason::Failed),
                ("finished-new".to_owned(), AttentionReason::Finished),
                ("finished-old".to_owned(), AttentionReason::Finished),
            ]
        );
    }
}
