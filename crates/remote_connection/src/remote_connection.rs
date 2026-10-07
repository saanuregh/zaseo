use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use askpass::EncryptedPassword;
use auto_update::AutoUpdater;
use futures::{FutureExt as _, channel::oneshot, select};
use gpui::{
    AnyWindowHandle, App, AsyncApp, DismissEvent, Entity, EventEmitter, Focusable, FontFeatures,
    ParentElement as _, Render, SharedString, Task, TextStyleRefinement, WeakEntity,
};
use http_client::{AsyncBody, HttpClient};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use release_channel::ReleaseChannel;
use remote::{ConnectionIdentifier, RemoteClient, RemoteConnectionOptions, RemotePlatform};
use semver::Version;
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme, CommonAnimationExt, Context, InteractiveElement, KeyBinding, ListItem, Tooltip,
    prelude::*,
};
use ui_input::{ERASED_EDITOR_FACTORY, ErasedEditor};
use workspace::{DismissDecision, ModalView, Workspace};

pub struct RemoteConnectionPrompt {
    connection_string: SharedString,
    nickname: Option<SharedString>,
    is_wsl: bool,
    is_devcontainer: bool,
    status_message: Option<SharedString>,
    prompt: Option<(Entity<Markdown>, oneshot::Sender<EncryptedPassword>)>,
    prompt_cancellation_task: Option<Task<()>>,
    cancellation: Option<oneshot::Sender<()>>,
    editor: Arc<dyn ErasedEditor>,
    is_password_prompt: bool,
    is_masked: bool,
}

impl Drop for RemoteConnectionPrompt {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancellation.take() {
            log::debug!("cancelling remote connection");
            cancel.send(()).ok();
        }
    }
}

pub struct RemoteConnectionModal {
    pub prompt: Entity<RemoteConnectionPrompt>,
    paths: Vec<PathBuf>,
    finished: bool,
}

impl RemoteConnectionPrompt {
    pub fn new(
        connection_string: String,
        nickname: Option<String>,
        is_wsl: bool,
        is_devcontainer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor_factory = ERASED_EDITOR_FACTORY
            .get()
            .expect("ErasedEditorFactory to be initialized");
        let editor = (editor_factory)(window, cx);

        Self {
            connection_string: connection_string.into(),
            nickname: nickname.map(|nickname| nickname.into()),
            is_wsl,
            is_devcontainer,
            editor,
            status_message: None,
            cancellation: None,
            prompt: None,
            prompt_cancellation_task: None,
            is_password_prompt: false,
            is_masked: true,
        }
    }

    pub fn set_cancellation_tx(&mut self, tx: oneshot::Sender<()>) {
        self.cancellation = Some(tx);
    }

    pub fn set_prompt(
        &mut self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_yes_no = prompt.contains("yes/no");
        self.is_password_prompt = !is_yes_no;
        self.is_masked = !is_yes_no;
        self.editor.set_masked(self.is_masked, window, cx);

        let markdown = cx.new(|cx| Markdown::new_text(prompt.into(), cx));
        self.prompt = Some((markdown, tx));
        self.prompt_cancellation_task = Some(cx.spawn(async move |this, cx| {
            cancellation.await.ok();
            this.update(cx, |this, cx| {
                this.prompt.take();
                cx.notify();
            })
            .ok();
        }));
        self.status_message.take();
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn set_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.status_message = status.map(|s| s.into());
        cx.notify();
    }

    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, tx)) = self.prompt.take() {
            self.prompt_cancellation_task.take();
            self.status_message = Some("Connecting".into());

            let pw = self.editor.text(cx);
            if let Ok(secure) = EncryptedPassword::try_from(pw.as_ref()) {
                tx.send(secure).ok();
            }
            self.editor.clear(window, cx);
        }
    }
}

