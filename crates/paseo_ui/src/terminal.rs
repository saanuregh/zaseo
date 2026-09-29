use anyhow::Result;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    SharedString, Subscription, Task, TaskExt, Window, prelude::*, px,
};
use paseo_client::{PaseoEvent, TerminalInfo, is_absolute_workspace_path};
use settings::Settings as _;
use terminal::{
    DisplayOnlyDelegate, Terminal, TerminalBuilder, terminal_settings::TerminalSettings,
};
use terminal_view::TerminalView;
use ui::{Tooltip, prelude::*};
use util::paths::PathStyle;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::store::{ConnectionStatus, PaseoStore, StoreEvent};

/// Resets the screen before a restore, which redraws everything the daemon still shows.
const RESET_SCREEN: &[u8] = b"\x1bc";

enum TerminalMessage {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

fn path_style(directory: &str) -> PathStyle {
    if directory.starts_with('/') || !is_absolute_workspace_path(directory) {
        PathStyle::Unix
    } else {
        PathStyle::Windows
    }
}

fn dimension(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX).max(1)
}

pub(crate) fn terminal_title(info: &TerminalInfo) -> String {
    info.title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            if info.name.is_empty() {
                "Terminal".into()
            } else {
                info.name.clone()
            }
        })
}

/// A terminal whose shell runs in the Paseo daemon, so it works for remote hosts too. Zed's
/// terminal renders it; keystrokes and size changes go back to the daemon.
pub struct PaseoTerminal {
    store: Entity<PaseoStore>,
    terminal_id: String,
    directory: String,
    title: SharedString,
    view: Entity<TerminalView>,
    terminal: Entity<Terminal>,
    exited: bool,
    /// The connection this tab last subscribed on; a failed subscribe is not retried until the
    /// next connection.
    subscribed_connection: Option<u64>,
    subscription_id: Option<String>,
    _forward_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PaseoTerminal {
    fn new(
        info: TerminalInfo,
        directory: String,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = crate::store(cx);
        let settings = TerminalSettings::get_global(cx).clone();
        let builder = TerminalBuilder::new_display_only(
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            cx.entity_id().as_u64(),
            cx.background_executor(),
            path_style(&directory),
        );
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let (sender, receiver) = async_channel::unbounded();
        terminal.update(cx, |terminal, _| {
            let input = sender.clone();
            terminal.set_display_only_delegate(DisplayOnlyDelegate {
                input: Box::new(move |bytes| {
                    if input
                        .try_send(TerminalMessage::Input(bytes.to_vec()))
                        .is_err()
                    {
                        log::debug!("Paseo terminal closed before input was sent");
                    }
                }),
                resize: Box::new(move |bounds| {
                    let message = TerminalMessage::Resize {
                        rows: dimension(bounds.num_lines()),
                        cols: dimension(bounds.num_columns()),
                    };
                    if sender.try_send(message).is_err() {
                        log::debug!("Paseo terminal closed before resize was sent");
                    }
                }),
            });
        });
        let weak_workspace = workspace.weak_handle();
        let workspace_id = workspace.database_id();
        let project = workspace.project().downgrade();
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                weak_workspace,
                workspace_id,
                project,
                window,
                cx,
            )
        });
        let forward_task = cx.spawn({
            let terminal_id = info.id.clone();
            let store = store.clone();
            async move |_, cx| {
                while let Ok(message) = receiver.recv().await {
                    let terminal_id = terminal_id.clone();
                    let task = store.update(cx, |store, cx| {
                        store.session_request(cx, move |session| async move {
                            match message {
                                TerminalMessage::Input(bytes) => {
                                    session
                                        .terminal_input(
                                            &terminal_id,
                                            String::from_utf8_lossy(&bytes).into_owned(),
                                        )
                                        .await
                                }
                                TerminalMessage::Resize { rows, cols } => {
                                    session.resize_terminal(&terminal_id, rows, cols).await
                                }
                            }
                        })
                    });
                    if let Err(error) = task.await {
                        log::debug!("Paseo terminal message not delivered: {error:#}");
                    }
                }
            }
        });
        let subscriptions = vec![
            cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
                this.handle_store_event(event, cx)
            }),
            cx.observe(&store, |this: &mut Self, _, cx| {
                this.subscribe_if_needed(cx)
            }),
        ];
        cx.on_release(|this: &mut Self, cx| {
            let store = this.store.read(cx);
            if store.status != ConnectionStatus::Connected
                || this.subscribed_connection != Some(store.connection_count)
            {
                return;
            }
            let terminal_id = this.terminal_id.clone();
            let subscription_id = this.subscription_id.take();
            this.store.update(cx, |store, cx| {
                store
                    .session_request(cx, move |session| async move {
                        session
                            .release_terminal(&terminal_id, subscription_id.as_deref())
                            .await
                    })
                    .detach_and_log_err(cx);
            });
        })
        .detach();
        let mut this = Self {
            store,
            terminal_id: info.id.clone(),
            directory,
            title: terminal_title(&info).into(),
            view,
            terminal,
            exited: false,
            subscribed_connection: None,
            subscription_id: None,
            _forward_task: forward_task,
            _subscriptions: subscriptions,
        };
        this.subscribe_if_needed(cx);
        this
    }

    pub(crate) fn terminal_id(&self) -> &str {
        &self.terminal_id
    }

    /// Subscribes once per connection, so a reconnect restores the screen and resumes output.
    fn subscribe_if_needed(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let connection = store.connection_count;
        if store.status != ConnectionStatus::Connected
            || self.subscribed_connection == Some(connection)
            || self.exited
        {
            if let Some(info) = store
                .terminals
                .get(&self.directory)
                .and_then(|terminals| terminals.iter().find(|info| info.id == self.terminal_id))
            {
                let title = SharedString::from(terminal_title(info));
                if title != self.title {
                    self.title = title;
                    cx.emit(ItemEvent::UpdateTab);
                }
            }
            return;
        }
        self.subscribed_connection = Some(connection);
        self.subscription_id = None;
        let bounds = self.terminal.read(cx).last_content.terminal_bounds;
        let (rows, cols) = (
            dimension(bounds.num_lines()),
            dimension(bounds.num_columns()),
        );
        let terminal_id = self.terminal_id.clone();
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.subscribe_terminal(&terminal_id, rows, cols).await
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(subscription_id) => this.subscription_id = subscription_id,
                Err(error) => this.terminal.update(cx, |terminal, cx| {
                    terminal.write_output(format!("\n[{error}]\n").as_bytes(), cx);
                }),
            })
        })
        .detach_and_log_err(cx);
    }

    fn handle_store_event(&mut self, event: &StoreEvent, cx: &mut Context<Self>) {
        let StoreEvent::Stream(event) = event else {
            return;
        };
        match event {
            PaseoEvent::TerminalOutput {
                terminal_id,
                bytes,
                restore,
            } if *terminal_id == self.terminal_id => {
                self.terminal.update(cx, |terminal, cx| {
                    if *restore {
                        terminal.write_raw_output(RESET_SCREEN, cx);
                    }
                    terminal.write_raw_output(bytes, cx);
                });
            }
            PaseoEvent::TerminalExited { terminal_id, error }
                if *terminal_id == self.terminal_id =>
            {
                self.exited = error.is_none();
                let message = match error {
                    Some(error) => format!("\n[Terminal stream stopped: {error}]\n"),
                    None => "\n[Process exited]\n".into(),
                };
                self.terminal.update(cx, |terminal, cx| {
                    terminal.write_output(message.as_bytes(), cx);
                });
                cx.notify();
            }
            _ => {}
        }
    }

    fn kill(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let terminal_id = self.terminal_id.clone();
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.kill_terminal(&terminal_id).await
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => cx.emit(ItemEvent::CloseItem),
                Err(error) => this.terminal.update(cx, |terminal, cx| {
                    terminal.write_output(format!("\n[{error}]\n").as_bytes(), cx);
                }),
            })
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<ItemEvent> for PaseoTerminal {}

