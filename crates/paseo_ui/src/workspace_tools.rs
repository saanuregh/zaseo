use std::{path::PathBuf, rc::Rc};

use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, PromptLevel, Subscription, WeakEntity, Window, prelude::*, px,
};
use menu::{Cancel, Confirm};
use paseo_client::{AgentSummary, WorkspaceDescriptor, WorkspaceLabel};
use ui::{ContextMenu, IconPosition, PopoverMenu, Tooltip, prelude::*};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

use crate::{
    InterruptAgent,
    agent_view::{checkout_label, checkout_picker},
    command_center::{BaseBranchPicker, DirectoryPicker, recent_directories},
    composer::{Composer, ComposerEvent},
    sidebar::project_label,
    store::{self, PaseoStore},
    terminal, worktrees,
};

/// Paseo's label colors, in the order new labels take them.
const LABEL_COLORS: [&str; 10] = [
    "violet", "sky", "emerald", "orange", "pink", "indigo", "teal", "red", "amber", "blue",
];

/// A one-line text prompt, such as a rename. Enter confirms and Escape cancels.
pub(crate) struct TextPromptModal {
    input: Entity<InputField>,
    on_confirm: Rc<dyn Fn(String, &mut Window, &mut App)>,
    focus_handle: FocusHandle,
}

impl TextPromptModal {
    fn new(
        title: &str,
        placeholder: &str,
        initial: &str,
        on_confirm: Rc<dyn Fn(String, &mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputField::new(window, cx, placeholder).label(title.to_owned()));
        input.update(cx, |input, cx| input.set_text(initial, window, cx));
        let handle = input.focus_handle(cx);
        window.focus(&handle, cx);
        Self {
            input,
            on_confirm,
            focus_handle: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        cx.emit(DismissEvent);
        (self.on_confirm)(text, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for TextPromptModal {}
impl ModalView for TextPromptModal {}

impl Focusable for TextPromptModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextPromptModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PaseoRename")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(px(420.))
            .p_4()
            .gap_2()
            .elevation_3(cx)
            .rounded_md()
            .child(self.input.clone())
            .child(
                Label::new("Enter to save · Esc to cancel")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

pub(crate) fn open_text_prompt(
    workspace: &mut Workspace,
    title: &str,
    placeholder: &str,
    initial: &str,
    on_confirm: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let on_confirm: Rc<dyn Fn(String, &mut Window, &mut App)> = Rc::new(on_confirm);
    let (title, placeholder, initial) =
        (title.to_owned(), placeholder.to_owned(), initial.to_owned());
    workspace.toggle_modal(window, cx, move |window, cx| {
        TextPromptModal::new(&title, &placeholder, &initial, on_confirm, window, cx)
    });
}

/// Opens a text prompt from a menu, after the menu's own update finishes.
fn prompt_from_menu(
    workspace: WeakEntity<Workspace>,
    title: &'static str,
    placeholder: &'static str,
    initial: String,
    on_confirm: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
        open_text_prompt(
            workspace,
            title,
            placeholder,
            &initial,
            on_confirm,
            window,
            cx,
        )
    });
}

/// Asks before a destructive action and runs it only when the first answer is chosen.
pub(crate) fn confirm_then(
    title: &str,
    detail: &str,
    confirm_label: &str,
    action: impl FnOnce(&mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let answer = window.prompt(
        PromptLevel::Warning,
        title,
        Some(detail),
        &[confirm_label, "Cancel"],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(0) {
            cx.update(action);
        }
    })
    .detach();
}

fn workspace_agents<'a>(
    agents: &'a [AgentSummary],
    workspace_id: &'a str,
) -> impl Iterator<Item = &'a AgentSummary> {
    agents
        .iter()
        .filter(move |agent| store::agent_workspace_id(agent) == Some(workspace_id))
}

/// Whether "Mark as Read" has something to clear: the daemon clears attention except on agents
/// waiting for a permission, which only answering clears.
pub(crate) fn has_clearable_attention(agents: &[AgentSummary], workspace_id: &str) -> bool {
    workspace_agents(agents, workspace_id).any(|agent| {
        store::agent_requires_attention(agent)
            && store::agent_string(agent, "attentionReason") != Some("permission")
    })
}

/// The color a new label takes: the first one no label uses yet, else the next in turn.
fn next_label_color(labels: &[WorkspaceLabel]) -> &'static str {
    LABEL_COLORS
        .iter()
        .find(|color| !labels.iter().any(|label| label.color == **color))
        .copied()
        .unwrap_or(LABEL_COLORS[labels.len() % LABEL_COLORS.len()])
}

fn archive_detail(workspace: &WorkspaceDescriptor, agent_count: usize) -> String {
    let agents = match agent_count {
        0 => "It has no agents.".to_owned(),
        1 => "Its agent is archived.".to_owned(),
        count => format!("Its {count} agents are archived."),
    };
    if workspace.is_worktree() {
        format!(
            "{agents} Once no other workspace uses it, its worktree folder {} is removed from disk, including uncommitted and untracked changes. The branch is kept.",
            workspace.directory.display()
        )
    } else {
        format!("{agents} Files on disk are not changed.")
    }
}

