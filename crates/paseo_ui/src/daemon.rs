use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, SharedString, Subscription, Window, prelude::*, px,
};
use paseo_client::{DaemonStatus, DaemonUpdate, ProviderAvailability};
use settings::Settings as _;
use std::collections::HashMap;
use theme_settings::ThemeSettings;
use ui::{CommonAnimationExt, CopyButton, Indicator, Tooltip, prelude::*};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::store::{ConnectionStatus, PaseoStore};
use crate::timeline::{format_relative, parse_timestamp};

/// The daemon's facts in display order; absent fields are left out rather than shown blank.
pub(crate) fn daemon_status_rows(
    status: &DaemonStatus,
    now: DateTime<Utc>,
) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if let Some(version) = &status.version {
        rows.push(("Version", version.clone()));
    }
    if let Some(pid) = status.pid {
        rows.push(("PID", pid.to_string()));
    }
    if let Some(listen) = &status.listen {
        rows.push(("Listen address", listen.clone()));
    }
    if let Some(started_at) = &status.started_at {
        let started = parse_timestamp(started_at)
            .map(|timestamp| format_relative(timestamp, now))
            .unwrap_or_else(|| started_at.clone());
        rows.push(("Started", started));
    }
    if let Some(node_path) = &status.node_path {
        rows.push(("Node path", node_path.clone()));
    }
    if let Some(relay) = &status.relay {
        rows.push((
            "Relay",
            if relay.enabled {
                "Enabled".to_string()
            } else {
                "Disabled".to_string()
            },
        ));
        if let Some(endpoint) = &relay.endpoint {
            rows.push(("Relay endpoint", endpoint.clone()));
        }
        if let Some(public_endpoint) = &relay.public_endpoint {
            rows.push(("Relay public endpoint", public_endpoint.clone()));
        }
    }
    if !status.server_id.is_empty() {
        rows.push(("Server ID", status.server_id.clone()));
    }
    rows
}

enum Loadable<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(String),
}

enum DiagnosticState {
    Running,
    Finished(String),
    Failed(String),
}

enum UpdateState {
    Idle,
    Running,
    Finished(DaemonUpdate),
    Failed(String),
}

pub struct DaemonStatusView {
    store: Entity<PaseoStore>,
    focus_handle: FocusHandle,
    status: Loadable<DaemonStatus>,
    providers: Loadable<Vec<ProviderAvailability>>,
    refreshing_providers: bool,
    providers_error: Option<String>,
    diagnostics: HashMap<String, DiagnosticState>,
    restarting: bool,
    restart_error: Option<String>,
    update: UpdateState,
    /// The connection the shown status came from; a reconnect, restart, or host switch makes a
    /// new one, and the status is reloaded for it.
    loaded_for_connection: Option<u64>,
    _store_subscription: Subscription,
}

impl DaemonStatusView {
    fn new(cx: &mut Context<Self>) -> Self {
        let store = crate::store(cx);
        let store_subscription = cx.observe(&store, |view: &mut Self, store, cx| {
            let connection = store.read(cx).connection_count;
            if view.connected(cx) && view.loaded_for_connection != Some(connection) {
                view.refresh(cx);
            }
            cx.notify();
        });
        let mut view = Self {
            store,
            focus_handle: cx.focus_handle(),
            status: Loadable::Idle,
            providers: Loadable::Idle,
            refreshing_providers: false,
            providers_error: None,
            diagnostics: HashMap::new(),
            restarting: false,
            restart_error: None,
            update: UpdateState::Idle,
            loaded_for_connection: None,
            _store_subscription: store_subscription,
        };
        view.refresh(cx);
        view
    }

    fn connected(&self, cx: &App) -> bool {
        self.store.read(cx).status == ConnectionStatus::Connected
    }

    fn status_supported(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .server_info
            .has_feature("daemonStatusRpc")
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.connected(cx) {
            return;
        }
        self.loaded_for_connection = Some(self.store.read(cx).connection_count);
        if self.status_supported(cx) {
            self.load_status(cx);
        } else {
            self.load_providers(cx);
        }
    }

