use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, SharedString, Subscription, Task, TaskExt, Window, prelude::*, px,
    relative,
};
use paseo_client::{ProviderUsage, UsageBalance, UsageWindow};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use ui::{Indicator, Tooltip, prelude::*};
use workspace::{HideStatusItem, Item, ItemHandle, StatusItemView, Workspace, item::ItemEvent};

use crate::composer::format_tokens;
use crate::store::{ConnectionStatus, PaseoStore, agent_is_running, agent_provider};
use crate::timeline::parse_timestamp;

/// Paseo treats usage as fresh for five minutes before refetching on open.
const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Default,
    Warning,
    Danger,
}

pub(crate) fn window_tone(window: &UsageWindow) -> Tone {
    match window.tone.as_deref() {
        Some("danger") => Tone::Danger,
        Some("warning") => Tone::Warning,
        Some(_) => Tone::Default,
        None => match window.used_percent {
            Some(used) if used > 90.0 => Tone::Danger,
            Some(used) if used >= 70.0 => Tone::Warning,
            _ => Tone::Default,
        },
    }
}

fn short_span(seconds: i64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}m", (seconds / 60).max(1))
    }
}

/// Paseo's trailing window text: "runs out 2h" when the provider predicts a shortfall,
/// otherwise "resets 3h" or "resetting now".
pub(crate) fn window_timing(window: &UsageWindow, now: DateTime<Utc>) -> Option<String> {
    if let Some(runs_out) = window.runs_out_at.as_deref().and_then(parse_timestamp) {
        return Some(format!(
            "runs out {}",
            short_span((runs_out - now).num_seconds().max(0))
        ));
    }
    let resets = window.resets_at.as_deref().and_then(parse_timestamp)?;
    let seconds = (resets - now).num_seconds();
    Some(if seconds <= 0 {
        "resetting now".into()
    } else {
        format!("resets {}", short_span(seconds))
    })
}

fn format_amount(amount: f64, unit: &str) -> String {
    match unit {
        "usd" => format!("${amount:.2}"),
        "tokens" if amount >= 1_000.0 => format!("{} tokens", format_tokens(amount as u64)),
        "" => format!("{amount}"),
        unit => format!("{amount} {unit}"),
    }
}

pub(crate) fn balance_text(balance: &UsageBalance) -> String {
    match (balance.used, balance.remaining, balance.limit) {
        (Some(used), _, Some(limit)) => format!(
            "{} of {}",
            format_amount(used, &balance.unit),
            format_amount(limit, &balance.unit)
        ),
        (_, Some(remaining), _) => format!("{} left", format_amount(remaining, &balance.unit)),
        (Some(used), None, None) => format!("{} used", format_amount(used, &balance.unit)),
        _ => "—".into(),
    }
}

fn updated_ago(fetched_at: Option<&str>, now: DateTime<Utc>) -> Option<String> {
    let fetched = parse_timestamp(fetched_at?)?;
    let seconds = (now - fetched).num_seconds();
    Some(if seconds < 60 {
        "Updated just now".into()
    } else {
        format!("Updated {} ago", short_span(seconds))
    })
}

enum UsageState {
    Idle,
    Loading,
    Loaded(Vec<ProviderUsage>),
    Failed(String),
}

/// Paseo's "Plan usage" page: each provider's plan limits for the connected host.
pub struct ProviderUsageView {
    store: Entity<PaseoStore>,
    state: UsageState,
    loaded_at: Option<Instant>,
    /// Usage belongs to one host, so a new connection generation discards it.
    connection: (u64, ConnectionStatus),
    focus_handle: FocusHandle,
    _store_subscription: Subscription,
}