/// Runs a daemon request on `host`, whose failure shows in the Paseo error banner. `None` is a
/// workspace or project no host knows any more, which the banner says too.
fn request_on<F, Fut>(host: Option<Entity<PaseoStore>>, cx: &mut App, make_request: F)
where
    F: FnOnce(std::sync::Arc<paseo_client::PaseoSession>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let Some(host) = host else {
        crate::hosts::default_store(cx).update(cx, |store, cx| {
            store.state.error =
                Some("This workspace's host isn't connected any more, so nothing changed".into());
            cx.notify();
        });
        return;
    };
    host.update(cx, |store, cx| {
        store.request_reporting_errors(cx, make_request)
    });
}

/// Sets a workspace's title; an empty name restores the default.
fn rename_workspace(workspace_id: String, name: &str, cx: &mut App) {
    let title = Some(name.trim().to_owned()).filter(|name| !name.is_empty());
    request_on(
        crate::hosts::store_for_workspace(&workspace_id, cx),
        cx,
        move |session| async move {
            session
                .set_workspace_title(&workspace_id, title.as_deref())
                .await
        },
    );
}

pub(crate) fn open_workspace_rename(
    workspace: &mut Workspace,
    descriptor: &WorkspaceDescriptor,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_id = descriptor.id.clone();
    open_text_prompt(
        workspace,
        "Rename workspace",
        "Workspace name (empty uses the default)",
        descriptor.title.as_deref().unwrap_or(&descriptor.name),
        move |name, _, cx| rename_workspace(workspace_id.clone(), &name, cx),
        window,
        cx,
    );
}

/// Renames the title an agent shows: its workspace's name when it is alone in one, otherwise its
/// own title.
pub(crate) fn open_agent_rename(
    workspace: &mut Workspace,
    agent_id: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(host) = crate::hosts::store_for_agent(&agent_id, cx) else {
        return;
    };
    let (lone_workspace, current) = {
        let store = host.read(cx);
        let Some(agent) = store.agent(&agent_id) else {
            return;
        };
        (
            store.lone_agent_workspace(agent).cloned(),
            store::agent_title(agent),
        )
    };
    if let Some(descriptor) = lone_workspace {
        open_workspace_rename(workspace, &descriptor, window, cx);
        return;
    }
    open_text_prompt(
        workspace,
        "Rename agent",
        "Agent name",
        &current,
        move |name, _, cx| {
            let agent_id = agent_id.clone();
            host.update(cx, |store, cx| store.rename(&agent_id, name, cx));
        },
        window,
        cx,
    );
}

/// The workspace row's right-click menu, like Paseo's sidebar workspace menu.
pub(crate) fn workspace_menu(
    menu: ContextMenu,
    workspace_id: &str,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> ContextMenu {
    let Some(host) = crate::hosts::store_for_workspace(workspace_id, cx) else {
        return menu.label("This workspace is gone");
    };
    let store = host.read(cx);
    let Some(descriptor) = store.state.workspaces.get(workspace_id).cloned() else {
        return menu.label("This workspace is gone");
    };
    let agent_count = workspace_agents(store.state.agents(), workspace_id).count();
    let clearable = has_clearable_attention(store.state.agents(), workspace_id);
    let labels = store.state.labels.clone();
    let is_local = store.is_local_host();
    let setup = store.state.setup.get(workspace_id).cloned();
    let id = descriptor.id.clone();

    let new_agent = (workspace.clone(), descriptor.id.clone());
    let rename = (
        workspace.clone(),
        id.clone(),
        descriptor.title.clone().unwrap_or(descriptor.name.clone()),
    );
    let pinned = descriptor.pinned_at.is_some();
    let copy_path = descriptor.directory.display().to_string();
    let branch = descriptor.current_branch.clone();
    let reveal_path = descriptor.directory.clone();
    let archive = (id.clone(), archive_detail(&descriptor, agent_count));
    let label_descriptor = descriptor.clone();
    let label_workspace = workspace;
    let scripts = descriptor.scripts;
    let scripts_id = id.clone();

    let menu = menu
        .entry("New Agent Here", None, move |window, cx| {
            let (workspace, paseo_workspace_id) = new_agent.clone();
            crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
                crate::new_agent_in_paseo_workspace(workspace, &paseo_workspace_id, window, cx)
                    .detach_and_log_err(cx);
            });
        })
        .separator()
        .entry("Rename…", None, move |window, cx| {
            let (workspace, workspace_id, current) = rename.clone();
            prompt_from_menu(
                workspace,
                "Rename workspace",
                "Workspace name (empty uses the default)",
                current,
                move |name, _, cx| rename_workspace(workspace_id.clone(), &name, cx),
                window,
                cx,
            );
        });
    let mut menu = add_attention_and_pin_entries(menu, &id, clearable, pinned).submenu(
        "Labels",
        move |menu, _, _| {
            labels_submenu(
                menu,
                &label_descriptor,
                &labels,
                label_workspace.clone(),
                host.clone(),
            )
        },
    );
    if !scripts.is_empty()
        || setup
            .as_ref()
            .is_some_and(|setup| setup_can_run(&setup.status))
    {
        menu = menu.submenu("Scripts", move |menu, _, _| {
            scripts_submenu(menu, &scripts_id, &scripts, setup.as_ref())
        });
    }
    add_copy_and_archive_entries(
        menu,
        copy_path,
        branch,
        is_local.then_some(reveal_path),
        archive,
    )
}

