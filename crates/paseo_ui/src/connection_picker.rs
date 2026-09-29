use anyhow::{Result, anyhow, bail};
use fs::Fs;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, TaskExt, Window, prelude::*, px,
};
use menu::{Cancel, Confirm};
use paseo_client::ConnectionTarget;
use settings::{PaseoConnectionProfile, Settings};
use std::sync::Arc;
use ui::{Divider, Tooltip, prelude::*};
use ui_input::InputField;
use url::Url;
use workspace::{ModalView, Workspace};

use crate::{PaseoSettings, client_id_for, store};

pub(super) fn parse_target(profile: &PaseoConnectionProfile) -> Result<ConnectionTarget> {
    if profile.client_id.trim().is_empty() {
        bail!("Paseo client ID is required");
    }
    if profile.target_uri.starts_with("ssh://") {
        if profile.editor_ssh_uri.is_some() {
            bail!("SSH Paseo profiles use their target for the editor connection");
        }
        return paseo_client::parse_ssh_uri(&profile.target_uri);
    }
    let url = Url::parse(&profile.target_uri)?;
    if !matches!(url.scheme(), "ws" | "wss")
        || url.host_str().is_none()
        || url.port_or_known_default().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/ws"
    {
        bail!("Paseo URL must be ws:// or wss:// with a host and /ws path, without credentials");
    }
    if let Some(editor_ssh) = &profile.editor_ssh_uri {
        paseo_client::parse_ssh_uri(editor_ssh)?;
    }
    Ok(ConnectionTarget::Direct {
        websocket_url: profile.target_uri.clone(),
        editor_ssh: profile.editor_ssh_uri.clone(),
    })
}

fn is_unencrypted_remote(target_uri: &str) -> bool {
    Url::parse(target_uri).ok().is_some_and(|url| {
        url.scheme() == "ws" && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    })
}

pub(crate) fn open_hosts(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.project().read(cx).fs().clone();
    workspace.toggle_modal(window, cx, |window, cx| HostsModal::new(fs, window, cx));
}

pub(crate) fn open_rename(
    workspace: &mut Workspace,
    agent_id: String,
    current: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    crate::workspace_tools::open_text_prompt(
        workspace,
        "Rename agent",
        "Agent name",
        &current,
        move |name, _, cx| {
            let agent_id = agent_id.clone();
            store(cx).update(cx, |store, cx| store.rename(&agent_id, name, cx));
        },
        window,
        cx,
    );
}

pub struct HostsModal {
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    name: Entity<InputField>,
    target: Entity<InputField>,
    editor_ssh: Entity<InputField>,
    password: Entity<InputField>,
    editing: Option<String>,
    error: Option<String>,
}