impl Focusable for PaseoTerminal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for PaseoTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(28.))
                    .px_2()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .bg(colors.tab_bar_background)
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(self.directory.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .when(self.exited, |this| {
                        this.child(
                            Label::new("Exited")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(!self.exited, |this| {
                        this.child(
                            Button::new("paseo-terminal-kill", "Kill")
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text(
                                    "Stop the terminal's process on the Paseo host",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| this.kill(window, cx))),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}

impl Item for PaseoTerminal {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Terminal))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("Paseo terminal · {}", self.directory).into())
    }
}

/// Opens a daemon terminal's tab, reusing one already open.
pub fn open_terminal(
    workspace: &mut Workspace,
    info: TerminalInfo,
    directory: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<PaseoTerminal>(cx)
        .find(|tab| tab.read(cx).terminal_id() == info.id);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let tab = cx.new(|cx| PaseoTerminal::new(info, directory, workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
}

/// Starts a shell in `directory` on the Paseo host and opens it.
pub fn new_terminal(directory: String, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = crate::store(cx);
    let task: Task<Result<TerminalInfo>> = store.update(cx, |store, cx| {
        let directory = directory.clone();
        store.session_request(cx, move |session| async move {
            session.create_terminal(&directory, 24, 80).await
        })
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let result = task.await;
        workspace.update_in(cx, |workspace, window, cx| match result {
            Ok(info) => open_terminal(workspace, info, directory, window, cx),
            Err(error) => workspace.show_error(error, cx),
        })
    })
    .detach_and_log_err(cx);
}

/// The agent's directory, where its new terminals start.
pub(crate) fn agent_directory(agent_id: Option<String>, cx: &App) -> Option<String> {
    Some(
        crate::store(cx)
            .read(cx)
            .timeline_directory(&agent_id?)?
            .to_str()?
            .to_owned(),
    )
}

pub(crate) fn terminals_for(directory: &str, cx: &App) -> Vec<TerminalInfo> {
    crate::store(cx)
        .read(cx)
        .terminals
        .get(directory)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_fall_back_to_name() {
        let info = TerminalInfo {
            id: "t".into(),
            name: "zsh".into(),
            title: Some(" ".into()),
            cwd: None,
        };
        assert_eq!(terminal_title(&info), "zsh");
        assert_eq!(path_style("C:\\work"), PathStyle::Windows);
        assert_eq!(path_style("/work"), PathStyle::Unix);
    }
}
