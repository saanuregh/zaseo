use anyhow::{Result, bail};
use gpui::{Context, IntoElement, Window};
use paseo_client::ConnectionTarget;
use settings::{PaseoConnectionProfile, Settings};
use ui::{Button, Label, prelude::*};
use url::Url;

use crate::{PaseoSettings, PaseoView};

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

pub(super) fn render(
    view: &mut PaseoView,
    _window: &mut Window,
    cx: &mut Context<PaseoView>,
) -> impl IntoElement {
    let settings = PaseoSettings::get_global(cx);
    let profiles = settings.profiles.clone();
    let active_profile = settings.active_profile.clone();
    let store = view.store.read(cx);
    let connected = store.connected;
    let connecting = store.connecting;
    let status = if connecting {
        "Connecting to Paseo…"
    } else if connected {
        "Connected to Paseo"
    } else {
        "Disconnected"
    };
    let warning = Url::parse(&view.target_input.read(cx).text(cx))
        .ok()
        .is_some_and(|url| {
            url.scheme() == "ws"
                && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        });

    let mut profile_list = v_flex().gap_1();
    for profile in profiles {
        let label = profile.name.clone();
        let name = profile.name.clone();
        let target_uri = profile.target_uri.clone();
        let editor_ssh_uri = profile.editor_ssh_uri.clone();
        profile_list = profile_list.child(
            Button::new(format!("paseo-profile-{label}"), label.clone()).on_click(cx.listener(
                move |view, _, window, cx| {
                    view.profile_name_input
                        .update(cx, |input, cx| input.set_text(&name, window, cx));
                    view.target_input
                        .update(cx, |input, cx| input.set_text(&target_uri, window, cx));
                    view.editor_ssh_input.update(cx, |input, cx| {
                        input.set_text(editor_ssh_uri.as_deref().unwrap_or(""), window, cx)
                    });
                },
            )),
        );
    }

    v_flex()
        .p_2()
        .gap_2()
        .child(Label::new(format!("Paseo · {status}")))
        .child(profile_list)
        .child(Label::new(format!("Profile: {active_profile}")))
        .child(view.profile_name_input.clone())
        .child(view.target_input.clone())
        .child(view.editor_ssh_input.clone())
        .child(view.password_input.clone())
        .when(warning, |this| {
            this.child(Label::new(
                "Unencrypted remote WebSocket: use a trusted network or VPN",
            ))
        })
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(
                        "paseo-connect",
                        if connected { "Reconnect" } else { "Connect" },
                    )
                    .on_click(cx.listener(|view, _, window, cx| view.connect(window, cx))),
                )
                .child(
                    Button::new("paseo-save-profile", "Save Profile")
                        .on_click(cx.listener(|view, _, _, cx| view.save_profile(cx))),
                ),
        )
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
}