impl ProviderUsageView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = crate::store(cx);
        let subscription = cx.observe(&store, |view, store, cx| {
            let (generation, status) = {
                let store = store.read(cx);
                (store.connection_generation, store.status)
            };
            if view.connection != (generation, status) {
                if generation != view.connection.0 {
                    view.state = UsageState::Idle;
                    view.loaded_at = None;
                }
                view.connection = (generation, status);
                cx.notify();
            }
            if status == ConnectionStatus::Connected && matches!(view.state, UsageState::Idle) {
                view.refresh(cx);
            }
        });
        let connection = {
            let store = store.read(cx);
            (store.connection_generation, store.status)
        };
        let mut view = Self {
            store,
            state: UsageState::Idle,
            loaded_at: None,
            connection,
            focus_handle: cx.focus_handle(),
            _store_subscription: subscription,
        };
        view.refresh(cx);
        view
    }

    fn supported(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .server_info
            .has_feature("providerUsageList")
    }

    fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self
            .loaded_at
            .is_none_or(|loaded_at| loaded_at.elapsed() > STALE_AFTER)
        {
            self.refresh(cx);
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if store.status != ConnectionStatus::Connected || !self.supported(cx) {
            return;
        }
        self.state = UsageState::Loading;
        let generation = store.connection_generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move { session.provider_usage().await })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| {
                // A reply from before a host switch or reconnect belongs to the old host.
                if !view.store.read(cx).is_current_connection(generation) {
                    return;
                }
                view.state = match result {
                    Ok(providers) => UsageState::Loaded(providers),
                    Err(error) => UsageState::Failed(error.to_string()),
                };
                view.loaded_at = Some(Instant::now());
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn render_window(&self, window: &UsageWindow, now: DateTime<Utc>, cx: &App) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let fill = match window_tone(window) {
            Tone::Default => colors.text_accent,
            Tone::Warning => status.warning,
            Tone::Danger => status.error,
        };
        let used = window.used_percent.map(|used| used.clamp(0.0, 100.0));
        let summary = [
            used.map(|used| format!("{used:.0}%")),
            window_timing(window, now),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new(window.label.clone()))
                    .child(
                        Label::new(if summary.is_empty() {
                            "—".into()
                        } else {
                            summary
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .h(px(6.))
                    .w_full()
                    .rounded_full()
                    .bg(colors.element_background)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(fill)
                            .w(relative(used.unwrap_or(0.0) as f32 / 100.0)),
                    ),
            )
            .into_any_element()
    }

    fn render_provider(
        &self,
        index: usize,
        provider: &ProviderUsage,
        now: DateTime<Utc>,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = match provider.status.as_str() {
            "error" => Some(("Error", Color::Error)),
            "unavailable" => Some(("Unavailable", Color::Muted)),
            _ => None,
        };
        let footer = [
            provider.source_label.clone(),
            updated_ago(provider.fetched_at.as_deref(), now),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        v_flex()
            .id(("paseo-usage-provider", index))
            .w_full()
            .p_4()
            .gap_3()
            .rounded(px(12.))
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(provider.display_name.clone()).weight(FontWeight::SEMIBOLD))
                    .when_some(provider.plan_label.clone(), |this, plan| {
                        this.child(
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .bg(colors.element_background)
                                .child(Label::new(plan).size(LabelSize::Small).color(Color::Muted)),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(status, |this, (label, color)| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(Indicator::dot().color(color))
                                .child(Label::new(label).size(LabelSize::Small).color(color)),
                        )
                    }),
            )
            .when_some(provider.error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .line_clamp(3),
                )
            })
            .children(
                provider
                    .windows
                    .iter()
                    .map(|window| self.render_window(window, now, cx)),
            )
            .children(provider.balances.iter().map(|balance| {
                h_flex()
                    .justify_between()
                    .child(Label::new(balance.label.clone()))
                    .child(
                        Label::new(balance_text(balance))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
            }))
            .children(provider.details.iter().map(|detail| {
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(detail.label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(detail.value.clone()))
            }))
            .when(!footer.is_empty(), |this| {
                this.child(
                    Label::new(footer)
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
            })
            .into_any_element()
    }

    fn render_message(message: &'static str) -> AnyElement {
        v_flex()
            .py_8()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for ProviderUsageView {}

impl Focusable for ProviderUsageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProviderUsageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Utc::now();
        let connected = self.store.read(cx).status == ConnectionStatus::Connected;
        let loading = matches!(self.state, UsageState::Loading);
        let body = if !connected {
            Self::render_message("Connect to this host to see provider usage")
        } else if !self.supported(cx) {
            Self::render_message("Update the host to see provider usage")
        } else {
            match &self.state {
                UsageState::Idle | UsageState::Loading => crate::render_loading("Loading usage…"),
                UsageState::Failed(error) => v_flex()
                    .p_4()
                    .gap_2()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(cx.theme().status().error_border)
                    .bg(cx.theme().status().error_background)
                    .child(Label::new("Unable to load usage").weight(FontWeight::SEMIBOLD))
                    .child(Label::new(error.clone()).size(LabelSize::Small))
                    .child(
                        h_flex().child(
                            Button::new("paseo-usage-retry", "Try again")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
                        ),
                    )
                    .into_any_element(),
                UsageState::Loaded(providers) if providers.is_empty() => {
                    Self::render_message("No usage data")
                }
                UsageState::Loaded(providers) => {
                    v_flex()
                        .gap_3()
                        .children(providers.iter().enumerate().map(|(index, provider)| {
                            self.render_provider(index, provider, now, cx)
                        }))
                        .into_any_element()
                }
            }
        };
        let host = self
            .store
            .read(cx)
            .active_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        div()
            .id("paseo-usage")
            .key_context("PaseoUsage")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex().w_full().justify_center().px_4().py_6().child(
                    v_flex()
                        .w_full()
                        .max_w(px(720.))
                        .gap_4()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .child(
                                            Headline::new("Plan usage").size(HeadlineSize::Small),
                                        )
                                        .child(
                                            Label::new(format!(
                                                "Provider plan limits reported by {host}"
                                            ))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    Button::new(
                                        "paseo-usage-refresh",
                                        if loading { "Refreshing…" } else { "Refresh" },
                                    )
                                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                                    .disabled(loading || !connected)
                                    .tooltip(Tooltip::text("Fetch usage from the host again"))
                                    .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
                                ),
                        )
                        .child(body),
                ),
            )
    }
}