impl Render for RemoteConnectionPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeSettings::get_global(cx);

        let mut text_style = window.text_style();
        let refinement = TextStyleRefinement {
            font_family: Some(theme.buffer_font.family.clone()),
            font_features: Some(FontFeatures::disable_ligatures()),
            font_size: Some(theme.buffer_font_size(cx).into()),
            color: Some(cx.theme().colors().editor_foreground),
            background_color: Some(gpui::transparent_black()),
            ..Default::default()
        };

        text_style.refine(&refinement);
        let markdown_style = MarkdownStyle {
            base_text_style: text_style,
            selection_background_color: cx.theme().colors().element_selection_background,
            ..Default::default()
        };

        let is_password_prompt = self.is_password_prompt;
        let is_masked = self.is_masked;
        let (masked_password_icon, masked_password_tooltip) = if is_masked {
            (IconName::Eye, "Toggle to Unmask Password")
        } else {
            (IconName::EyeOff, "Toggle to Mask Password")
        };

        v_flex()
            .key_context("PasswordPrompt")
            .p_2()
            .size_full()
            .when_some(self.prompt.as_ref(), |this, prompt| {
                this.child(
                    v_flex()
                        .text_sm()
                        .size_full()
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .child(MarkdownElement::new(prompt.0.clone(), markdown_style))
                                .when(is_password_prompt, |this| {
                                    this.child(
                                        IconButton::new("toggle_mask", masked_password_icon)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text(masked_password_tooltip))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.is_masked = !this.is_masked;
                                                this.editor.set_masked(this.is_masked, window, cx);
                                                window.focus(&this.editor.focus_handle(cx), cx);
                                                cx.notify();
                                            })),
                                    )
                                }),
                        )
                        .child(div().flex_1().child(self.editor.render(window, cx))),
                )
                .when(window.capslock().on, |this| {
                    this.child(
                        h_flex()
                            .py_0p5()
                            .min_w_0()
                            .w_full()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("Caps lock is on.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                })
            })
            .when_some(self.status_message.clone(), |this, status_message| {
                this.child(
                    h_flex()
                        .min_w_0()
                        .w_full()
                        .mt_1()
                        .gap_1()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                                .with_rotate_animation(2),
                        )
                        .child(
                            Label::new(format!("{}…", status_message))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate()
                                .flex_1(),
                        ),
                )
            })
    }
}

impl RemoteConnectionModal {
    pub fn new(
        connection_options: &RemoteConnectionOptions,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (connection_string, nickname, is_wsl, is_devcontainer) = match connection_options {
            RemoteConnectionOptions::Ssh(options) => (
                options.connection_string(),
                options.nickname.clone(),
                false,
                false,
            ),
            RemoteConnectionOptions::Wsl(options) => {
                (options.distro_name.clone(), None, true, false)
            }
            RemoteConnectionOptions::Docker(options) => (options.name.clone(), None, false, true),
            #[cfg(feature = "test-support")]
            RemoteConnectionOptions::Mock(options) => {
                (format!("mock-{}", options.id), None, false, false)
            }
        };
        Self {
            prompt: cx.new(|cx| {
                RemoteConnectionPrompt::new(
                    connection_string,
                    nickname,
                    is_wsl,
                    is_devcontainer,
                    window,
                    cx,
                )
            }),
            finished: false,
            paths,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt
            .update(cx, |prompt, cx| prompt.confirm(window, cx))
    }

    pub fn finished(&mut self, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tx) = self
            .prompt
            .update(cx, |prompt, _cx| prompt.cancellation.take())
        {
            log::debug!("cancelling remote connection");
            tx.send(()).ok();
        }
        self.finished(cx);
    }
}

pub struct SshConnectionHeader {
    pub connection_string: SharedString,
    pub paths: Vec<PathBuf>,
    pub nickname: Option<SharedString>,
    pub is_wsl: bool,
    pub is_devcontainer: bool,
}