/// "Mark as Read" or "Mark as Unread", and pinning, for a workspace's menu.
fn add_attention_and_pin_entries(
    menu: ContextMenu,
    workspace_id: &str,
    clearable: bool,
    pinned: bool,
) -> ContextMenu {
    let attention_id = workspace_id.to_owned();
    let pin_id = workspace_id.to_owned();
    let menu = if clearable {
        menu.entry("Mark as Read", None, move |_, cx| {
            let workspace_id = attention_id.clone();
            request_on(
                crate::hosts::store_for_workspace(&workspace_id, cx),
                cx,
                move |session| async move {
                    session.clear_workspace_attention(vec![workspace_id]).await
                },
            );
        })
    } else {
        menu.entry("Mark as Unread", None, move |_, cx| {
            let workspace_id = attention_id.clone();
            request_on(
                crate::hosts::store_for_workspace(&workspace_id, cx),
                cx,
                move |session| async move { session.mark_workspace_unread(&workspace_id).await },
            );
        })
    };
    menu.entry(
        if pinned { "Unpin" } else { "Pin to Top" },
        None,
        move |_, cx| {
            let workspace_id = pin_id.clone();
            request_on(
                crate::hosts::store_for_workspace(&workspace_id, cx),
                cx,
                move |session| async move {
                    session.set_workspace_pinned(&workspace_id, !pinned).await
                },
            );
        },
    )
}

/// The copy entries, revealing a local folder, and archiving, which end a workspace's menu.
/// `archive` is the workspace's ID and the confirmation's detail.
fn add_copy_and_archive_entries(
    menu: ContextMenu,
    copy_path: String,
    branch: Option<String>,
    reveal_path: Option<PathBuf>,
    archive: (String, String),
) -> ContextMenu {
    let mut menu = menu.separator().entry("Copy Path", None, move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
    });
    if let Some(branch) = branch {
        menu = menu.entry("Copy Branch Name", None, move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
        });
    }
    if let Some(reveal_path) = reveal_path {
        menu = menu.entry(
            ui::utils::reveal_in_file_manager_label(false),
            None,
            move |_, cx| {
                cx.reveal_path(&reveal_path);
            },
        );
    }
    menu.separator()
        .entry("Archive Workspace…", None, move |window, cx| {
            let (workspace_id, detail) = archive.clone();
            confirm_then(
                "Archive this workspace?",
                &detail,
                "Archive",
                move |cx| {
                    request_on(crate::hosts::store_for_workspace(&workspace_id, cx), cx, move |session| async move {
                        session.archive_workspace(&workspace_id).await
                    })
                },
                window,
                cx,
            );
        })
}

fn labels_submenu(
    mut menu: ContextMenu,
    workspace: &WorkspaceDescriptor,
    labels: &[WorkspaceLabel],
    workspace_handle: WeakEntity<Workspace>,
    host: Entity<PaseoStore>,
) -> ContextMenu {
    for label in labels {
        let assigned = workspace.labels.contains(&label.name);
        let workspace_id = workspace.id.clone();
        let label = label.clone();
        menu = menu.toggleable_entry(
            label.name.clone(),
            assigned,
            ui::IconPosition::End,
            None,
            move |_, cx| {
                let (workspace_id, label) = (workspace_id.clone(), label.clone());
                request_on(
                    crate::hosts::store_for_workspace(&workspace_id, cx),
                    cx,
                    move |session| async move {
                        session
                            .set_workspace_label(&workspace_id, &label, !assigned)
                            .await
                            .map(drop)
                    },
                );
            },
        );
    }
    if !labels.is_empty() {
        menu = menu.separator();
    }
    let workspace_id = workspace.id.clone();
    let color = next_label_color(labels);
    let new_label_workspace = workspace_handle.clone();
    let manage_labels = labels.to_vec();
    menu.entry("New Label…", None, move |window, cx| {
        let workspace_id = workspace_id.clone();
        prompt_from_menu(
            new_label_workspace.clone(),
            "New label",
            "Label name",
            String::new(),
            move |name, _, cx| {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let workspace_id = workspace_id.clone();
                let label = WorkspaceLabel {
                    name,
                    color: color.to_owned(),
                };
                request_on(
                    crate::hosts::store_for_workspace(&workspace_id, cx),
                    cx,
                    move |session| async move {
                        session
                            .set_workspace_label(&workspace_id, &label, true)
                            .await
                            .map(drop)
                    },
                );
            },
            window,
            cx,
        );
    })
    .when(!manage_labels.is_empty(), |menu| {
        menu.submenu("Manage Labels", move |menu, _, _| {
            manage_labels_submenu(menu, &manage_labels, workspace_handle.clone(), host.clone())
        })
    })
}