impl HostsModal {
    fn new(fs: Arc<dyn Fs>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputField::new(window, cx, "Local").label("Name"));
        let target = cx.new(|cx| {
            InputField::new(window, cx, "ws://127.0.0.1:6767/ws or ssh://user@host")
                .label("Daemon address")
        });
        let editor_ssh = cx.new(|cx| {
            InputField::new(window, cx, "ssh://user@host (optional)").label("Editor SSH mapping")
        });
        let password = cx.new(|cx| {
            InputField::new(window, cx, "Only kept for this session")
                .label("Password")
                .masked(true)
        });
        let mut modal = Self {
            fs,
            focus_handle: cx.focus_handle(),
            name,
            target,
            editor_ssh,
            password,
            editing: None,
            error: None,
        };
        let active = PaseoSettings::get_global(cx).active().cloned();
        if let Some(profile) = active {
            modal.load(&profile, window, cx);
        }
        let handle = modal.name.focus_handle(cx);
        window.focus(&handle, cx);
        modal
    }

    fn load(
        &mut self,
        profile: &PaseoConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing = Some(profile.name.clone());
        self.name
            .update(cx, |input, cx| input.set_text(&profile.name, window, cx));
        self.target.update(cx, |input, cx| {
            input.set_text(&profile.target_uri, window, cx)
        });
        self.editor_ssh.update(cx, |input, cx| {
            input.set_text(profile.editor_ssh_uri.as_deref().unwrap_or(""), window, cx)
        });
        self.password
            .update(cx, |input, cx| input.clear(window, cx));
        self.error = None;
        cx.notify();
    }

    fn new_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        for input in [&self.name, &self.target, &self.editor_ssh, &self.password] {
            input.update(cx, |input, cx| input.clear(window, cx));
        }
        self.error = None;
        let handle = self.name.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn profile_from_inputs(&self, cx: &App) -> Result<PaseoConnectionProfile> {
        let name = self.name.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            return Err(anyhow!("Name is required"));
        }
        let target_uri = self.target.read(cx).text(cx).trim().to_owned();
        let editor_ssh_uri = self.editor_ssh.read(cx).text(cx).trim().to_owned();
        let existing = PaseoSettings::get_global(cx)
            .profiles
            .iter()
            .find(|profile| profile.name == name)
            .cloned();
        let profile = PaseoConnectionProfile {
            name,
            target_uri,
            editor_ssh_uri: (!editor_ssh_uri.is_empty()).then_some(editor_ssh_uri),
            client_id: existing
                .map(|profile| profile.client_id)
                .filter(|client_id| !client_id.is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        };
        parse_target(&profile)?;
        Ok(profile)
    }

    fn persist(
        &self,
        profile: PaseoConnectionProfile,
        replaced: Option<String>,
        cx: &App,
    ) -> futures::channel::oneshot::Receiver<Result<()>> {
        settings::update_settings_file_with_completion(self.fs.clone(), cx, move |settings, _| {
            let paseo = settings.paseo.get_or_insert_default();
            let profiles = paseo.profiles.get_or_insert_default();
            if let Some(replaced) = replaced.filter(|replaced| *replaced != profile.name) {
                profiles.retain(|existing| existing.name != replaced);
            }
            if let Some(existing) = profiles
                .iter_mut()
                .find(|existing| existing.name == profile.name)
            {
                *existing = profile.clone();
            } else {
                profiles.push(profile.clone());
            }
            paseo.active_profile = Some(profile.name);
        })
    }

    fn save(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let profile = match self.profile_from_inputs(cx) {
            Ok(profile) => profile,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let password = self.password.read(cx).text(cx);
        self.password
            .update(cx, |input, cx| input.clear(window, cx));
        let completion = self.persist(profile.clone(), self.editing.clone(), cx);
        if connect {
            let store = store(cx);
            let mut connection_profile = profile.clone();
            connection_profile.client_id = client_id_for(&connection_profile, cx);
            store.update(cx, |store, cx| {
                let generation = store.begin_connection(connection_profile.clone());
                store.connect(connection_profile, Some(password), generation, cx);
            });
        }
        self.editing = Some(profile.name);
        cx.spawn(async move |this, cx| {
            let error = match completion.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            this.update(cx, |modal, cx| {
                modal.error = error.clone();
                if error.is_none() && connect {
                    cx.emit(DismissEvent);
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.editing.clone() else {
            return;
        };
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let paseo = settings.paseo.get_or_insert_default();
            if let Some(profiles) = paseo.profiles.as_mut() {
                profiles.retain(|profile| profile.name != name);
            }
            if paseo.active_profile.as_deref() == Some(name.as_str()) {
                paseo.active_profile = None;
            }
        });
        self.new_host(window, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.save(true, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for HostsModal {}
impl ModalView for HostsModal {}

impl Focusable for HostsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HostsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let settings = PaseoSettings::get_global(cx);
        let profiles = settings.profiles.clone();
        let active = store(cx)
            .read(cx)
            .active_profile
            .as_ref()
            .map(|profile| profile.name.clone());
        let warning = is_unencrypted_remote(&self.target.read(cx).text(cx));
        v_flex()
            .key_context("PaseoHosts")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(px(560.))
            .elevation_3(cx)
            .rounded_lg()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .child(Headline::new("Paseo Hosts").size(HeadlineSize::Small))
                    .child(
                        ui::Button::new("paseo-new-host", "New Host")
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .on_click(cx.listener(|modal, _, window, cx| modal.new_host(window, cx))),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .items_start()
                    .child(
                        v_flex()
                            .w(px(180.))
                            .flex_none()
                            .p_2()
                            .gap_0p5()
                            .border_r_1()
                            .border_color(colors.border_variant)
                            .children(profiles.into_iter().enumerate().map(|(index, profile)| {
                                let selected = self.editing.as_deref() == Some(profile.name.as_str());
                                let connected = active.as_deref() == Some(profile.name.as_str());
                                let target = profile.target_uri.clone();
                                let name = profile.name.clone();
                                h_flex()
                                    .id(("paseo-host", index))
                                    .px_2()
                                    .py_1()
                                    .gap_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(selected, |this| this.bg(colors.element_selected))
                                    .hover(|style| style.bg(colors.ghost_element_hover))
                                    .tooltip(Tooltip::text(target))
                                    .on_click(cx.listener(move |modal, _, window, cx| {
                                        modal.load(&profile, window, cx)
                                    }))
                                    .child(
                                        Icon::new(IconName::Server)
                                            .size(IconSize::Small)
                                            .color(if connected { Color::Success } else { Color::Muted }),
                                    )
                                    .child(
                                        Label::new(name)
                                        .truncate(),
                                    )
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .p_4()
                            .gap_3()
                            .child(self.name.clone())
                            .child(self.target.clone())
                            .child(self.editor_ssh.clone())
                            .child(self.password.clone())
                            .when(warning, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1p5()
                                        .child(
                                            Icon::new(IconName::Warning)
                                                .size(IconSize::XSmall)
                                                .color(Color::Warning),
                                        )
                                        .child(
                                            Label::new(
                                                "Unencrypted remote WebSocket: use a trusted network or VPN",
                                            )
                                            .size(LabelSize::Small)
                                            .color(Color::Warning),
                                        ),
                                )
                            })
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    Label::new(error)
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                )
                            }),
                    ),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .child(
                        ui::Button::new("paseo-delete-host", "Delete")
                            .label_size(LabelSize::Small)
                            .color(Color::Error)
                            .disabled(self.editing.is_none())
                            .on_click(cx.listener(|modal, _, window, cx| modal.delete(window, cx))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                ui::Button::new("paseo-save-host", "Save")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|modal, _, window, cx| {
                                        modal.save(false, window, cx)
                                    })),
                            )
                            .child(
                                ui::Button::new("paseo-connect-host", "Connect")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .key_binding(
                                        ui::KeyBinding::for_action_in(&Confirm, &self.focus_handle, cx)
                                            .size(rems_from_px(11_f32)),
                                    )
                                    .on_click(cx.listener(|modal, _, window, cx| {
                                        modal.save(true, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parser_rejects_embedded_credentials() {
        let profile = PaseoConnectionProfile {
            name: "Bad".into(),
            target_uri: "ws://user:secret@example.com/ws".into(),
            editor_ssh_uri: None,
            client_id: "client".into(),
        };
        assert!(parse_target(&profile).is_err());
    }

    #[test]
    fn unencrypted_remote_warning_ignores_loopback() {
        assert!(is_unencrypted_remote("ws://10.0.0.2:6767/ws"));
        assert!(!is_unencrypted_remote("ws://127.0.0.1:6767/ws"));
        assert!(!is_unencrypted_remote("wss://host.example/ws"));
    }
}