impl RenderOnce for SshConnectionHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();

        let mut header_color = theme.colors().text;
        header_color.fade_out(0.96);

        let (main_label, meta_label) = if let Some(nickname) = self.nickname {
            (nickname, Some(format!("({})", self.connection_string)))
        } else {
            (self.connection_string, None)
        };

        let icon = if self.is_wsl {
            IconName::Linux
        } else if self.is_devcontainer {
            IconName::Box
        } else {
            IconName::Server
        };

        h_flex()
            .px(DynamicSpacing::Base12.rems(cx))
            .pt(DynamicSpacing::Base08.rems(cx))
            .pb(DynamicSpacing::Base04.rems(cx))
            .rounded_t_sm()
            .w_full()
            .gap_1p5()
            .child(Icon::new(icon).size(IconSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .max_w_96()
                            .overflow_x_hidden()
                            .text_ellipsis()
                            .child(Headline::new(main_label).size(HeadlineSize::XSmall)),
                    )
                    .children(
                        meta_label.map(|label| {
                            Label::new(label).color(Color::Muted).size(LabelSize::Small)
                        }),
                    )
                    .child(div().overflow_x_hidden().text_ellipsis().children(
                        self.paths.into_iter().map(|path| {
                            Label::new(path.to_string_lossy().into_owned())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        }),
                    )),
            )
    }
}

impl Render for RemoteConnectionModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl ui::IntoElement {
        let nickname = self.prompt.read(cx).nickname.clone();
        let connection_string = self.prompt.read(cx).connection_string.clone();
        let is_wsl = self.prompt.read(cx).is_wsl;
        let is_devcontainer = self.prompt.read(cx).is_devcontainer;

        let theme = cx.theme().clone();
        let body_color = theme.colors().editor_background;

        v_flex()
            .elevation_3(cx)
            .w(rems(34.))
            .border_1()
            .border_color(theme.colors().border)
            .key_context("SshConnectionModal")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm))
            .child(
                SshConnectionHeader {
                    paths: self.paths.clone(),
                    connection_string,
                    nickname,
                    is_wsl,
                    is_devcontainer,
                }
                .render(window, cx),
            )
            .child(
                div()
                    .w_full()
                    .bg(body_color)
                    .border_y_1()
                    .border_color(theme.colors().border_variant)
                    .child(self.prompt.clone()),
            )
            .child(
                div().w_full().py_1().child(
                    ListItem::new("li-devcontainer-go-back")
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(Icon::new(IconName::Close).color(Color::Muted))
                        .child(Label::new("Cancel"))
                        .end_slot(
                            KeyBinding::for_action_in(&menu::Cancel, &self.focus_handle(cx), cx)
                                .size(rems_from_px(12_f32)),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss(&menu::Cancel, window, cx);
                        })),
                ),
            )
    }
}

impl Focusable for RemoteConnectionModal {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.prompt.read(cx).editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RemoteConnectionModal {}

impl ModalView for RemoteConnectionModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(self.finished)
    }

    fn fade_out_background(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct RemoteClientDelegate {
    window: AnyWindowHandle,
    ui: WeakEntity<RemoteConnectionPrompt>,
    known_password: Option<EncryptedPassword>,
}

impl RemoteClientDelegate {
    pub fn new(
        window: AnyWindowHandle,
        ui: WeakEntity<RemoteConnectionPrompt>,
        known_password: Option<EncryptedPassword>,
    ) -> Self {
        Self {
            window,
            ui,
            known_password,
        }
    }
}

impl remote::RemoteClientDelegate for RemoteClientDelegate {
    fn ask_password(
        &self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cancellation: oneshot::Receiver<()>,
        cx: &mut AsyncApp,
    ) {
        let mut known_password = self.known_password.clone();
        if let Some(password) = known_password.take() {
            tx.send(password).ok();
        } else {
            self.window
                .update(cx, |_, window, cx| {
                    self.ui.update(cx, |modal, cx| {
                        modal.set_prompt(prompt, tx, cancellation, window, cx);
                    })
                })
                .ok();
        }
    }

    fn set_status(&self, status: Option<&str>, cx: &mut AsyncApp) {
        self.update_status(status, cx)
    }

    fn download_server_binary_locally(
        &self,
        platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<anyhow::Result<PathBuf>> {
        self.set_status(Some("Downloading remote server"), cx);
        let http_client = cx.update(|cx| cx.http_client());
        cx.background_spawn(download_upstream_remote_server(
            paths::remote_servers_dir().clone(),
            platform,
            http_client,
        ))
    }

    fn get_download_url(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        cx.spawn(async move |cx| {
            AutoUpdater::get_remote_server_release_url(
                release_channel,
                version,
                platform.os.as_str(),
                platform.arch.as_str(),
                cx,
            )
            .await
        })
    }
}

impl RemoteClientDelegate {
    fn update_status(&self, status: Option<&str>, cx: &mut AsyncApp) {
        cx.update(|cx| {
            self.ui
                .update(cx, |modal, cx| {
                    modal.set_status(status.map(|s| s.to_string()), cx);
                })
                .ok()
        });
    }
}

/// Shows a [`RemoteConnectionModal`] on the given workspace and establishes
/// a remote connection. This is a convenience wrapper around
/// [`RemoteConnectionModal`] and [`connect`] suitable for use as the
/// `connect_remote` callback in [`MultiWorkspace::find_or_create_workspace`].
///
/// When the global connection pool already has a live connection for the
/// given options, the modal is skipped entirely and the connection is
/// reused silently.
pub fn connect_with_modal(
    workspace: &Entity<Workspace>,
    connection_options: RemoteConnectionOptions,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<Option<Entity<RemoteClient>>>> {
    if remote::has_active_connection(&connection_options, cx) {
        return connect_reusing_pool(connection_options, cx);
    }

    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
        });
        let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) else {
            return Task::ready(Err(anyhow::anyhow!(
                "Failed to open remote connection dialog"
            )));
        };
        let prompt = modal.read(cx).prompt.clone();
        connect(
            ConnectionIdentifier::setup(),
            connection_options,
            prompt,
            window,
            cx,
        )
    })
}