fn manage_labels_submenu(
    mut menu: ContextMenu,
    labels: &[WorkspaceLabel],
    workspace: WeakEntity<Workspace>,
    host: Entity<PaseoStore>,
) -> ContextMenu {
    for label in labels {
        let label = label.clone();
        let workspace = workspace.clone();
        let host = host.clone();
        menu = menu.submenu(label.name.clone(), move |mut menu, _, _| {
            let rename = (workspace.clone(), label.name.clone());
            let rename_host = host.clone();
            menu = menu.entry("Rename…", None, move |window, cx| {
                let (workspace, name) = rename.clone();
                let host = rename_host.clone();
                prompt_from_menu(
                    workspace,
                    "Rename label",
                    "Label name",
                    name.clone(),
                    move |new_name, _, cx| {
                        let new_name = new_name.trim().to_owned();
                        if new_name.is_empty() || new_name == name {
                            return;
                        }
                        let name = name.clone();
                        request_on(Some(host.clone()), cx, move |session| async move {
                            session
                                .update_label(&name, Some(&new_name), None)
                                .await
                                .map(drop)
                        });
                    },
                    window,
                    cx,
                );
            });
            let color_name = label.name.clone();
            let current_color = label.color.clone();
            let color_host = host.clone();
            menu = menu.submenu("Color", move |mut menu, _, _| {
                for color in LABEL_COLORS {
                    let name = color_name.clone();
                    let host = color_host.clone();
                    menu = menu.toggleable_entry(
                        color,
                        current_color == color,
                        ui::IconPosition::End,
                        None,
                        move |_, cx| {
                            let name = name.clone();
                            request_on(Some(host.clone()), cx, move |session| async move {
                                session
                                    .update_label(&name, None, Some(color))
                                    .await
                                    .map(drop)
                            });
                        },
                    );
                }
                menu
            });
            let delete_name = label.name.clone();
            let delete_host = host.clone();
            menu.separator().entry("Delete…", None, move |window, cx| {
                let name = delete_name.clone();
                let host = delete_host.clone();
                let usage = host.update(cx, |store, cx| {
                    let name = name.clone();
                    store.session_request(cx, move |session| async move {
                        session.label_usage(&name).await
                    })
                });
                let window_handle = window.window_handle();
                cx.spawn(async move |cx| {
                    let detail = match usage.await {
                        Ok(1) => "It is removed from 1 workspace.".to_owned(),
                        Ok(count) => format!("It is removed from {count} workspaces."),
                        Err(error) => format!("Paseo could not count its workspaces: {error}"),
                    };
                    window_handle.update(cx, |_, window, cx| {
                        confirm_then(
                            &format!("Delete the label “{name}”?"),
                            &detail,
                            "Delete",
                            move |cx| {
                                request_on(Some(host.clone()), cx, move |session| async move {
                                    session.delete_label(&name).await
                                })
                            },
                            window,
                            cx,
                        )
                    })
                })
                .detach_and_log_err(cx);
            })
        });
    }
    menu
}

fn setup_can_run(status: &str) -> bool {
    matches!(status, "blocked" | "failed")
}

fn scripts_submenu(
    mut menu: ContextMenu,
    workspace_id: &str,
    scripts: &[paseo_client::WorkspaceScript],
    setup: Option<&paseo_client::SetupSnapshot>,
) -> ContextMenu {
    for script in scripts {
        let running = script.running;
        let label = if running {
            format!("Stop {}", script.name)
        } else {
            format!("Start {}", script.name)
        };
        let (workspace_id, script_name) = (workspace_id.to_owned(), script.name.clone());
        menu = menu.entry(label, None, move |_, cx| {
            let (workspace_id, script_name) = (workspace_id.clone(), script_name.clone());
            request_on(
                crate::hosts::store_for_workspace(&workspace_id, cx),
                cx,
                move |session| async move {
                    session
                        .set_workspace_script_running(&workspace_id, &script_name, !running)
                        .await
                },
            );
        });
        if let Some(url) = script.proxy_url.clone().filter(|_| running) {
            menu = menu.entry(format!("Open {}", script.name), None, move |_, cx| {
                cx.open_url(&url);
            });
        }
    }
    if let Some(setup) = setup.filter(|setup| setup_can_run(&setup.status)) {
        if !scripts.is_empty() {
            menu = menu.separator();
        }
        if let Some(error) = setup.error.clone() {
            menu = menu.label(format!("Setup {}: {error}", setup.status));
        }
        let workspace_id = workspace_id.to_owned();
        menu = menu.entry("Run Setup", None, move |_, cx| {
            let workspace_id = workspace_id.clone();
            request_on(
                crate::hosts::store_for_workspace(&workspace_id, cx),
                cx,
                move |session| async move { session.run_workspace_setup(&workspace_id).await },
            );
        });
    }
    menu
}

/// Paseo's New Workspace window: where the workspace lives, how it is isolated, and whether it
/// starts with a chat, written right here, or only a terminal. A chat never opens as a draft tab;
/// its first message creates the workspace, which then opens in its own editor workspace.
/// The New Workspace window's unsent message, kept from closing the window to reopening it until
/// it creates an agent.
#[derive(Default)]
struct UnsentNewWorkspaceMessage(String);

impl Global for UnsentNewWorkspaceMessage {}

pub(crate) struct NewWorkspaceModal {
    workspace: WeakEntity<Workspace>,
    /// Holds the draft's folder, isolation and base branch for both launches.
    composer: Entity<Composer>,
    launch_terminal: bool,
    title: Entity<InputField>,
    creating: bool,
    agent_created: bool,
    error: Option<String>,
    /// Kept across retries, so trying again after a timeout can't create a second workspace.
    idempotency_key: String,
    _composer_subscriptions: [Subscription; 3],
    _keep_unsent_message: Subscription,
}

/// Opens the New Workspace window, starting in `project`'s folder on its host when given.
pub(crate) fn open_new_workspace(
    workspace: &mut Workspace,
    project: Option<(PathBuf, Entity<PaseoStore>)>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        NewWorkspaceModal::new(handle, project, window, cx)
    });
}