impl Item for ProviderUsageView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Provider Usage".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sparkle))
    }
}

/// Opens the usage tab, reusing an open one and refetching when its data is stale.
pub fn open_usage(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<ProviderUsageView>(cx).next();
    if let Some(existing) = existing {
        existing.update(cx, |view, cx| view.refresh_if_stale(cx));
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let view = cx.new(ProviderUsageView::new);
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// A window's reset or run-out time in status-bar form: `2h`, `now`, or `out 2h`.
pub(crate) fn compact_timing(window: &UsageWindow, now: DateTime<Utc>) -> Option<String> {
    if let Some(runs_out) = window.runs_out_at.as_deref().and_then(parse_timestamp) {
        return Some(format!(
            "out {}",
            short_span((runs_out - now).num_seconds().max(0))
        ));
    }
    let seconds = (window.resets_at.as_deref().and_then(parse_timestamp)? - now).num_seconds();
    Some(if seconds <= 0 {
        "now".into()
    } else {
        short_span(seconds)
    })
}

/// The current agent's provider limits in the status bar, like Paseo's usage chip.
pub struct UsageStatusItem {
    store: Entity<PaseoStore>,
    active_agent_id: Option<String>,
    usage: Vec<ProviderUsage>,
    loaded_at: Option<Instant>,
    loading: bool,
    running_agents: HashSet<String>,
    connected: bool,
    _refresh_timer: Task<()>,
    _store_subscription: Subscription,
}

impl UsageStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = crate::store(cx);
        let store_subscription = cx.observe(&store, |item, _, cx| item.store_changed(cx));
        let refresh_timer = cx.spawn(async move |item, cx| {
            loop {
                cx.background_executor().timer(STALE_AFTER).await;
                if item.update(cx, |item, cx| item.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        let mut item = Self {
            store,
            active_agent_id: None,
            usage: Vec::new(),
            loaded_at: None,
            loading: false,
            running_agents: HashSet::new(),
            connected: false,
            _refresh_timer: refresh_timer,
            _store_subscription: store_subscription,
        };
        item.store_changed(cx);
        item
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let connected = store.status == ConnectionStatus::Connected;
        let running_agents: HashSet<String> = store
            .state
            .agents
            .iter()
            .filter(|agent| agent_is_running(agent))
            .map(|agent| agent.id.clone())
            .collect();
        let reconnected = connected && !self.connected;
        let turn_finished = !self.running_agents.is_subset(&running_agents);
        self.connected = connected;
        self.running_agents = running_agents;
        if !connected {
            if !self.usage.is_empty() {
                self.usage.clear();
                self.loaded_at = None;
                cx.notify();
            }
        } else if reconnected || turn_finished {
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if self.loading
            || store.status != ConnectionStatus::Connected
            || !store.server_info.has_feature("providerUsageList")
        {
            return;
        }
        self.loading = true;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move { session.provider_usage().await })
        });
        cx.spawn(async move |item, cx| {
            let result = task.await;
            item.update(cx, |item, cx| {
                item.loading = false;
                match result {
                    Ok(usage) => {
                        item.usage = usage;
                        item.loaded_at = Some(Instant::now());
                    }
                    Err(error) => log::warn!("Paseo provider usage failed: {error:#}"),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn provider_usage(&self, cx: &App) -> Option<&ProviderUsage> {
        let store = self.store.read(cx);
        let agent_id = self
            .active_agent_id
            .as_deref()
            .or(store.focused_agent.as_deref())?;
        let provider = agent_provider(store.agent(agent_id)?);
        self.usage
            .iter()
            .find(|usage| usage.provider_id == provider && usage.status == "available")
    }
}

impl Render for UsageStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(usage) = self.provider_usage(cx) else {
            return div().into_any_element();
        };
        if usage.windows.is_empty() && usage.balances.is_empty() {
            return div().into_any_element();
        }
        let now = Utc::now();
        let percent =
            |window: &UsageWindow| format!("{:.0}%", window.used_percent.unwrap_or_default());
        let tooltip_lines = usage
            .plan_label
            .iter()
            .map(|plan| format!("Plan: {plan}"))
            .chain(usage.windows.iter().map(|window| {
                let timing = window_timing(window, now)
                    .map(|timing| format!(" · {timing}"))
                    .unwrap_or_default();
                format!("{}: {}{timing}", window.label, percent(window))
            }))
            .chain(
                usage
                    .balances
                    .iter()
                    .map(|balance| format!("{}: {}", balance.label, balance_text(balance))),
            )
            .chain(
                usage
                    .details
                    .iter()
                    .map(|detail| format!("{}: {}", detail.label, detail.value)),
            )
            .collect::<Vec<_>>()
            .join("\n");
        let title = format!("{} usage", usage.display_name);
        // Window labels such as `Weekly · Fable` contain dots, so windows are split by rules.
        let separator = || {
            ui::Divider::vertical()
                .color(ui::DividerColor::Border)
                .into_any_element()
        };
        let mut segments: Vec<AnyElement> = Vec::new();
        for window in &usage.windows {
            if !segments.is_empty() {
                segments.push(separator());
            }
            let color = match window_tone(window) {
                Tone::Default => Color::Muted,
                Tone::Warning => Color::Warning,
                Tone::Danger => Color::Error,
            };
            segments.push(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(format!("{} {}", window.label, percent(window)))
                            .size(LabelSize::Small)
                            .color(color),
                    )
                    .children(compact_timing(window, now).map(|timing| {
                        Label::new(timing)
                            .size(LabelSize::Small)
                            .color(Color::Placeholder)
                    }))
                    .into_any_element(),
            );
        }
        for balance in &usage.balances {
            if !segments.is_empty() {
                segments.push(separator());
            }
            segments.push(
                Label::new(format!("{} {}", balance.label, balance_text(balance)))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        h_flex()
            .id("paseo-usage-status")
            .px_1()
            .gap_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Label::new(usage.display_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .children(segments)
            .tooltip(move |_, cx| {
                Tooltip::with_meta(title.clone(), None, tooltip_lines.clone(), cx)
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(crate::OpenProviderUsage), cx)
            })
            .into_any_element()
    }
}

impl StatusItemView for UsageStatusItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent_id = active_pane_item
            .and_then(|item| item.downcast::<crate::AgentTab>())
            .and_then(|tab| tab.read(cx).agent_id(cx));
        // Other tabs, such as files, keep showing the agent that was last in view.
        if agent_id.is_some() && agent_id != self.active_agent_id {
            self.active_agent_id = agent_id;
            if self
                .loaded_at
                .is_none_or(|loaded_at| loaded_at.elapsed() > STALE_AFTER)
            {
                self.refresh(cx);
            }
            cx.notify();
        }
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used_percent: Option<f64>) -> UsageWindow {
        UsageWindow {
            label: "5-hour".into(),
            used_percent,
            resets_at: None,
            runs_out_at: None,
            tone: None,
        }
    }

    #[test]
    fn compact_timing_shortens_reset_and_run_out_times() {
        let now = parse_timestamp("2026-09-26T10:00:00Z").expect("time");
        let mut usage = window(Some(52.0));
        assert_eq!(compact_timing(&usage, now), None);
        usage.resets_at = Some("2026-09-26T12:30:00Z".into());
        assert_eq!(compact_timing(&usage, now).as_deref(), Some("2h"));
        usage.resets_at = Some("2026-09-26T09:00:00Z".into());
        assert_eq!(compact_timing(&usage, now).as_deref(), Some("now"));
        usage.runs_out_at = Some("2026-09-27T10:00:00Z".into());
        assert_eq!(compact_timing(&usage, now).as_deref(), Some("out 1d"));
    }

    #[test]
    fn tones_follow_paseo_thresholds() {
        assert_eq!(window_tone(&window(Some(50.0))), Tone::Default);
        assert_eq!(window_tone(&window(Some(70.0))), Tone::Warning);
        assert_eq!(window_tone(&window(Some(91.0))), Tone::Danger);
        let mut explicit = window(Some(95.0));
        explicit.tone = Some("ok".into());
        assert_eq!(window_tone(&explicit), Tone::Default);
    }

    #[test]
    fn timing_prefers_run_out_prediction() {
        let now = parse_timestamp("2026-09-26T10:00:00Z").expect("time");
        let mut usage = window(Some(40.0));
        usage.resets_at = Some("2026-09-26T13:30:00Z".into());
        assert_eq!(window_timing(&usage, now).as_deref(), Some("resets 3h"));
        usage.resets_at = Some("2026-09-26T09:00:00Z".into());
        assert_eq!(window_timing(&usage, now).as_deref(), Some("resetting now"));
        usage.runs_out_at = Some("2026-09-28T10:00:00Z".into());
        assert_eq!(window_timing(&usage, now).as_deref(), Some("runs out 2d"));
    }

    #[test]
    fn balances_read_naturally() {
        let balance = UsageBalance {
            label: "Credits".into(),
            used: Some(3.5),
            remaining: None,
            limit: Some(20.0),
            unit: "usd".into(),
            tone: None,
        };
        assert_eq!(balance_text(&balance), "$3.50 of $20.00");
        let tokens = UsageBalance {
            used: None,
            remaining: Some(1_500_000.0),
            limit: None,
            unit: "tokens".into(),
            ..balance
        };
        assert_eq!(balance_text(&tokens), "1.5M tokens left");
    }
}