/// Dismisses any active [`RemoteConnectionModal`] on the given workspace.
///
/// This should be called after a remote connection attempt completes
/// (success or failure) when the modal was shown on a workspace that may
/// outlive the connection flow — for example, when the modal is shown
/// on a local workspace before switching to a newly-created remote
/// workspace.
pub fn dismiss_connection_modal(workspace: &Entity<Workspace>, cx: &mut gpui::AsyncWindowContext) {
    workspace
        .update_in(cx, |workspace, _window, cx| {
            if let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                modal.update(cx, |modal, cx| modal.finished(cx));
            }
        })
        .ok();
}

/// Creates a [`RemoteClient`] by reusing an existing connection from the
/// global pool. No interactive UI is shown. This should only be called
/// when [`remote::has_active_connection`] returns `true`.
pub fn connect_reusing_pool(
    connection_options: RemoteConnectionOptions,
    cx: &mut App,
) -> Task<Result<Option<Entity<RemoteClient>>>> {
    let delegate: Arc<dyn remote::RemoteClientDelegate> = Arc::new(BackgroundRemoteClientDelegate);

    cx.spawn(async move |cx| {
        let connection = remote::connect(connection_options, delegate.clone(), cx).await?;

        let (_cancel_guard, cancel_rx) = oneshot::channel::<()>();
        cx.update(|cx| {
            RemoteClient::new(
                ConnectionIdentifier::setup(),
                connection,
                cancel_rx,
                delegate,
                cx,
            )
        })
        .await
    })
}

/// Delegate for remote connections that reuse an existing pooled
/// connection. Password prompts are not expected (the SSH transport
/// is already established), but server binary downloads are supported
/// via [`AutoUpdater`].
struct BackgroundRemoteClientDelegate;

impl remote::RemoteClientDelegate for BackgroundRemoteClientDelegate {
    fn ask_password(
        &self,
        prompt: String,
        _tx: oneshot::Sender<EncryptedPassword>,
        _cancellation: oneshot::Receiver<()>,
        _cx: &mut AsyncApp,
    ) {
        log::warn!(
            "Pooled remote connection unexpectedly requires a password \
             (prompt: {prompt})"
        );
    }

    fn set_status(&self, _status: Option<&str>, _cx: &mut AsyncApp) {}