impl NewWorkspaceModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Option<(PathBuf, Entity<PaseoStore>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (directory, host) = match project {
            Some((directory, host)) => (Some(directory), host),
            None => (None, crate::hosts::default_store(cx)),
        };
        // The window shows the host's error for its own send, not one left by another chat.
        host.update(cx, |store, cx| store.dismiss_error(cx));
        let composer = cx.new(|cx| Composer::new(host, None, directory, window, cx));
        let unsent = cx
            .try_global::<UnsentNewWorkspaceMessage>()
            .map(|unsent| unsent.0.clone())
            .unwrap_or_default();
        if !unsent.is_empty() {
            composer.update(cx, |composer, cx| composer.set_text(&unsent, window, cx));
        }
        // Escape, clicking outside and the shortcut all close the window by releasing it.
        let keep_unsent_message = cx.on_release(|modal, cx| {
            let text = if modal.agent_created {
                String::new()
            } else {
                modal.composer.read(cx).text(cx)
            };
            cx.set_global(UnsentNewWorkspaceMessage(text));
        });
        let title = cx.new(|cx| InputField::new(window, cx, "Optional").label("Title"));
        let subscriptions = Self::watch_composer(&composer, window, cx);
        Self {
            workspace,
            composer,
            launch_terminal: false,
            title,
            creating: false,
            agent_created: false,
            error: None,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            _composer_subscriptions: subscriptions,
            _keep_unsent_message: keep_unsent_message,
        }
    }

    fn watch_composer(
        composer: &Entity<Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> [Subscription; 3] {
        let store = composer.read(cx).store.clone();
        [
            cx.subscribe_in(composer, window, Self::handle_composer_event),
            cx.observe(composer, |_, _, cx| cx.notify()),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ]
    }

    fn handle_composer_event(
        &mut self,
        _composer: &Entity<Composer>,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ComposerEvent::AgentCreated(agent_id) = event else {
            return;
        };
        let (workspace, agent_id) = (self.workspace.clone(), agent_id.clone());
        self.agent_created = true;
        cx.emit(DismissEvent);
        crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
            crate::open_agent(workspace, &agent_id, true, window, cx)
        });
    }

    fn store(&self, cx: &App) -> Entity<PaseoStore> {
        self.composer.read(cx).store.clone()
    }

    /// A composer runs agents on one host, so another host gets a fresh composer with the typed
    /// text. A folder belongs to its host, so the new one starts without the old one's.
    fn switch_host(
        &mut self,
        store: Entity<PaseoStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.store(cx) == store {
            return;
        }
        let text = self.composer.read(cx).text(cx);
        store.update(cx, |store, cx| store.dismiss_error(cx));
        let composer = cx.new(|cx| Composer::new(store, None, None, window, cx));
        composer.update(cx, |composer, cx| {
            // The saved folder may be on the old host.
            composer.clear_draft_directory(cx);
            composer.set_text(&text, window, cx);
            composer.focus(window, cx);
        });
        self._composer_subscriptions = Self::watch_composer(&composer, window, cx);
        self.composer = composer;
        cx.notify();
    }

    fn set_launch_terminal(
        &mut self,
        launch_terminal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.launch_terminal = launch_terminal;
        self.error = None;
        let focus = self.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch_terminal {
            self.create_with_terminal(window, cx);
        } else {
            cx.propagate();
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn create_with_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let composer = self.composer.read(cx);
        let Some(root) = composer.draft_directory.clone() else {
            self.error = Some("Choose a project for the new workspace".into());
            cx.notify();
            return;
        };
        let new_worktree = composer.uses_new_worktree(cx);
        let base = composer.worktree_base().map(|base| base.ref_name.clone());
        let host = composer.store.clone();
        let project_id = host
            .read(cx)
            .state
            .projects
            .values()
            .find(|project| project.root_path == root)
            .map(|project| project.id.clone());
        let root = root.display().to_string();
        let source = if new_worktree {
            paseo_client::WorkspaceSource::Worktree {
                cwd: root,
                project_id,
                base_ref: base,
            }
        } else {
            paseo_client::WorkspaceSource::Directory {
                path: root,
                project_id,
            }
        };
        let title = self.title.read(cx).text(cx);
        let idempotency_key = self.idempotency_key.clone();
        let task = host.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session
                    .create_workspace(&source, Some(&title), &idempotency_key)
                    .await
            })
        });
        self.creating = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |modal, window, cx| match result {
                Ok(created) => {
                    let directory = created.directory.display().to_string();
                    if let Err(error) = modal.workspace.update(cx, |_, cx| {
                        terminal::new_terminal(host.clone(), directory, window, cx)
                    }) {
                        log::debug!("Paseo workspace closed: {error}");
                    }
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    modal.creating = false;
                    modal.error = Some(error.to_string());
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_project_picker(&self, cx: &Context<Self>) -> AnyElement {
        let directory = self.composer.read(cx).draft_directory.clone();
        let label = directory
            .as_ref()
            .and_then(|directory| directory.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Choose a project".into());
        let tooltip = directory
            .map(|directory| directory.display().to_string())
            .unwrap_or_else(|| "Where the workspace starts".into());
        let (workspace, store, composer) = (
            self.workspace.clone(),
            self.store(cx),
            self.composer.downgrade(),
        );
        PopoverMenu::new("paseo-new-workspace-project")
            .trigger_with_tooltip(
                checkout_picker(
                    "paseo-new-workspace-project-button",
                    IconName::Folder,
                    label.into(),
                ),
                Tooltip::text(tooltip),
            )
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let recent = workspace
                    .upgrade()
                    .map(|workspace| recent_directories(workspace.read(cx), &store, cx))
                    .unwrap_or_default();
                let composer = composer.clone();
                let on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)> =
                    Rc::new(move |directory, window, cx| {
                        if let Err(error) = composer.update(cx, |composer, cx| {
                            composer.set_draft_directory(directory, cx);
                            composer.focus(window, cx);
                        }) {
                            log::debug!("Paseo composer closed: {error}");
                        }
                    });
                let store = store.clone();
                Some(cx.new(|cx| {
                    DirectoryPicker::new(
                        store,
                        "Search directories, or type an absolute path…",
                        on_choose,
                        recent,
                        window,
                        cx,
                    )
                }))
            })
            .into_any_element()
    }

    /// Which host the workspace starts on, shown only when there is more than one.
    fn render_host_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let modal = cx.weak_entity();
        crate::agent_view::render_host_picker(
            "paseo-new-workspace-host",
            "paseo-new-workspace-host-button",
            "Choose the host",
            self.store(cx),
            crate::hosts::configured_hosts(cx),
            move |store, window, cx| {
                if let Err(error) =
                    modal.update(cx, |modal, cx| modal.switch_host(store, window, cx))
                {
                    log::debug!("Paseo new workspace closed: {error}");
                }
            },
        )
    }

    fn render_isolation_picker(&self, cx: &Context<Self>) -> AnyElement {
        let composer = self.composer.read(cx);
        if !composer.can_create_worktree(cx) {
            return checkout_label(IconName::Folder, "Local checkout".into());
        }
        let new_worktree = composer.uses_new_worktree(cx);
        let (icon, label) = if new_worktree {
            (IconName::GitWorktree, "New worktree")
        } else {
            (IconName::Folder, "Local checkout")
        };
        let composer = self.composer.downgrade();
        PopoverMenu::new("paseo-new-workspace-isolation")
            .trigger_with_tooltip(
                checkout_picker("paseo-new-workspace-isolation-button", icon, label.into()),
                Tooltip::text("Work in the project's checkout or a new worktree"),
            )
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let composer = composer.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let choose = |new_worktree: bool| {
                        let composer = composer.clone();
                        move |_: &mut Window, cx: &mut App| {
                            if let Err(error) = composer.update(cx, |composer, cx| {
                                composer.set_new_worktree(new_worktree, cx)
                            }) {
                                log::debug!("Paseo composer closed: {error}");
                            }
                        }
                    };
                    menu.toggleable_entry(
                        "Local checkout",
                        !new_worktree,
                        IconPosition::Start,
                        None,
                        choose(false),
                    )
                    .toggleable_entry(
                        "New worktree",
                        new_worktree,
                        IconPosition::Start,
                        None,
                        choose(true),
                    )
                }))
            })
            .into_any_element()
    }

    /// The branch a new worktree starts from.
    fn render_branch_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let composer = self.composer.read(cx);
        if !composer.uses_new_worktree(cx) {
            return None;
        }
        let directory = composer.draft_directory.as_ref()?.to_str()?.to_owned();
        let selected = composer.worktree_base().cloned();
        let label = selected
            .as_ref()
            .map(|base| base.label.clone())
            .unwrap_or_else(|| "Default branch".into());
        let tooltip = selected
            .as_ref()
            .map(|base| format!("Branch off {}", base.ref_name))
            .unwrap_or_else(|| "Branch off the repository's default branch".into());
        let (store, composer) = (self.store(cx), self.composer.downgrade());
        Some(
            PopoverMenu::new("paseo-new-workspace-branch")
                .trigger_with_tooltip(
                    checkout_picker(
                        "paseo-new-workspace-branch-button",
                        IconName::GitBranch,
                        label.into(),
                    ),
                    Tooltip::text(tooltip),
                )
                .anchor(gpui::Anchor::TopLeft)
                .menu(move |window, cx| {
                    let (store, composer, directory, selected) = (
                        store.clone(),
                        composer.clone(),
                        directory.clone(),
                        selected.clone(),
                    );
                    Some(cx.new(|cx| {
                        BaseBranchPicker::new(store, composer, directory, selected, window, cx)
                    }))
                })
                .into_any_element(),
        )
    }

    fn render_launch_picker(&self, cx: &Context<Self>) -> AnyElement {
        let launch_terminal = self.launch_terminal;
        let (icon, label) = if launch_terminal {
            (IconName::Terminal, "Terminal")
        } else {
            (IconName::Chat, "Chat")
        };
        let modal = cx.weak_entity();
        PopoverMenu::new("paseo-new-workspace-launch")
            .trigger_with_tooltip(
                checkout_picker("paseo-new-workspace-launch-button", icon, label.into()),
                Tooltip::text("Start with a chat or only a terminal"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let modal = modal.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let choose = |launch_terminal: bool| {
                        let modal = modal.clone();
                        move |window: &mut Window, cx: &mut App| {
                            if let Err(error) = modal.update(cx, |modal, cx| {
                                modal.set_launch_terminal(launch_terminal, window, cx)
                            }) {
                                log::debug!("Paseo new workspace closed: {error}");
                            }
                        }
                    };
                    menu.toggleable_entry(
                        "Chat",
                        !launch_terminal,
                        IconPosition::Start,
                        None,
                        choose(false),
                    )
                    .toggleable_entry(
                        "Terminal",
                        launch_terminal,
                        IconPosition::Start,
                        None,
                        choose(true),
                    )
                }))
            })
            .into_any_element()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl NewWorkspaceModal {
    pub(crate) fn composer_for_test(&self) -> Entity<Composer> {
        self.composer.clone()
    }
}

