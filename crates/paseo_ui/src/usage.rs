use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use fs::Fs;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, FontWeight, Global, IntoElement, SharedString,
    Subscription, Task, TaskExt, Window, prelude::*, px, relative,
};
use paseo_client::{
    UsageBalance, UsageProblem, UsageReport, UsageReportEntry, UsageReportsRequest, UsageWindow,
};
use settings::{PaseoUsageDisplay, Settings as _, SettingsStore};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use ui::{Indicator, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, ModalView, StatusItemView, Workspace};

use crate::PaseoSettings;
use crate::attention::HostActivity;
use crate::composer::format_tokens;
use crate::store::{PaseoStore, agent_provider};
use crate::timeline::parse_timestamp;

/// Paseo treats usage as fresh for five minutes before refetching on open.
const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
/// Paseo reuses an agent's usage for a minute before its context card asks again.
const AGENT_USAGE_FRESH_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Default,
    Warning,
    Danger,
}

fn window_tone(window: &UsageWindow) -> Tone {
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
fn window_timing(window: &UsageWindow, now: DateTime<Utc>) -> Option<String> {
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

fn balance_text(balance: &UsageBalance) -> String {
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

fn updated_ago(fetched_at: &str, now: DateTime<Utc>) -> Option<String> {
    let fetched = parse_timestamp(fetched_at)?;
    let seconds = (now - fetched).num_seconds();
    Some(if seconds < 60 {
        "Updated just now".into()
    } else {
        format!("Updated {} ago", short_span(seconds))
    })
}

/// How long ago `time` was, as Paseo words it in a sentence: "just now", "5m ago", "2d ago", and
/// past a week the date. A time still ahead reads as "just now", as in Paseo.
fn time_ago_prose(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = (now - time).num_seconds();
    if elapsed < 60 {
        "just now".into()
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else if elapsed < 7 * 86_400 {
        format!("{}d ago", elapsed / 86_400)
    } else {
        time.with_timezone(&Local).format("%b %-d").to_string()
    }
}

/// Why an account's usage can't be read, in Paseo's words.
fn problem_text(problem: &UsageProblem, now: DateTime<Utc>) -> String {
    let remedy = |refreshed_by: &Option<String>| match refreshed_by.as_deref() {
        Some(command) if !command.is_empty() => format!("Run {command} to refresh it."),
        _ => "Sign in again.".to_owned(),
    };
    match problem {
        UsageProblem::NoQuota { detail } => detail.clone(),
        UsageProblem::Rejected {
            status,
            refreshed_by,
        } => format!("Login rejected (HTTP {status}). {}", remedy(refreshed_by)),
        UsageProblem::Expired {
            expires_at,
            refreshed_by,
        } => match parse_timestamp(expires_at) {
            Some(expires_at) => format!(
                "Login expired {}. {}",
                time_ago_prose(expires_at, now),
                remedy(refreshed_by)
            ),
            None => format!("Login expired. {}", remedy(refreshed_by)),
        },
    }
}

fn report_message(report: &UsageReport, now: DateTime<Utc>) -> Option<String> {
    match report {
        UsageReport::Available { .. } => None,
        UsageReport::Unavailable(problem) => Some(problem_text(problem, now)),
        UsageReport::Error(error) => Some(error.clone()),
    }
}

/// The lines a card shows in red: one per failed login when an account has several, else why
/// the report failed.
fn report_messages(entry: &UsageReportEntry, now: DateTime<Utc>) -> Vec<String> {
    if matches!(entry.report, UsageReport::Available { .. }) {
        return Vec::new();
    }
    if !entry.login_errors.is_empty() {
        return entry
            .login_errors
            .iter()
            .map(|login| {
                format!(
                    "{}: {}",
                    login.harness,
                    report_message(&login.report, now).unwrap_or_default()
                )
            })
            .collect();
    }
    report_message(&entry.report, now).into_iter().collect()
}

/// The share of a window to show under the user's used/remaining preference, from 0 to 100.
fn displayed_percent(window: &UsageWindow, display_as: PaseoUsageDisplay) -> Option<f64> {
    let used = window.used_percent?.clamp(0.0, 100.0);
    Some(match display_as {
        PaseoUsageDisplay::Used => used,
        PaseoUsageDisplay::Remaining => 100.0 - used,
    })
}

fn percent_text(percent: f64) -> String {
    format!("{}%", percent.round() as i64)
}

/// A window row's value: "31%" used or "69% left", "—" without a percent.
fn window_value(window: &UsageWindow, display_as: PaseoUsageDisplay) -> String {
    match (displayed_percent(window, display_as), display_as) {
        (None, _) => "—".into(),
        (Some(percent), PaseoUsageDisplay::Used) => percent_text(percent),
        (Some(percent), PaseoUsageDisplay::Remaining) => format!("{} left", percent_text(percent)),
    }
}

/// A window in the status bar: the percent and its short name, like Paseo's sidebar summary,
/// which never adds "left".
fn status_window_text(window: &UsageWindow, display_as: PaseoUsageDisplay) -> String {
    let percent = displayed_percent(window, display_as)
        .map(percent_text)
        .unwrap_or_else(|| "—".into());
    match window.short_label.as_deref() {
        Some("") => percent,
        Some(short_label) => format!("{percent} {short_label}"),
        None => format!("{percent} {}", window.label),
    }
}

fn fetched_after(shown: &UsageReportEntry, arriving: &UsageReportEntry) -> bool {
    match (
        parse_timestamp(&shown.fetched_at),
        parse_timestamp(&arriving.fetched_at),
    ) {
        (Some(shown), Some(arriving)) => shown > arriving,
        _ => false,
    }
}

/// Puts a streamed report in place of its previous copy, or appends it if new. A copy fetched
/// after the streamed one, such as a card refreshed while the list streams, stays.
fn upsert_report(reports: &mut Vec<UsageReportEntry>, report: UsageReportEntry) {
    match reports.iter_mut().find(|shown| shown.id == report.id) {
        Some(shown) => {
            if !fetched_after(shown, &report) {
                *shown = report;
            }
        }
        None => reports.push(report),
    }
}

/// The list a finished request leaves: its reports, dropping any the host no longer has, except
/// that a copy on screen fetched after the request's copy stays.
fn settle_reports(
    shown: &[UsageReportEntry],
    finished: Vec<UsageReportEntry>,
) -> Vec<UsageReportEntry> {
    finished
        .into_iter()
        .map(
            |report| match shown.iter().find(|shown| shown.id == report.id) {
                Some(shown) if fetched_after(shown, &report) => shown.clone(),
                _ => report,
            },
        )
        .collect()
}

/// Swaps one report for its refreshed copy in place; `None` means the host no longer knows it.
fn replace_report(
    reports: &mut Vec<UsageReportEntry>,
    report_id: &str,
    refreshed: Option<UsageReportEntry>,
) {
    match refreshed {
        Some(refreshed) => {
            if let Some(shown) = reports.iter_mut().find(|shown| shown.id == report_id) {
                *shown = refreshed;
            }
        }
        None => reports.retain(|shown| shown.id != report_id),
    }
}

/// Whether the host can list usage at all, through usage sources or its older per-provider list.
fn supports_usage(store: &PaseoStore) -> bool {
    store.server_info.has_feature("usageSources")
        || store.server_info.has_feature("providerUsageList")
}

fn request_usage_reports(
    store: &Entity<PaseoStore>,
    request: UsageReportsRequest,
    on_report: impl FnMut(&UsageReportEntry) + Send + 'static,
    cx: &mut App,
) -> Task<Result<Vec<UsageReportEntry>>> {
    let server_info = store.read(cx).server_info.clone();
    store.update(cx, |store, cx| {
        store.session_request(cx, move |session| async move {
            session
                .usage_reports(&server_info, request, on_report)
                .await
        })
    })
}

/// Usage reports streamed in by one request, as the Usage modal and the context card show them.
#[derive(Default)]
struct StreamedReports {
    /// `None` until the first report of a load arrives.
    reports: Option<Vec<UsageReportEntry>>,
    error: Option<String>,
    loading: bool,
    load_task: Option<Task<()>>,
}

/// Loads `request` into the view's [`StreamedReports`], picked by `streamed`: each report shows
/// as it arrives, so a slow source never holds back the others, and the final list then replaces
/// them. Replies `is_current` rejects, such as from a connection since replaced, are dropped.
fn load_reports<T: 'static>(
    view: &mut T,
    store: &Entity<PaseoStore>,
    request: UsageReportsRequest,
    streamed: fn(&mut T) -> &mut StreamedReports,
    is_current: impl Fn(&T, &App) -> bool + 'static,
    on_loaded: fn(&mut T, &[UsageReportEntry], &mut Context<T>),
    cx: &mut Context<T>,
) {
    let (sender, mut receiver) = mpsc::unbounded();
    let request = request_usage_reports(
        store,
        request,
        move |report| {
            if sender.unbounded_send(report.clone()).is_err() {
                log::debug!("Paseo usage report arrived after its view closed");
            }
        },
        cx,
    );
    let state = streamed(view);
    state.loading = true;
    state.load_task = Some(cx.spawn(async move |this, cx| {
        let update = async {
            // The sender lives as long as the request, so the stream ends once it finishes.
            while let Some(report) = receiver.next().await {
                this.update(cx, |view, cx| {
                    if is_current(view, cx) {
                        upsert_report(streamed(view).reports.get_or_insert_default(), report);
                        cx.notify();
                    }
                })?;
            }
            let result = request.await;
            this.update(cx, |view, cx| {
                if !is_current(view, cx) {
                    return;
                }
                let state = streamed(view);
                state.loading = false;
                match result {
                    Ok(finished) => {
                        let shown = state.reports.take().unwrap_or_default();
                        let settled = settle_reports(&shown, finished);
                        state.error = None;
                        on_loaded(view, &settled, cx);
                        streamed(view).reports = Some(settled);
                    }
                    Err(error) => state.error = Some(error.to_string()),
                }
                cx.notify();
            })
        };
        if let Err(error) = update.await {
            log::debug!("Paseo usage view closed: {error}");
        }
    }));
    cx.notify();
}

enum CardRefresh {
    /// Held so closing the modal cancels the request.
    Pending {
        _task: Task<()>,
    },
    Failed,
}

/// One usage report as a card, in the Usage modal or (compact, without a frame) in the context
/// hover card.
fn render_usage_card(
    entry: &UsageReportEntry,
    display_as: PaseoUsageDisplay,
    compact: bool,
    refreshing: bool,
    refresh_failed: bool,
    on_refresh: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    now: DateTime<Utc>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let status = match &entry.report {
        UsageReport::Available { .. } => None,
        UsageReport::Error(_) => Some(("Error", Color::Error)),
        UsageReport::Unavailable(_) => Some(("Unavailable", Color::Muted)),
    };
    let (plan_label, windows, balances, details) = match &entry.report {
        UsageReport::Available {
            plan_label,
            windows,
            balances,
            details,
        } => (
            plan_label.as_deref(),
            windows.as_slice(),
            balances.as_slice(),
            details.as_slice(),
        ),
        UsageReport::Unavailable(_) | UsageReport::Error(_) => (None, &[][..], &[][..], &[][..]),
    };
    let messages = report_messages(entry, now);
    let freshness = updated_ago(&entry.fetched_at, now).unwrap_or_else(|| "Refresh".into());
    v_flex()
        .id(SharedString::from(format!(
            "paseo-usage-report-{}",
            entry.id
        )))
        .w_full()
        .gap_3()
        .when(!compact, |this| {
            this.p_4()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.editor_background)
        })
        .child(
            h_flex()
                .gap_2()
                .child(crate::sidebar::source_icon(
                    &entry.source_id,
                    entry.icon.as_deref(),
                    IconSize::Small,
                    cx,
                ))
                .child(Label::new(entry.source_label.clone()).truncate())
                .when_some(plan_label.map(readable_plan_name), |this, plan| {
                    this.child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .bg(colors.element_background)
                            .child(Label::new(plan).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                })
                .child(div().flex_1())
                .when_some(status, |this, (label, color)| {
                    this.child(
                        h_flex()
                            .gap_1p5()
                            .child(Indicator::dot().color(color))
                            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                    )
                })
                .child(
                    IconButton::new(
                        SharedString::from(format!("paseo-usage-refresh-{}", entry.id)),
                        IconName::RotateCw,
                    )
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(refreshing)
                    .tooltip(Tooltip::text(freshness))
                    .on_click(on_refresh),
                ),
        )
        .when(!messages.is_empty(), |this| {
            this.child(v_flex().children(messages.into_iter().map(|message| {
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Error)
            })))
        })
        .when(!windows.is_empty() || !balances.is_empty(), |this| {
            this.child(
                v_flex()
                    .gap_1()
                    .children(
                        windows
                            .iter()
                            .map(|window| render_window_row(window, display_as, now, cx)),
                    )
                    .children(balances.iter().map(|balance| {
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                Label::new(balance.label.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(Label::new(balance_text(balance)).size(LabelSize::Small))
                    })),
            )
        })
        .when(!details.is_empty(), |this| {
            this.child(v_flex().gap_1().children(details.iter().map(|detail| {
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        Label::new(detail.label.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(Label::new(detail.value.clone()).size(LabelSize::Small))
            })))
        })
        .when_some(entry.account_label.clone(), |this, account| {
            this.child(
                Label::new(account)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
        })
        .when(refresh_failed, |this| {
            this.child(
                Label::new("Unable to refresh usage")
                    .size(LabelSize::Small)
                    .color(Color::Error),
            )
        })
        .into_any_element()
}

fn render_window_row(
    window: &UsageWindow,
    display_as: PaseoUsageDisplay,
    now: DateTime<Utc>,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    let fill = match window_tone(window) {
        Tone::Default => colors.text_muted,
        Tone::Warning => status.warning,
        Tone::Danger => status.error,
    };
    let at_risk = window
        .runs_out_at
        .as_deref()
        .and_then(parse_timestamp)
        .is_some();
    let percent = displayed_percent(window, display_as).unwrap_or(0.0);
    v_flex()
        .gap(px(3.))
        .py_1()
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .child(
                    Label::new(window.label.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .child(
                            Label::new(window_value(window, display_as))
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                        )
                        .children(window_timing(window, now).map(|timing| {
                            Label::new(format!(" · {timing}"))
                                .size(LabelSize::Small)
                                .color(if at_risk { Color::Error } else { Color::Muted })
                        })),
                ),
        )
        .child(
            div()
                .h(px(4.))
                .w_full()
                .rounded_full()
                .bg(colors.element_background)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(fill)
                        .w(relative(percent as f32 / 100.0)),
                ),
        )
        .into_any_element()
}

fn set_display_as(display_as: PaseoUsageDisplay, cx: &mut App) {
    let fs = <dyn Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |settings, _| {
        settings
            .paseo
            .get_or_insert_default()
            .usage
            .get_or_insert_default()
            .display_as = Some(display_as);
    });
}

/// Paseo's Usage dialog: every usage report of one host, one card per account.
pub struct UsageModal {
    store: Entity<PaseoStore>,
    /// The store's connection generation and count when usage last loaded: a new generation is
    /// another daemon session, whose usage replaces this one's, and a new count is a reconnect,
    /// which failed the load in flight without a new generation.
    connection: (u64, u64),
    streamed: StreamedReports,
    card_refreshes: HashMap<String, CardRefresh>,
    focus_handle: FocusHandle,
    _store_subscription: Subscription,
    _settings_subscription: Subscription,
}

impl UsageModal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = crate::hosts::current_store(cx);
        let mut modal = Self {
            _store_subscription: Self::observe_store(&store, cx),
            connection: Self::connection_of(&store, cx),
            store,
            streamed: StreamedReports::default(),
            card_refreshes: HashMap::default(),
            focus_handle: cx.focus_handle(),
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        };
        modal.load(false, cx);
        window.focus(&modal.focus_handle, cx);
        modal
    }

    fn observe_store(store: &Entity<PaseoStore>, cx: &mut Context<Self>) -> Subscription {
        cx.observe(store, |modal, _, cx| modal.store_changed(cx))
    }

    fn connection_of(store: &Entity<PaseoStore>, cx: &App) -> (u64, u64) {
        let store = store.read(cx);
        (store.connection_generation, store.connection_count)
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        let connection = Self::connection_of(&self.store, cx);
        if connection == self.connection {
            return;
        }
        if connection.0 != self.connection.0 {
            self.clear();
        }
        self.connection = connection;
        // Replacing the load task cancels one still waiting on the old socket.
        self.streamed.error = None;
        self.load(false, cx);
    }

    fn clear(&mut self) {
        self.streamed = StreamedReports::default();
        self.card_refreshes.clear();
    }

    fn select_host(&mut self, store: Entity<PaseoStore>, cx: &mut Context<Self>) {
        if store == self.store {
            return;
        }
        self._store_subscription = Self::observe_store(&store, cx);
        self.connection = Self::connection_of(&store, cx);
        self.store = store;
        self.clear();
        self.load(false, cx);
    }

    /// Whether a reply started on `host` at `generation` still belongs to what the modal shows.
    fn is_current(&self, host: &Entity<PaseoStore>, generation: u64, cx: &App) -> bool {
        &self.store == host && self.store.read(cx).is_current_connection(generation)
    }

    fn load(&mut self, force_refresh: bool, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if !store.connected() || !supports_usage(store) {
            return;
        }
        let generation = store.connection_generation;
        let host = self.store.clone();
        let request = UsageReportsRequest {
            force_refresh,
            ..Default::default()
        };
        load_reports(
            self,
            &host.clone(),
            request,
            |modal| &mut modal.streamed,
            move |modal, cx| modal.is_current(&host, generation, cx),
            |_, _, _| {},
            cx,
        );
    }

    fn refresh_card(&mut self, report_id: String, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if !store.connected() {
            return;
        }
        let generation = store.connection_generation;
        let host = self.store.clone();
        let request = UsageReportsRequest {
            report_ids: Some(vec![report_id.clone()]),
            force_refresh: true,
            ..Default::default()
        };
        let task = request_usage_reports(&self.store, request, |_| {}, cx);
        let refresh = cx.spawn({
            let report_id = report_id.clone();
            async move |modal, cx| {
                let result = task.await;
                let update = modal.update(cx, |modal, cx| {
                    if !modal.is_current(&host, generation, cx) {
                        return;
                    }
                    match result {
                        Ok(reports) => {
                            let refreshed =
                                reports.into_iter().find(|report| report.id == report_id);
                            if let Some(shown) = modal.streamed.reports.as_mut() {
                                replace_report(shown, &report_id, refreshed);
                            }
                            modal.card_refreshes.remove(&report_id);
                        }
                        Err(error) => {
                            log::warn!("Paseo usage refresh failed: {error:#}");
                            modal.card_refreshes.insert(report_id, CardRefresh::Failed);
                        }
                    }
                    cx.notify();
                });
                if let Err(error) = update {
                    log::debug!("Paseo usage modal closed: {error}");
                }
            }
        });
        self.card_refreshes
            .insert(report_id, CardRefresh::Pending { _task: refresh });
        cx.notify();
    }

    /// The host whose usage shows, offered when more than one connected host can report it.
    fn render_host_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let hosts = crate::hosts::configured_hosts(cx)
            .into_iter()
            .filter(|(_, store)| {
                let store = store.read(cx);
                store.connected() && supports_usage(store)
            })
            .collect();
        let modal = cx.weak_entity();
        crate::agent_view::render_host_picker(
            "paseo-usage-host",
            "paseo-usage-host-button",
            "Host whose usage to show",
            self.store.clone(),
            hosts,
            move |store, _, cx| {
                if let Err(error) = modal.update(cx, |modal, cx| modal.select_host(store, cx)) {
                    log::debug!("Paseo usage modal closed: {error}");
                }
            },
        )
    }

    fn render_display_toggle(&self, display_as: PaseoUsageDisplay) -> AnyElement {
        let option = |id: &'static str, label: &'static str, value: PaseoUsageDisplay| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(display_as == value)
                .on_click(move |_, _, cx| set_display_as(value, cx))
        };
        h_flex()
            .gap_0p5()
            .child(option(
                "paseo-usage-display-used",
                "Used",
                PaseoUsageDisplay::Used,
            ))
            .child(option(
                "paseo-usage-display-remaining",
                "Remaining",
                PaseoUsageDisplay::Remaining,
            ))
            .into_any_element()
    }
}