    fn download_server_binary_locally(
        &self,
        platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<anyhow::Result<PathBuf>> {
        let http_client = cx.update(|cx| cx.http_client());
        cx.background_spawn(download_upstream_remote_server(
            paths::remote_servers_dir().clone(),
            platform,
            http_client,
        ))
    }

    fn get_download_url(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        cx.spawn(async move |cx| {
            AutoUpdater::get_remote_server_release_url(
                release_channel,
                version,
                platform.os.as_str(),
                platform.arch.as_str(),
                cx,
            )
            .await
        })
    }
}

fn upstream_remote_server_cache_path(root: &Path, platform: RemotePlatform) -> PathBuf {
    let extension = if platform.os.is_windows() {
        "zip"
    } else {
        "gz"
    };
    root.join("upstream")
        .join(remote::UPSTREAM_REMOTE_SERVER_TAG)
        .join(format!(
            "{}-{}.{extension}",
            platform.os.as_str(),
            platform.arch.as_str()
        ))
}

/// Downloads the pinned upstream server into the cache under `root`, reusing a finished download.
async fn download_upstream_remote_server(
    root: PathBuf,
    platform: RemotePlatform,
    http_client: Arc<dyn HttpClient>,
) -> Result<PathBuf> {
    let path = upstream_remote_server_cache_path(&root, platform);
    if smol::fs::metadata(&path).await.is_ok() {
        return Ok(path);
    }

    let url = remote::upstream_remote_server_url(platform);
    let directory = path
        .parent()
        .context("remote server cache path has no parent directory")?;
    smol::fs::create_dir_all(directory)
        .await
        .with_context(|| format!("creating {}", directory.display()))?;

    let mut response = http_client
        .get(&url, AsyncBody::default(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download {url}: {}",
        response.status()
    );

    // Download beside the final path so the rename is atomic and a partial file is never reused.
    // Two hosts can fetch the same platform at once, so each download gets its own file.
    static DOWNLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let download = DOWNLOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = path.with_extension(format!("download-{}-{download}", std::process::id()));
    let result = async {
        let mut file = smol::fs::File::create(&temp_path)
            .await
            .with_context(|| format!("creating {}", temp_path.display()))?;
        smol::io::copy(response.body_mut(), &mut file)
            .await
            .with_context(|| format!("downloading {url}"))?;
        smol::fs::rename(&temp_path, &path)
            .await
            .with_context(|| format!("moving {} into place", temp_path.display()))
    }
    .await;
    if let Err(error) = result {
        if let Err(remove_error) = smol::fs::remove_file(&temp_path).await {
            log::debug!(
                "Could not remove the partial download {}: {remove_error}",
                temp_path.display()
            );
        }
        return Err(error);
    }
    Ok(path)
}

pub fn connect(
    unique_identifier: ConnectionIdentifier,
    connection_options: RemoteConnectionOptions,
    ui: Entity<RemoteConnectionPrompt>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<Option<Entity<RemoteClient>>>> {
    let window = window.window_handle();
    let known_password = match &connection_options {
        RemoteConnectionOptions::Ssh(ssh_connection_options) => ssh_connection_options
            .password
            .as_deref()
            .and_then(|pw| pw.try_into().ok()),
        _ => None,
    };
    let (tx, mut rx) = oneshot::channel();
    ui.update(cx, |ui, _cx| ui.set_cancellation_tx(tx));

    let delegate = Arc::new(RemoteClientDelegate {
        window,
        ui: ui.downgrade(),
        known_password,
    });

    cx.spawn(async move |cx| {
        let connection = remote::connect(connection_options, delegate.clone(), cx);
        let connection = select! {
            _ = rx => return Ok(None),
            result = connection.fuse() => result,
        }?;

        cx.update(|cx| remote::RemoteClient::new(unique_identifier, connection, rx, delegate, cx))
            .await
    })
}

use anyhow::Context as _;

#[cfg(test)]
mod tests {
    use editor::Editor;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    use super::*;

    #[test]
    fn upstream_remote_server_cache_path_layout() {
        let root = Path::new("/cache");
        let path = upstream_remote_server_cache_path(
            root,
            RemotePlatform {
                os: remote::RemoteOs::Linux,
                arch: remote::RemoteArch::Aarch64,
            },
        );
        assert_eq!(
            path,
            root.join("upstream")
                .join(remote::UPSTREAM_REMOTE_SERVER_TAG)
                .join("linux-aarch64.gz"),
        );

        let path = upstream_remote_server_cache_path(
            root,
            RemotePlatform {
                os: remote::RemoteOs::Windows,
                arch: remote::RemoteArch::X86_64,
            },
        );
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("windows-x86_64.zip"),
        );
    }

    #[test]
    fn upstream_remote_server_downloads_once_into_the_cache() {
        let platform = RemotePlatform {
            os: remote::RemoteOs::Linux,
            arch: remote::RemoteArch::X86_64,
        };
        let expected_url = remote::upstream_remote_server_url(platform);
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client = http_client::FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                let expected_url = expected_url.clone();
                async move {
                    requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let status = if request.uri().to_string() == expected_url {
                        200
                    } else {
                        404
                    };
                    Ok(http_client::Response::builder()
                        .status(status)
                        .body(AsyncBody::from("server bytes".to_owned()))?)
                }
            }
        });
        let root = tempfile::tempdir().unwrap();