impl EventEmitter<DismissEvent> for NewWorkspaceModal {}
impl ModalView for NewWorkspaceModal {}

impl Focusable for NewWorkspaceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.launch_terminal {
            self.title.focus_handle(cx)
        } else {
            self.composer.focus_handle(cx)
        }
    }
}

impl Render for NewWorkspaceModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_error = self.store(cx).read(cx).state.error.clone();
        v_flex()
            .key_context("PaseoNewWorkspace")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            // Escape in the composer asks to interrupt, which a draft passes on.
            .on_action(cx.listener(|_, _: &InterruptAgent, _, cx| cx.emit(DismissEvent)))
            .w(px(860.))
            .p_4()
            .gap_3()
            .elevation_3(cx)
            .rounded_md()
            .child(Headline::new("New workspace").size(HeadlineSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .child(self.render_project_picker(cx))
                    .children(self.render_host_picker(cx))
                    .child(self.render_isolation_picker(cx))
                    .children(self.render_branch_picker(cx))
                    .child(div().flex_1())
                    .child(self.render_launch_picker(cx)),
            )
            .when(!self.launch_terminal, |this| {
                this.child(self.composer.clone())
                    .when_some(store_error, |this, error| {
                        this.child(crate::render_inline_error(error))
                    })
            })
            .when(self.launch_terminal, |this| {
                this.child(self.title.clone())
                    .when_some(self.error.clone(), |this, error| {
                        this.child(crate::render_inline_error(error))
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                ui::Button::new("paseo-new-workspace-cancel", "Cancel")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                ui::Button::new(
                                    "paseo-new-workspace-create",
                                    if self.creating {
                                        "Creating…"
                                    } else {
                                        "Create"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(self.creating)
                                .on_click(cx.listener(
                                    |modal, _, window, cx| modal.create_with_terminal(window, cx),
                                )),
                            ),
                    )
            })
    }
}

/// The project header's right-click menu.
pub(crate) fn project_menu(
    menu: ContextMenu,
    project_id: &str,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> ContextMenu {
    let Some(host) = crate::hosts::store_for_project(project_id, cx) else {
        return menu.label("This project is gone");
    };
    let store = host.read(cx);
    let Some(project) = store.state.projects.get(project_id).cloned() else {
        return menu.label("This project is gone");
    };
    let is_local = store.is_local_host();
    let workspace_count = store
        .state
        .workspaces
        .values()
        .filter(|workspace| workspace.project_id == project.id)
        .count();
    let new_agent = (workspace.clone(), project.root_path.clone(), host.clone());
    let new_workspace = (workspace.clone(), project.root_path.clone(), host.clone());
    let worktrees_handle = (workspace.clone(), project.id.clone());
    let rename = (workspace, project.id.clone(), project_label(&project));
    let icon_project = project.id.clone();
    let automatic_icon = project.id.clone();
    let copy_path = project.root_path.display().to_string();
    let reveal_path = project.root_path.clone();
    let remove_id = project.id.clone();
    let remove_name = project_label(&project);
    let mut menu = menu
        .entry("New Agent", None, move |window, cx| {
            let (workspace, directory, host) = new_agent.clone();
            crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
                crate::open_draft_in_on(workspace, host, directory, window, cx)
                    .detach_and_log_err(cx);
            });
        })
        .entry("New Workspace…", None, move |window, cx| {
            let (workspace, directory, host) = new_workspace.clone();
            crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
                open_new_workspace(workspace, Some((directory, host)), window, cx);
            });
        })
        .entry("Paseo Worktrees…", None, move |window, cx| {
            let (workspace, project_id) = worktrees_handle.clone();
            crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
                worktrees::open_worktrees(workspace, &project_id, window, cx);
            });
        })
        .separator()
        .entry("Rename…", None, move |window, cx| {
            let (workspace, project_id, current) = rename.clone();
            prompt_from_menu(
                workspace,
                "Rename project",
                "Project name (empty uses the default)",
                current,
                move |name, _, cx| {
                    let project_id = project_id.clone();
                    let name = Some(name.trim().to_owned()).filter(|name| !name.is_empty());
                    request_on(
                        crate::hosts::store_for_project(&project_id, cx),
                        cx,
                        move |session| async move {
                            session.rename_project(&project_id, name.as_deref()).await
                        },
                    );
                },
                window,
                cx,
            );
        })
        .entry("Set Icon…", None, move |_, cx| {
            choose_project_icon(icon_project.clone(), cx);
        })
        .entry("Use Automatic Icon", None, move |_, cx| {
            let project_id = automatic_icon.clone();
            request_on(
                crate::hosts::store_for_project(&project_id, cx),
                cx,
                move |session| async move { session.set_project_icon(&project_id, None).await },
            );
        })
        .separator()
        .entry("Copy Path", None, move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
        });
    if is_local {
        menu = menu.entry(
            ui::utils::reveal_in_file_manager_label(false),
            None,
            move |_, cx| {
                cx.reveal_path(&reveal_path);
            },
        );
    }
    menu.separator()
        .entry("Remove Project…", None, move |window, cx| {
            let project_id = remove_id.clone();
            let detail = match workspace_count {
                0 => "Files on disk are not changed.".to_owned(),
                1 => "Its workspace leaves the sidebar. Files on disk are not changed.".to_owned(),
                count => format!(
                    "Its {count} workspaces leave the sidebar. Files on disk are not changed."
                ),
            };
            confirm_then(
                &format!("Remove the project “{remove_name}” from Paseo?"),
                &detail,
                "Remove",
                move |cx| {
                    request_on(
                        crate::hosts::store_for_project(&project_id, cx),
                        cx,
                        move |session| async move { session.remove_project(&project_id).await },
                    )
                },
                window,
                cx,
            );
        })
}