    fn load_status(&mut self, cx: &mut Context<Self>) {
        self.status = Loadable::Loading;
        let generation = self.store.read(cx).connection_generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move { session.daemon_status().await })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                // A reply from before a host switch or reconnect belongs to the old host.
                if !view.store.read(cx).is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(status) => {
                        view.providers = Loadable::Loaded(status.providers.clone());
                        view.status = Loadable::Loaded(status);
                    }
                    Err(error) => view.status = Loadable::Failed(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn load_providers(&mut self, cx: &mut Context<Self>) {
        self.providers = Loadable::Loading;
        let generation = self.store.read(cx).connection_generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(
                cx,
                |session| async move { session.available_providers().await },
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                // A reply from before a host switch or reconnect belongs to the old host.
                if !view.store.read(cx).is_current_connection(generation) {
                    return;
                }
                view.providers = match result {
                    Ok(providers) => Loadable::Loaded(providers),
                    Err(error) => Loadable::Failed(error.to_string()),
                };
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn refresh_providers(&mut self, cx: &mut Context<Self>) {
        self.refreshing_providers = true;
        self.providers_error = None;
        let generation = self.store.read(cx).connection_generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move {
                session.refresh_providers().await?;
                session.available_providers().await
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.refreshing_providers = false;
                if !view.store.read(cx).is_current_connection(generation) {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(providers) => view.providers = Loadable::Loaded(providers),
                    Err(error) => view.providers_error = Some(error.to_string()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn run_diagnostic(&mut self, provider: String, cx: &mut Context<Self>) {
        self.diagnostics
            .insert(provider.clone(), DiagnosticState::Running);
        let task = self.store.update(cx, |store, cx| {
            let provider = provider.clone();
            store.session_request(cx, |session| async move {
                session.provider_diagnostic(&provider).await
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                let state = match result {
                    Ok(text) => DiagnosticState::Finished(text),
                    Err(error) => DiagnosticState::Failed(error.to_string()),
                };
                view.diagnostics.insert(provider, state);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn confirm_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        crate::workspace_tools::confirm_then(
            "Restart the daemon?",
            "Agents keep running. Zaseo reconnects automatically.",
            "Restart",
            move |cx| {
                if let Err(error) = view.update(cx, |view, cx| view.restart(cx)) {
                    log::debug!("Paseo daemon tab closed: {error}");
                }
            },
            window,
            cx,
        );
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.restarting = true;
        self.restart_error = None;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move { session.restart_daemon().await })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.restarting = false;
                if let Err(error) = result {
                    view.restart_error = Some(error.to_string());
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn confirm_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        crate::workspace_tools::confirm_then(
            "Update the daemon?",
            "Updates the daemon to the latest version and restarts it.",
            "Update",
            move |cx| {
                if let Err(error) = view.update(cx, |view, cx| view.update_daemon(cx)) {
                    log::debug!("Paseo daemon tab closed: {error}");
                }
            },
            window,
            cx,
        );
    }

    fn update_daemon(&mut self, cx: &mut Context<Self>) {
        self.update = UpdateState::Running;
        let task = self.store.update(cx, |store, cx| {
            store.daemon_update_phase = None;
            store.session_request(cx, |session| async move { session.update_daemon().await })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.update = match result {
                    Ok(update) => UpdateState::Finished(update),
                    Err(error) => UpdateState::Failed(error.to_string()),
                };
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn render_message(message: impl Into<SharedString>) -> AnyElement {
        v_flex()
            .py_4()
            .child(Label::new(message.into()).color(Color::Muted))
            .into_any_element()
    }

    fn render_error(message: String) -> AnyElement {
        Label::new(message)
            .size(LabelSize::Small)
            .color(Color::Error)
            .into_any_element()
    }

    fn render_card(id: &'static str, cx: &App) -> gpui::Stateful<Div> {
        let colors = cx.theme().colors();
        v_flex()
            .id(id)
            .w_full()
            .p_4()
            .gap_3()
            .rounded(px(12.))
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
    }

    fn render_section_header(title: &'static str, button: Option<AnyElement>) -> AnyElement {
        h_flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .child(Label::new(title).weight(FontWeight::SEMIBOLD)),
            )
            .children(button)
            .into_any_element()
    }

    fn render_status(&self, now: DateTime<Utc>, cx: &mut Context<Self>) -> AnyElement {
        let loading = matches!(self.status, Loadable::Loading);
        let body =
            if !self.status_supported(cx) {
                Self::render_message("Update the host to read daemon status.")
            } else {
                match &self.status {
                    Loadable::Idle | Loadable::Loading => crate::render_loading("Loading status…"),
                    Loadable::Failed(error) => Self::render_error(error.clone()),
                    Loadable::Loaded(status) => v_flex()
                        .gap_1()
                        .children(daemon_status_rows(status, now).into_iter().map(
                            |(label, value)| {
                                h_flex()
                                    .gap_4()
                                    .justify_between()
                                    .child(Label::new(label).color(Color::Muted))
                                    .child(Label::new(value))
                            },
                        ))
                        .into_any_element(),
                }
            };
        Self::render_card("paseo-daemon-status", cx)
            .child(Self::render_section_header(
                "Status",
                Some(
                    Button::new(
                        "paseo-daemon-refresh",
                        if loading { "Refreshing…" } else { "Refresh" },
                    )
                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                    .disabled(loading)
                    .tooltip(Tooltip::text("Read the daemon status again"))
                    .on_click(cx.listener(|view, _, _, cx| view.refresh(cx)))
                    .into_any_element(),
                ),
            ))
            .child(body)
            .into_any_element()
    }

    fn render_provider(
        &self,
        index: usize,
        provider: &ProviderAvailability,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let diagnostic_background = cx.theme().colors().element_background;
        let (status_label, status_color) = if provider.available {
            ("Available", Color::Success)
        } else {
            ("Unavailable", Color::Muted)
        };
        let diagnostic = self.diagnostics.get(&provider.provider);
        let running = matches!(diagnostic, Some(DiagnosticState::Running));
        let provider_id = provider.provider.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(provider.provider.clone()))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Indicator::dot().color(status_color))
                            .child(
                                Label::new(status_label)
                                    .size(LabelSize::Small)
                                    .color(status_color),
                            ),
                    )
                    .child(div().flex_1())
                    .when(running, |this| {
                        this.child(
                            Icon::new(IconName::ArrowCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                    })
                    .child(
                        Button::new(
                            ("paseo-daemon-diagnostic", index),
                            if running {
                                "Running…"
                            } else {
                                "Run diagnostic"
                            },
                        )
                        .disabled(running)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.run_diagnostic(provider_id.clone(), cx)
                        })),
                    ),
            )
            .when_some(provider.error.clone(), |this, error| {
                this.child(Self::render_error(error))
            })
            .map(|this| match diagnostic {
                Some(DiagnosticState::Finished(text)) => this.child(
                    h_flex()
                        .items_start()
                        .gap_2()
                        .p_2()
                        .rounded_md()
                        .bg(diagnostic_background)
                        .child(
                            div()
                                .id(("paseo-daemon-diagnostic-text", index))
                                .flex_1()
                                .max_h(px(320.))
                                .overflow_y_scroll()
                                .font_buffer(cx)
                                .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
                                .whitespace_normal()
                                .child(text.clone()),
                        )
                        .child(CopyButton::new(
                            SharedString::from(format!("paseo-daemon-diagnostic-copy-{index}")),
                            text.clone(),
                        )),
                ),
                Some(DiagnosticState::Failed(error)) => {
                    this.child(Self::render_error(error.clone()))
                }
                Some(DiagnosticState::Running) | None => this,
            })
            .into_any_element()
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let refreshing = self.refreshing_providers;
        let body = match &self.providers {
            Loadable::Idle | Loadable::Loading => crate::render_loading("Loading providers…"),
            Loadable::Failed(error) => Self::render_error(error.clone()),
            Loadable::Loaded(providers) if providers.is_empty() => {
                Self::render_message("No providers reported")
            }
            Loadable::Loaded(providers) => v_flex()
                .gap_3()
                .children(
                    providers
                        .iter()
                        .enumerate()
                        .map(|(index, provider)| self.render_provider(index, provider, cx)),
                )
                .into_any_element(),
        };
        Self::render_card("paseo-daemon-providers", cx)
            .child(Self::render_section_header(
                "Providers",
                Some(
                    Button::new(
                        "paseo-daemon-refresh-providers",
                        if refreshing {
                            "Refreshing…"
                        } else {
                            "Refresh providers"
                        },
                    )
                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                    .disabled(refreshing)
                    .tooltip(Tooltip::text("Recheck providers on the host"))
                    .on_click(cx.listener(|view, _, _, cx| view.refresh_providers(cx)))
                    .into_any_element(),
                ),
            ))
            .when_some(self.providers_error.clone(), |this, error| {
                this.child(Self::render_error(error))
            })
            .child(body)
            .into_any_element()
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let phase = store.daemon_update_phase.clone();
        let desktop_managed = store.server_info.desktop_managed;
        let updating = matches!(self.update, UpdateState::Running);
        let update_status = match &self.update {
            UpdateState::Idle => None,
            UpdateState::Running => Some(
                Label::new(match phase {
                    Some(phase) => format!("Updating: {phase}"),
                    None => "Updating…".to_string(),
                })
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            ),
            UpdateState::Finished(update) => Some(
                Label::new(format!(
                    "Updated {} → {}",
                    update.previous_version.as_deref().unwrap_or("unknown"),
                    update.new_version.as_deref().unwrap_or("unknown"),
                ))
                .size(LabelSize::Small)
                .color(Color::Success)
                .into_any_element(),
            ),
            UpdateState::Failed(error) => Some(Self::render_error(error.clone())),
        };
        Self::render_card("paseo-daemon-actions", cx)
            .child(Self::render_section_header("Actions", None))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(
                            "paseo-daemon-restart",
                            if self.restarting {
                                "Restarting…"
                            } else {
                                "Restart daemon"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.restarting || updating)
                        .on_click(
                            cx.listener(|view, _, window, cx| view.confirm_restart(window, cx)),
                        ),
                    )
                    .child(
                        Button::new(
                            "paseo-daemon-update",
                            if updating {
                                "Updating…"
                            } else {
                                "Update daemon"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(updating || self.restarting || desktop_managed)
                        .on_click(
                            cx.listener(|view, _, window, cx| view.confirm_update(window, cx)),
                        ),
                    ),
            )
            .when(desktop_managed, |this| {
                this.child(
                    Label::new("Update Paseo Desktop on the host.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(self.restart_error.clone(), |this, error| {
                this.child(Self::render_error(error))
            })
            .children(update_status)
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for DaemonStatusView {}

impl Focusable for DaemonStatusView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DaemonStatusView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Utc::now();
        let host = self
            .store
            .read(cx)
            .active_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let body = if !self.connected(cx) {
            Self::render_message("Connect to this host to see the daemon")
        } else {
            v_flex()
                .gap_4()
                .child(self.render_status(now, cx))
                .child(self.render_providers(cx))
                .child(self.render_actions(cx))
                .into_any_element()
        };
        div()
            .id("paseo-daemon")
            .key_context("PaseoDaemon")
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
                            v_flex()
                                .child(Headline::new("Daemon").size(HeadlineSize::Small))
                                .child(
                                    Label::new(format!("The Paseo daemon on {host}"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(body),
                ),
            )
    }
}

impl Item for DaemonStatusView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Daemon".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Server))
    }
}

/// Opens the Daemon tab for the connected host, reusing an open one.
pub(crate) fn open_daemon_status(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<DaemonStatusView>(cx).next();
    if let Some(existing) = existing {
        existing.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let view = cx.new(DaemonStatusView::new);
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_client::RelayStatus;

    fn status(relay: Option<RelayStatus>) -> DaemonStatus {
        DaemonStatus {
            server_id: "server-1".into(),
            version: Some("1.2.3".into()),
            pid: None,
            node_path: None,
            started_at: None,
            listen: Some("127.0.0.1:6767".into()),
            relay,
            providers: Vec::new(),
        }
    }

    #[test]
    fn daemon_status_rows_show_version_listen_and_relay() {
        let now = Utc::now();
        let enabled = status(Some(RelayStatus {
            enabled: true,
            endpoint: Some("relay.example".into()),
            public_endpoint: None,
        }));
        assert_eq!(
            daemon_status_rows(&enabled, now),
            vec![
                ("Version", "1.2.3".to_string()),
                ("Listen address", "127.0.0.1:6767".to_string()),
                ("Relay", "Enabled".to_string()),
                ("Relay endpoint", "relay.example".to_string()),
                ("Server ID", "server-1".to_string()),
            ]
        );

        let disabled = status(Some(RelayStatus {
            enabled: false,
            endpoint: None,
            public_endpoint: None,
        }));
        let rows = daemon_status_rows(&disabled, now);
        assert!(rows.contains(&("Relay", "Disabled".to_string())));
        assert!(!rows.iter().any(|(label, _)| *label == "PID"));

        let no_relay = status(None);
        assert!(
            !daemon_status_rows(&no_relay, now)
                .iter()
                .any(|(label, _)| label.starts_with("Relay"))
        );
    }
}