        let path = smol::block_on(download_upstream_remote_server(
            root.path().to_path_buf(),
            platform,
            client.clone(),
        ))
        .unwrap();
        assert_eq!(
            path,
            upstream_remote_server_cache_path(root.path(), platform)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "server bytes");

        // A finished download is reused without asking GitHub again.
        let again = smol::block_on(download_upstream_remote_server(
            root.path().to_path_buf(),
            platform,
            client,
        ))
        .unwrap();
        assert_eq!(again, path);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn upstream_remote_server_download_failure_names_the_url() {
        let platform = RemotePlatform {
            os: remote::RemoteOs::MacOs,
            arch: remote::RemoteArch::Aarch64,
        };
        let root = tempfile::tempdir().unwrap();
        let error = smol::block_on(download_upstream_remote_server(
            root.path().to_path_buf(),
            platform,
            http_client::FakeHttpClient::with_404_response(),
        ))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&remote::upstream_remote_server_url(platform)),
            "{error}"
        );
        assert!(!upstream_remote_server_cache_path(root.path(), platform).exists());
    }

    #[gpui::test]
    fn clears_prompt_when_password_request_is_cancelled(cx: &mut TestAppContext) {
        initialize_test(cx);

        let window = cx.add_window(|window, cx| Editor::single_line(window, cx));
        let prompt = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    RemoteConnectionPrompt::new(
                        "example.com".to_string(),
                        None,
                        false,
                        false,
                        window,
                        cx,
                    )
                })
            })
            .expect("test window should remain open");

        let (response_sender, _response_receiver) = oneshot::channel();
        let (cancellation_sender, cancellation) = oneshot::channel();
        window
            .update(cx, |_, window, cx| {
                prompt.update(cx, |prompt, cx| {
                    prompt.set_prompt(
                        "Confirm user presence".to_string(),
                        response_sender,
                        cancellation,
                        window,
                        cx,
                    );
                });
            })
            .expect("test window should remain open");

        drop(cancellation_sender);
        cx.run_until_parked();

        assert!(prompt.read_with(cx, |prompt, _| prompt.prompt.is_none()));
    }

    #[gpui::test]
    fn stale_cancellation_does_not_clear_replacement_prompt(cx: &mut TestAppContext) {
        initialize_test(cx);

        let window = cx.add_window(|window, cx| Editor::single_line(window, cx));
        let prompt = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    RemoteConnectionPrompt::new(
                        "example.com".to_string(),
                        None,
                        false,
                        false,
                        window,
                        cx,
                    )
                })
            })
            .expect("test window should remain open");

        let (first_response_sender, _first_response_receiver) = oneshot::channel();
        let (first_cancellation_sender, first_cancellation) = oneshot::channel();
        let (second_response_sender, _second_response_receiver) = oneshot::channel();
        let (_second_cancellation_sender, second_cancellation) = oneshot::channel();
        window
            .update(cx, |_, window, cx| {
                prompt.update(cx, |prompt, cx| {
                    prompt.set_prompt(
                        "First prompt".to_string(),
                        first_response_sender,
                        first_cancellation,
                        window,
                        cx,
                    );
                    prompt.set_prompt(
                        "Second prompt".to_string(),
                        second_response_sender,
                        second_cancellation,
                        window,
                        cx,
                    );
                });
            })
            .expect("test window should remain open");

        drop(first_cancellation_sender);
        cx.run_until_parked();

        assert!(prompt.read_with(cx, |prompt, _| prompt.prompt.is_some()));
    }

    fn initialize_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }
}