/// The host of a project, else the default host, for errors about it.
fn project_host(project_id: &str, cx: &App) -> Entity<PaseoStore> {
    crate::hosts::store_for_project(project_id, cx)
        .unwrap_or_else(|| crate::hosts::default_store(cx))
}

/// Uploads an image chosen on this machine as the project's icon.
fn choose_project_icon(project_id: String, cx: &mut App) {
    let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Choose Icon".into()),
    });
    cx.spawn(async move |cx| {
        let paths = match paths.await {
            Ok(Ok(Some(paths))) => paths,
            Ok(Ok(None)) | Err(_) => return anyhow::Ok(()),
            Ok(Err(error)) => {
                cx.update(|cx| {
                    project_host(&project_id, cx).update(cx, |store, cx| {
                        store.state.error = Some(format!("Could not choose an icon: {error}"));
                        cx.notify();
                    })
                });
                return Ok(());
            }
        };
        let Some(path) = paths.into_iter().next() else {
            return Ok(());
        };
        let bytes = cx
            .background_spawn(async move { std::fs::read(&path) })
            .await;
        cx.update(|cx| match bytes {
            Ok(bytes) => request_on(crate::hosts::store_for_project(&project_id, cx), cx, move |session| async move {
                session.set_project_icon(&project_id, Some(&bytes)).await
            }),
            Err(error) => project_host(&project_id, cx).update(cx, |store, cx| {
                store.state.error = Some(format!("Could not read the icon image: {error}"));
                cx.notify();
            }),
        });
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Adds a host directory as a Paseo project.
pub(crate) fn add_project(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let host = crate::hosts::default_store(cx);
    crate::command_center::pick_directory(
        workspace,
        host.clone(),
        "Add a project: search directories, or type an absolute path…",
        move |directory, _, cx| {
            let cwd = directory.display().to_string();
            request_on(Some(host.clone()), cx, move |session| async move {
                session.add_project(&cwd).await.map(drop)
            });
        },
        window,
        cx,
    );
}

/// Creates a directory on the host and adds it as a project.
pub(crate) fn new_project_directory(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let handle = cx.weak_entity();
    let host = crate::hosts::default_store(cx);
    crate::command_center::pick_directory(
        workspace,
        host.clone(),
        "New project: choose the parent directory…",
        move |parent: PathBuf, window, cx| {
            let parent = parent.display().to_string();
            let host = host.clone();
            prompt_from_menu(
                handle.clone(),
                "New project directory",
                "Directory name",
                String::new(),
                move |name, _, cx| {
                    let name = name.trim().to_owned();
                    if name.is_empty() {
                        return;
                    }
                    let parent = parent.clone();
                    request_on(Some(host.clone()), cx, move |session| async move {
                        session
                            .create_project_directory(&parent, &name)
                            .await
                            .map(drop)
                    });
                },
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn agent(id: &str, extra: Value) -> AgentSummary {
        AgentSummary {
            id: id.into(),
            title: None,
            status: "idle".into(),
            directory: None,
            project: None,
            extra,
        }
    }

    #[test]
    fn workspace_menu_offers_mark_read_only_with_attention() {
        let agents = vec![
            agent(
                "finished",
                json!({"workspaceId": "wks_1", "requiresAttention": true, "attentionReason": "finished"}),
            ),
            agent(
                "asking",
                json!({"workspaceId": "wks_2", "requiresAttention": true, "attentionReason": "permission"}),
            ),
            agent(
                "quiet",
                json!({"workspaceId": "wks_3", "requiresAttention": false}),
            ),
        ];
        assert!(has_clearable_attention(&agents, "wks_1"));
        assert!(
            !has_clearable_attention(&agents, "wks_2"),
            "only answering clears a permission request"
        );
        assert!(!has_clearable_attention(&agents, "wks_3"));
        assert!(!has_clearable_attention(&agents, "wks_missing"));
    }

    #[test]
    fn new_labels_take_an_unused_color() {
        let used = |colors: &[&str]| {
            colors
                .iter()
                .map(|color| WorkspaceLabel {
                    name: (*color).into(),
                    color: (*color).into(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(next_label_color(&[]), "violet");
        assert_eq!(next_label_color(&used(&["violet"])), "sky");
        assert_eq!(next_label_color(&used(&LABEL_COLORS)), "violet");
    }

    #[test]
    fn archive_detail_names_the_worktree_it_removes() {
        let mut workspace = WorkspaceDescriptor {
            id: "wks_1".into(),
            project_id: "prj_1".into(),
            project_display_name: "zaseo".into(),
            project_root_path: PathBuf::from("/repo"),
            directory: PathBuf::from("/home/sr/.paseo/worktrees/abc/snake"),
            kind: "worktree".into(),
            worktree_slug: Some("snake".into()),
            name: "Snake".into(),
            title: None,
            pinned_at: None,
            labels: Vec::new(),
            status: "done".into(),
            activity_at: None,
            diff_stat: None,
            scripts: Vec::new(),
            current_branch: Some("snake".into()),
            is_paseo_worktree: true,
            extra: json!({}),
        };
        assert_eq!(
            archive_detail(&workspace, 2),
            "Its 2 agents are archived. Once no other workspace uses it, its worktree folder /home/sr/.paseo/worktrees/abc/snake is removed from disk, including uncommitted and untracked changes. The branch is kept."
        );
        workspace.kind = "directory".into();
        workspace.is_paseo_worktree = false;
        assert_eq!(
            archive_detail(&workspace, 1),
            "Its agent is archived. Files on disk are not changed."
        );
    }
}