impl EventEmitter<DismissEvent> for UsageModal {}
impl ModalView for UsageModal {}

impl Focusable for UsageModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UsageModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Utc::now();
        let display_as = PaseoSettings::get_global(cx).usage.display_as;
        let host = crate::hosts::host_name(&self.store, cx).unwrap_or_else(|| "this host".into());
        let (connected, supported) = {
            let store = self.store.read(cx);
            (store.connected(), supports_usage(store))
        };
        let available = connected && supported;
        let body = if !connected {
            crate::render_message(format!("Connect to {host} to see usage"), None, None)
        } else if !supported {
            crate::render_message(format!("Update {host} to see usage"), None, None)
        } else if let Some(reports) = &self.streamed.reports {
            if reports.is_empty() {
                crate::render_message("No usage data", None, None)
            } else {
                v_flex()
                    .gap_3()
                    .children(reports.iter().map(|entry| {
                        let report_id = entry.id.clone();
                        let refresh = self.card_refreshes.get(&entry.id);
                        render_usage_card(
                            entry,
                            display_as,
                            false,
                            matches!(refresh, Some(CardRefresh::Pending { .. })),
                            matches!(refresh, Some(CardRefresh::Failed)),
                            cx.listener(move |modal, _, _, cx| {
                                modal.refresh_card(report_id.clone(), cx)
                            }),
                            now,
                            cx,
                        )
                    }))
                    .into_any_element()
            }
        } else if let Some(error) = &self.streamed.error {
            crate::render_error(
                "Unable to load usage",
                error.clone(),
                Some(
                    h_flex()
                        .child(
                            Button::new("paseo-usage-retry", "Try again")
                                .style(ButtonStyle::Outlined)
                                .on_click(cx.listener(|modal, _, _, cx| modal.load(true, cx))),
                        )
                        .into_any_element(),
                ),
                cx,
            )
        } else {
            crate::render_loading("Loading usage...")
        };
        v_flex()
            .key_context("PaseoUsage PaseoView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(px(640.))
            .max_h(px(720.))
            .elevation_3(cx)
            .rounded_md()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .child(Headline::new("Usage").size(HeadlineSize::Small))
                    .child(div().flex_1())
                    .children(self.render_host_picker(cx))
                    .when(available, |this| {
                        this.child(self.render_display_toggle(display_as)).child(
                            IconButton::new("paseo-usage-refresh-all", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .disabled(self.streamed.loading)
                                .tooltip(Tooltip::text("Refresh all"))
                                .on_click(cx.listener(|modal, _, _, cx| modal.load(true, cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id("paseo-usage-body")
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .pb_4()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

/// Opens the Usage modal on the focused agent's host, or closes it if it is open.
pub fn open_usage(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.toggle_modal(window, cx, UsageModal::new);
}

/// The usage of the account one agent runs under, for the context meter's hover card. It loads
/// once when the card opens rather than polling.
pub(crate) struct AgentUsage {
    store: Entity<PaseoStore>,
    agent_id: String,
    generation: u64,
    streamed: StreamedReports,
}

impl AgentUsage {
    pub(crate) fn new(store: Entity<PaseoStore>, agent_id: String, cx: &mut Context<Self>) -> Self {
        let generation = store.read(cx).connection_generation;
        // The card is rebuilt on every hover, so it starts from the status chip's recent copy
        // when there is one instead of asking the host again each time.
        let cached = cx.try_global::<GlobalSharedUsage>().and_then(|shared| {
            let agent_reports = shared.0.read(cx).reports(&store, &agent_id)?;
            agent_reports
                .loaded_within(AGENT_USAGE_FRESH_FOR)
                .then(|| agent_reports.reports.clone())
        });
        let load = cached.is_none();
        let mut usage = Self {
            store,
            agent_id,
            generation,
            streamed: StreamedReports {
                reports: cached,
                ..StreamedReports::default()
            },
        };
        if load {
            usage.load(false, cx);
        }
        usage
    }

    /// Only hosts with usage sources know which account an agent runs under.
    pub(crate) fn supported(store: &PaseoStore) -> bool {
        store.connected() && store.server_info.has_feature("usageSources")
    }

    fn load(&mut self, force_refresh: bool, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if !Self::supported(store) || !store.is_current_connection(self.generation) {
            return;
        }
        let generation = self.generation;
        let request = UsageReportsRequest {
            agent_id: Some(self.agent_id.clone()),
            force_refresh,
            ..Default::default()
        };
        let store = self.store.clone();
        load_reports(
            self,
            &store,
            request,
            |usage| &mut usage.streamed,
            move |usage, cx| usage.store.read(cx).is_current_connection(generation),
            |usage, reports, cx| {
                if let Some(shared) = cx.try_global::<GlobalSharedUsage>() {
                    shared.0.clone().update(cx, |shared, cx| {
                        shared.record(&usage.store, &usage.agent_id, reports.to_vec(), cx)
                    });
                }
            },
            cx,
        );
    }
}

impl Render for AgentUsage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !Self::supported(self.store.read(cx)) {
            return div().into_any_element();
        }
        let divider = || ui::Divider::horizontal().color(ui::DividerColor::Border);
        let message = |text: String| {
            v_flex()
                .gap_2()
                .child(divider())
                .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        };
        // A failed load keeps whatever streamed in before it, which alone would pass for the
        // agent's complete usage.
        if let Some(error) = &self.streamed.error {
            return message(format!("Unable to load usage: {error}"));
        }
        let Some(reports) = &self.streamed.reports else {
            return message("Loading usage...".into());
        };
        if reports.is_empty() {
            return div().into_any_element();
        }
        let now = Utc::now();
        let display_as = PaseoSettings::get_global(cx).usage.display_as;
        v_flex()
            .gap_2()
            .child(divider())
            .children(reports.iter().map(|entry| {
                render_usage_card(
                    entry,
                    display_as,
                    true,
                    self.streamed.loading,
                    false,
                    // An agent's request names the agent, not report IDs, so a card refresh
                    // reloads the agent's whole account.
                    cx.listener(|usage, _, _, cx| usage.load(true, cx)),
                    now,
                    cx,
                )
            }))
            .into_any_element()
    }
}

/// Usage reports per host and agent, shared by every workspace's status item, so several
/// workspaces showing one agent poll its host once rather than once each.
struct SharedUsage {
    hosts: HashMap<EntityId, HostUsage>,
    /// The host and agent each status item shows, so usage no item shows stops polling.
    shown_by: HashMap<EntityId, (EntityId, Option<String>)>,
    _refresh_timer: Task<()>,
}

struct HostUsage {
    store: Entity<PaseoStore>,
    agents: HashMap<String, AgentReports>,
    /// The store's connection count at the last store change; a new one is a reconnect.
    connection_count: Option<u64>,
    /// How many agents ran at the last store change, as the title bar counts them; fewer now
    /// means a turn finished, which can change usage.
    running: usize,
    _store_subscription: Subscription,
}

#[derive(Default)]
struct AgentReports {
    reports: Vec<UsageReportEntry>,
    loaded_at: Option<Instant>,
    loading: bool,
}

impl AgentReports {
    fn loaded_within(&self, age: Duration) -> bool {
        self.loaded_at
            .is_some_and(|loaded_at| loaded_at.elapsed() <= age)
    }
}

fn is_shown(
    shown_by: &HashMap<EntityId, (EntityId, Option<String>)>,
    host: EntityId,
    agent: &str,
) -> bool {
    shown_by
        .values()
        .any(|(shown, shown_agent)| *shown == host && shown_agent.as_deref() == Some(agent))
}

struct GlobalSharedUsage(Entity<SharedUsage>);

impl Global for GlobalSharedUsage {}

fn shared_usage(cx: &mut App) -> Entity<SharedUsage> {
    if let Some(shared) = cx.try_global::<GlobalSharedUsage>() {
        return shared.0.clone();
    }
    let shared = cx.new(|cx| SharedUsage {
        hosts: HashMap::default(),
        shown_by: HashMap::default(),
        _refresh_timer: cx.spawn(async move |shared, cx| {
            loop {
                cx.background_executor().timer(STALE_AFTER).await;
                if shared
                    .update(cx, |shared: &mut SharedUsage, cx| shared.refresh_all(cx))
                    .is_err()
                {
                    break;
                }
            }
        }),
    });
    cx.set_global(GlobalSharedUsage(shared.clone()));
    shared
}

impl SharedUsage {
    /// Records that `item` shows `agent`'s usage on `store`'s host, starting to follow that host
    /// and agent if no item did.
    fn show(
        &mut self,
        item: EntityId,
        store: &Entity<PaseoStore>,
        agent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let host = store.entity_id();
        self.shown_by.insert(item, (host, agent.clone()));
        let new_host = !self.hosts.contains_key(&host);
        let host_usage = self.hosts.entry(host).or_insert_with(|| HostUsage {
            store: store.clone(),
            agents: HashMap::default(),
            connection_count: None,
            running: 0,
            _store_subscription: cx.observe(store, |shared, store, cx| {
                shared.store_changed(store.entity_id(), cx)
            }),
        });
        let new_agent = agent.filter(|agent| !host_usage.agents.contains_key(agent));
        if let Some(agent) = &new_agent {
            host_usage
                .agents
                .insert(agent.clone(), AgentReports::default());
        }
        self.forget_unshown();
        if new_host {
            // A first look at the host counts as a reconnect, which loads every agent shown.
            self.store_changed(host, cx);
        } else if let Some(agent) = new_agent {
            self.refresh(host, &agent, cx);
        } else if let Some((_, Some(agent))) = self.shown_by.get(&item).cloned() {
            self.refresh_if_stale(host, &agent, cx);
        }
    }

    fn forget(&mut self, item: EntityId) {
        self.shown_by.remove(&item);
        self.forget_unshown();
    }

    fn forget_unshown(&mut self) {
        let shown_by = &self.shown_by;
        self.hosts
            .retain(|host, _| shown_by.values().any(|(shown, _)| shown == host));
        // An agent no item shows keeps its reports while they are fresh, so switching back to
        // it shows usage at once instead of a blank chip until a new request answers.
        for (host, host_usage) in &mut self.hosts {
            host_usage.agents.retain(|agent, agent_reports| {
                agent_reports.loaded_within(STALE_AFTER) || is_shown(shown_by, *host, agent)
            });
        }
    }

    /// Keeps reports another view loaded for an agent on a host this cache follows, so the
    /// status chip and the next context card reuse them.
    fn record(
        &mut self,
        store: &Entity<PaseoStore>,
        agent: &str,
        reports: Vec<UsageReportEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(host_usage) = self.hosts.get_mut(&store.entity_id()) else {
            return;
        };
        let agent_reports = host_usage.agents.entry(agent.to_owned()).or_default();
        agent_reports.reports = reports;
        agent_reports.loaded_at = Some(Instant::now());
        cx.notify();
    }

    fn reports(&self, store: &Entity<PaseoStore>, agent: &str) -> Option<&AgentReports> {
        self.hosts.get(&store.entity_id())?.agents.get(agent)
    }

    fn store_changed(&mut self, host: EntityId, cx: &mut Context<Self>) {
        let Some(host_usage) = self.hosts.get_mut(&host) else {
            return;
        };
        let store = host_usage.store.read(cx);
        let connected = store.connected();
        let running = HostActivity::of_stores([store]).running;
        let reconnected = connected && host_usage.connection_count != Some(store.connection_count);
        let turn_finished = running < host_usage.running;
        if connected {
            host_usage.connection_count = Some(store.connection_count);
        }
        host_usage.running = running;
        if !connected {
            let mut cleared = false;
            for agent_reports in host_usage.agents.values_mut() {
                if !agent_reports.reports.is_empty() {
                    agent_reports.reports.clear();
                    agent_reports.loaded_at = None;
                    cleared = true;
                }
            }
            if cleared {
                cx.notify();
            }
        } else if reconnected || turn_finished {
            self.refresh_host(host, cx);
        }
    }

    fn refresh_if_stale(&mut self, host: EntityId, agent: &str, cx: &mut Context<Self>) {
        let stale = self
            .hosts
            .get(&host)
            .and_then(|host_usage| host_usage.agents.get(agent))
            .is_some_and(|agent_reports| !agent_reports.loaded_within(STALE_AFTER));
        if stale {
            self.refresh(host, agent, cx);
        }
    }

    fn refresh_all(&mut self, cx: &mut Context<Self>) {
        for host in self.hosts.keys().copied().collect::<Vec<_>>() {
            self.refresh_host(host, cx);
        }
    }

    fn refresh_host(&mut self, host: EntityId, cx: &mut Context<Self>) {
        let agents = self
            .hosts
            .get(&host)
            .map(|host_usage| host_usage.agents.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for agent in agents {
            if is_shown(&self.shown_by, host, &agent) {
                self.refresh(host, &agent, cx);
            }
        }
    }

    fn refresh(&mut self, host: EntityId, agent: &str, cx: &mut Context<Self>) {
        let Some(host_usage) = self.hosts.get_mut(&host) else {
            return;
        };
        let store_entity = host_usage.store.clone();
        let store = store_entity.read(cx);
        if !store.connected() || !supports_usage(store) {
            return;
        }
        let generation = store.connection_generation;
        let provider = store
            .agent(agent)
            .map(|agent| agent_provider(agent).to_owned());
        let Some(agent_reports) = host_usage.agents.get_mut(agent) else {
            return;
        };
        if agent_reports.loading {
            return;
        }
        agent_reports.loading = true;
        let request = UsageReportsRequest {
            agent_id: Some(agent.to_owned()),
            provider,
            ..Default::default()
        };
        let task = request_usage_reports(&store_entity, request, |_| {}, cx);
        let agent = agent.to_owned();
        cx.spawn(async move |shared, cx| {
            let result = task.await;
            shared.update(cx, |shared, cx| {
                // No item shows this host or agent any more.
                let Some(host_usage) = shared.hosts.get_mut(&host) else {
                    return;
                };
                let current = host_usage.store.read(cx).is_current_connection(generation);
                let Some(agent_reports) = host_usage.agents.get_mut(&agent) else {
                    return;
                };
                agent_reports.loading = false;
                // A reply from before a reconnect belongs to the old session; the reconnect's
                // own refresh was skipped while this one was in flight, so it runs now.
                if !current {
                    shared.refresh(host, &agent, cx);
                    return;
                }
                match result {
                    Ok(reports) => {
                        agent_reports.reports = reports;
                        agent_reports.loaded_at = Some(Instant::now());
                    }
                    Err(error) => log::warn!("Paseo usage failed: {error:#}"),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }
}

/// The current agent's account limits in the status bar, like Paseo's usage chip.
pub struct UsageStatusItem {
    store: Entity<PaseoStore>,
    active_agent_id: Option<String>,
    /// The host and agent last handed to the shared cache, so unchanged ones aren't sent again
    /// on every host change.
    shown: Option<(EntityId, Option<String>)>,
    shared: Entity<SharedUsage>,
    /// The registry relays every host's changes and focus moves, so the item can follow the
    /// agent in view to its host once that loads.
    _subscriptions: [Subscription; 4],
}

impl UsageStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = crate::hosts::default_store(cx);
        let registry = crate::hosts::registry(cx);
        let shared = shared_usage(cx);
        let subscriptions = [
            cx.observe(&registry, |item, _, cx| item.hosts_changed(cx)),
            cx.subscribe(&registry, |item, _, event, cx| {
                if matches!(event, crate::hosts::HostsEvent::FocusChanged) {
                    item.hosts_changed(cx);
                }
            }),
            cx.observe(&shared, |_, _, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        ];
        let item_id = cx.entity_id();
        cx.on_release(move |item, cx| item.shared.update(cx, |shared, _| shared.forget(item_id)))
            .detach();
        let mut item = Self {
            store,
            active_agent_id: None,
            shown: None,
            shared,
            _subscriptions: subscriptions,
        };
        item.hosts_changed(cx);
        item
    }

    fn shown_agent(&self, cx: &App) -> Option<String> {
        self.active_agent_id
            .clone()
            .or_else(|| crate::hosts::focused_agent(cx))
    }

    /// Moves to the host of the agent in view, since usage is per host and an agent on another
    /// host would otherwise show nothing.
    fn hosts_changed(&mut self, cx: &mut Context<Self>) {
        let agent_host = match self.active_agent_id.clone() {
            // Every change on any host lands here, so only look across hosts once the agent in
            // view isn't on the current one.
            Some(agent_id) if self.store.read(cx).agent(&agent_id).is_none() => {
                crate::hosts::store_for_agent(&agent_id, cx)
            }
            Some(_) => None,
            None => crate::hosts::focused_store(cx),
        };
        if let Some(store) = agent_host
            && store != self.store
        {
            self.store = store;
            cx.notify();
        }
        let shown = (self.store.entity_id(), self.shown_agent(cx));
        if self.shown.as_ref() != Some(&shown) {
            self.shown = Some(shown.clone());
            let item_id = cx.entity_id();
            let store = self.store.clone();
            self.shared
                .update(cx, |shared, cx| shared.show(item_id, &store, shown.1, cx));
            cx.notify();
        }
    }

    fn agent_report<'a>(&self, cx: &'a App) -> Option<&'a UsageReportEntry> {
        let agent_id = self.shown.as_ref()?.1.as_deref()?;
        self.shared
            .read(cx)
            .reports(&self.store, agent_id)?
            .reports
            .iter()
            .find(|report| matches!(report.report, UsageReport::Available { .. }))
    }
}

impl Render for UsageStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entry) = self.agent_report(cx) else {
            return div().into_any_element();
        };
        let UsageReport::Available {
            plan_label,
            windows,
            balances,
            details,
        } = &entry.report
        else {
            return div().into_any_element();
        };
        if windows.is_empty() && balances.is_empty() {
            return div().into_any_element();
        }
        let now = Utc::now();
        let display_as = PaseoSettings::get_global(cx).usage.display_as;
        let tooltip_lines = plan_label
            .iter()
            .map(|plan| format!("Plan: {plan}"))
            .chain(windows.iter().map(|window| {
                let timing = window_timing(window, now)
                    .map(|timing| format!(" · {timing}"))
                    .unwrap_or_default();
                format!(
                    "{}: {}{timing}",
                    window.label,
                    window_value(window, display_as)
                )
            }))
            .chain(
                balances
                    .iter()
                    .map(|balance| format!("{}: {}", balance.label, balance_text(balance))),
            )
            .chain(
                details
                    .iter()
                    .map(|detail| format!("{}: {}", detail.label, detail.value)),
            )
            .collect::<Vec<_>>()
            .join("\n");
        let title = format!("{} usage", entry.source_label);
        // Window labels such as `Weekly · Fable` contain dots, so windows are split by rules.
        let separator = || {
            ui::Divider::vertical()
                .color(ui::DividerColor::Border)
                .into_any_element()
        };
        let mut segments: Vec<AnyElement> = Vec::new();
        for window in windows {
            if !segments.is_empty() {
                segments.push(separator());
            }
            let color = match window_tone(window) {
                Tone::Default => Color::Muted,
                Tone::Warning => Color::Warning,
                Tone::Danger => Color::Error,
            };
            segments.push(
                Label::new(status_window_text(window, display_as))
                    .size(LabelSize::Small)
                    .color(color)
                    .into_any_element(),
            );
        }
        for balance in balances {
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
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Label::new(entry.source_label.clone())
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Default),
            )
            .child(separator())
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
        if let Some(agent) = agent_id.clone()
            && agent_id != self.active_agent_id
        {
            self.active_agent_id = agent_id;
            self.hosts_changed(cx);
            let host = self.store.entity_id();
            self.shared
                .update(cx, |shared, cx| shared.refresh_if_stale(host, &agent, cx));
            cx.notify();
        }
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// Turns a plan id such as `self_serve_business_prolite` into text; values that already read as
/// text (with a space or a capital letter) are kept.
fn readable_plan_name(plan: &str) -> String {
    if plan.contains(' ') || plan.chars().any(char::is_uppercase) {
        return plan.to_owned();
    }
    crate::capitalize_first(&plan.replace(['_', '-'], " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn status_item_follows_the_focused_agents_host(cx: &mut gpui::TestAppContext) {
        use settings::Settings as _;
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::PaseoSettings::register(cx);
            crate::hosts::init(cx);
        });
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.paseo = Some(settings::PaseoSettingsContent {
                        profiles: Some(
                            ["Test", "Local"]
                                .map(|name| settings::PaseoConnectionProfile {
                                    name: name.into(),
                                    target_uri: format!("ws://localhost:{}/ws", 6000 + name.len()),
                                    editor_ssh_uri: None,
                                    client_id: format!("client-{name}"),
                                })
                                .to_vec(),
                        ),
                        active_profile: Some("Test".into()),
                        ..Default::default()
                    });
                });
            });
        });
        cx.run_until_parked();
        let item = cx.new(UsageStatusItem::new);
        let local = cx.update(|cx| crate::hosts::store_named("Local", cx));
        assert!(
            cx.read(|cx| item.read(cx).store != local),
            "starts on the default host"
        );
        local.update(cx, |store, cx| {
            store.handle_event(
                paseo_client::PaseoEvent::AgentsChanged(vec![crate::store::test_agent(
                    "on-local",
                    "idle",
                    serde_json::json!({}),
                )]),
                cx,
            )
        });
        cx.update(|cx| crate::hosts::set_focused_agent("on-local".into(), cx));
        cx.run_until_parked();
        assert!(
            cx.read(|cx| item.read(cx).store == local),
            "follows the host of the agent in view"
        );
    }

    #[gpui::test]
    async fn usage_modal_opens_from_action(cx: &mut gpui::TestAppContext) {
        cx.update(crate::test_init);
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [] as [&std::path::Path; 0], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            let multi_workspace = workspace::MultiWorkspace::test_new(project, window, cx);
            multi_workspace.workspace().update(cx, |workspace, _| {
                crate::register_workspace_actions(workspace)
            });
            multi_workspace
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        cx.run_until_parked();
        cx.dispatch_action(crate::OpenProviderUsage);
        cx.run_until_parked();
        assert!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<UsageModal>(cx)
                .is_some()),
            "the action opens the Usage modal"
        );
    }

    fn report(id: &str, fetched_at: &str, label: &str) -> UsageReportEntry {
        UsageReportEntry {
            id: id.into(),
            account_label: None,
            fetched_at: fetched_at.into(),
            source_id: "claude".into(),
            source_label: label.into(),
            icon: None,
            report: UsageReport::Error("down".into()),
            login_errors: Vec::new(),
        }
    }

    #[test]
    fn entries_upsert_by_id_and_final_list_replaces() {
        let mut shown = Vec::new();
        upsert_report(&mut shown, report("b", "2026-10-07T10:00:00Z", "B"));
        upsert_report(&mut shown, report("a", "2026-10-07T10:00:00Z", "A"));
        upsert_report(&mut shown, report("b", "2026-10-07T10:05:00Z", "B newer"));
        let labels = |reports: &[UsageReportEntry]| {
            reports
                .iter()
                .map(|report| report.source_label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&shown),
            ["B newer", "A"],
            "arrival order, updated in place"
        );
        upsert_report(&mut shown, report("b", "2026-10-07T10:01:00Z", "B older"));
        assert_eq!(labels(&shown), ["B newer", "A"], "an older copy never wins");

        let settled = settle_reports(
            &shown,
            vec![
                report("a", "2026-10-07T10:02:00Z", "A final"),
                report("b", "2026-10-07T10:00:00Z", "B final"),
            ],
        );
        assert_eq!(
            labels(&settled),
            ["A final", "B newer"],
            "the final list replaces, keeping copies fetched after it"
        );
        let settled = settle_reports(&settled, vec![report("c", "2026-10-07T10:03:00Z", "C")]);
        assert_eq!(labels(&settled), ["C"], "reports the host dropped leave");
    }

    #[test]
    fn problem_text_matches_upstream() {
        let now = parse_timestamp("2026-10-07T10:00:00Z").expect("time");
        let expired = |refreshed_by: Option<&str>, expires_at: &str| UsageProblem::Expired {
            expires_at: expires_at.into(),
            refreshed_by: refreshed_by.map(str::to_owned),
        };
        assert_eq!(
            problem_text(&expired(Some("claude login"), "2026-10-05T09:00:00Z"), now),
            "Login expired 2d ago. Run claude login to refresh it."
        );
        assert_eq!(
            problem_text(&expired(None, "2026-10-07T07:30:00Z"), now),
            "Login expired 2h ago. Sign in again."
        );
        assert_eq!(
            problem_text(&expired(None, "2026-10-07T09:59:30Z"), now),
            "Login expired just now. Sign in again."
        );
        assert_eq!(
            problem_text(
                &UsageProblem::Rejected {
                    status: 401,
                    refreshed_by: None
                },
                now
            ),
            "Login rejected (HTTP 401). Sign in again."
        );
        assert_eq!(
            problem_text(
                &UsageProblem::Rejected {
                    status: 403,
                    refreshed_by: Some("codex login".into())
                },
                now
            ),
            "Login rejected (HTTP 403). Run codex login to refresh it."
        );
        assert_eq!(
            problem_text(
                &UsageProblem::NoQuota {
                    detail: "No plan limits".into()
                },
                now
            ),
            "No plan limits"
        );
    }

    #[test]
    fn window_value_used_and_remaining() {
        assert_eq!(
            window_value(&window(Some(31.0)), PaseoUsageDisplay::Used),
            "31%"
        );
        assert_eq!(
            window_value(&window(Some(31.0)), PaseoUsageDisplay::Remaining),
            "69% left"
        );
        assert_eq!(
            window_value(&window(Some(130.0)), PaseoUsageDisplay::Used),
            "100%"
        );
        assert_eq!(window_value(&window(None), PaseoUsageDisplay::Used), "—");
        assert_eq!(
            window_value(&window(None), PaseoUsageDisplay::Remaining),
            "—"
        );
    }

    #[test]
    fn status_item_label_uses_short_labels() {
        let mut short = window(Some(31.0));
        short.short_label = Some("5h".into());
        let mut percent_only = window(Some(12.0));
        percent_only.short_label = Some(String::new());
        let missing = window(Some(52.4));
        assert_eq!(
            status_window_text(&short, PaseoUsageDisplay::Used),
            "31% 5h"
        );
        assert_eq!(
            status_window_text(&percent_only, PaseoUsageDisplay::Used),
            "12%"
        );
        assert_eq!(
            status_window_text(&missing, PaseoUsageDisplay::Used),
            "52% 5-hour"
        );
        assert_eq!(
            status_window_text(&short, PaseoUsageDisplay::Remaining),
            "69% 5h",
            "the chip never says \"left\""
        );
    }

    #[test]
    fn plan_ids_read_as_text() {
        assert_eq!(
            readable_plan_name("self_serve_business_prolite"),
            "Self serve business prolite"
        );
        assert_eq!(readable_plan_name("pro-max"), "Pro max");
        assert_eq!(readable_plan_name("Team 5x"), "Team 5x");
        assert_eq!(readable_plan_name("Pro"), "Pro");
        assert_eq!(readable_plan_name(""), "");
    }

    fn window(used_percent: Option<f64>) -> UsageWindow {
        UsageWindow {
            id: "5h".into(),
            label: "5-hour".into(),
            short_label: None,
            used_percent,
            resets_at: None,
            runs_out_at: None,
            tone: None,
        }
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
