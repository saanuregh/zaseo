use std::{path::PathBuf, rc::Rc};

use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, PromptLevel, WeakEntity, Window, prelude::*, px,
};
use menu::{Cancel, Confirm};
use paseo_client::{AgentSummary, WorkspaceDescriptor, WorkspaceLabel};
use serde_json::Value;
use ui::{ContextMenu, prelude::*};
use ui_input::InputField;
use workspace::{ModalView, Workspace};

use crate::{composer::BaseRef, sidebar::project_label, store, terminal, worktrees};

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
            .rounded_lg()
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
    window.defer(cx, move |window, cx| {
        if let Err(error) = workspace.update(cx, |workspace, cx| {
            open_text_prompt(
                workspace,
                title,
                placeholder,
                &initial,
                on_confirm,
                window,
                cx,
            )
        }) {
            log::debug!("Paseo workspace closed: {error}");
        }
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
        agent
            .extra
            .get("requiresAttention")
            .and_then(Value::as_bool)
            == Some(true)
            && agent.extra.get("attentionReason").and_then(Value::as_str) != Some("permission")
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
    if workspace.is_paseo_worktree || workspace.kind == "worktree" {
        format!(
            "{agents} Once no other workspace uses it, its worktree folder {} is removed from disk, including uncommitted and untracked changes. The branch is kept.",
            workspace.directory.display()
        )
    } else {
        format!("{agents} Files on disk are not changed.")
    }
}

/// Runs a daemon request whose failure shows in the Paseo error banner.
fn request<F, Fut>(cx: &mut App, make_request: F)
where
    F: FnOnce(std::sync::Arc<paseo_client::PaseoSession>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    store(cx).update(cx, |store, cx| {
        store.request_reporting_errors(cx, make_request)
    });
}

/// Sets a workspace's title; an empty name restores the default.
fn rename_workspace(workspace_id: String, name: &str, cx: &mut App) {
    let title = Some(name.trim().to_owned()).filter(|name| !name.is_empty());
    request(cx, move |session| async move {
        session
            .set_workspace_title(&workspace_id, title.as_deref())
            .await
    });
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

/// The workspace row's right-click menu, like Paseo's sidebar workspace menu.
pub(crate) fn workspace_menu(
    menu: ContextMenu,
    workspace_id: &str,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> ContextMenu {
    let store = store(cx);
    let store = store.read(cx);
    let Some(descriptor) = store.state.workspaces.get(workspace_id).cloned() else {
        return menu.label("This workspace is gone");
    };
    let agent_count = workspace_agents(&store.state.agents, workspace_id).count();
    let clearable = has_clearable_attention(&store.state.agents, workspace_id);
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
    let attention_id = id.clone();
    let pin_id = id.clone();
    let pinned = descriptor.pinned_at.is_some();
    let copy_path = descriptor.directory.display().to_string();
    let branch = descriptor.current_branch.clone();
    let reveal_path = descriptor.directory.clone();
    let archive = (id.clone(), archive_detail(&descriptor, agent_count));
    let label_descriptor = descriptor.clone();
    let label_workspace = workspace;
    let scripts = descriptor.scripts;
    let scripts_id = id;

    let mut menu = menu
        .entry("New Agent Here", None, move |window, cx| {
            let (workspace, paseo_workspace_id) = new_agent.clone();
            window.defer(cx, move |window, cx| {
                if let Err(error) = workspace.update(cx, |workspace, cx| {
                    crate::new_agent_in_paseo_workspace(workspace, &paseo_workspace_id, window, cx)
                        .detach_and_log_err(cx);
                }) {
                    log::debug!("Paseo workspace closed: {error}");
                }
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
    menu = if clearable {
        menu.entry("Mark as Read", None, move |_, cx| {
            let workspace_id = attention_id.clone();
            request(cx, move |session| async move {
                session.clear_workspace_attention(vec![workspace_id]).await
            });
        })
    } else {
        menu.entry("Mark as Unread", None, move |_, cx| {
            let workspace_id = attention_id.clone();
            request(cx, move |session| async move {
                session.mark_workspace_unread(&workspace_id).await
            });
        })
    };
    menu = menu
        .entry(
            if pinned { "Unpin" } else { "Pin to Top" },
            None,
            move |_, cx| {
                let workspace_id = pin_id.clone();
                request(cx, move |session| async move {
                    session.set_workspace_pinned(&workspace_id, !pinned).await
                });
            },
        )
        .submenu("Labels", move |menu, _, _| {
            labels_submenu(menu, &label_descriptor, &labels, label_workspace.clone())
        });
    if !scripts.is_empty()
        || setup
            .as_ref()
            .is_some_and(|setup| setup_can_run(&setup.status))
    {
        menu = menu.submenu("Scripts", move |menu, _, _| {
            scripts_submenu(menu, &scripts_id, &scripts, setup.as_ref())
        });
    }
    menu = menu.separator().entry("Copy Path", None, move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
    });
    if let Some(branch) = branch {
        menu = menu.entry("Copy Branch Name", None, move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
        });
    }
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
        .entry("Archive Workspace…", None, move |window, cx| {
            let (workspace_id, detail) = archive.clone();
            confirm_then(
                "Archive this workspace?",
                &detail,
                "Archive",
                move |cx| {
                    request(cx, move |session| async move {
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
                request(cx, move |session| async move {
                    session
                        .set_workspace_label(&workspace_id, &label, !assigned)
                        .await
                        .map(drop)
                });
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
                request(cx, move |session| async move {
                    session
                        .set_workspace_label(&workspace_id, &label, true)
                        .await
                        .map(drop)
                });
            },
            window,
            cx,
        );
    })
    .when(!manage_labels.is_empty(), |menu| {
        menu.submenu("Manage Labels", move |menu, _, _| {
            manage_labels_submenu(menu, &manage_labels, workspace_handle.clone())
        })
    })
}

fn manage_labels_submenu(
    mut menu: ContextMenu,
    labels: &[WorkspaceLabel],
    workspace: WeakEntity<Workspace>,
) -> ContextMenu {
    for label in labels {
        let label = label.clone();
        let workspace = workspace.clone();
        menu = menu.submenu(label.name.clone(), move |mut menu, _, _| {
            let rename = (workspace.clone(), label.name.clone());
            menu = menu.entry("Rename…", None, move |window, cx| {
                let (workspace, name) = rename.clone();
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
                        request(cx, move |session| async move {
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
            menu = menu.submenu("Color", move |mut menu, _, _| {
                for color in LABEL_COLORS {
                    let name = color_name.clone();
                    menu = menu.toggleable_entry(
                        color,
                        current_color == color,
                        ui::IconPosition::End,
                        None,
                        move |_, cx| {
                            let name = name.clone();
                            request(cx, move |session| async move {
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
            menu.separator().entry("Delete…", None, move |window, cx| {
                let name = delete_name.clone();
                let usage = store(cx).update(cx, |store, cx| {
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
                                request(cx, move |session| async move {
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
            request(cx, move |session| async move {
                session
                    .set_workspace_script_running(&workspace_id, &script_name, !running)
                    .await
            });
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
            request(cx, move |session| async move {
                session.run_workspace_setup(&workspace_id).await
            });
        });
    }
    menu
}

/// Paseo's New Workspace dialog: isolation, base branch, title, and whether the workspace starts
/// with a chat or only a terminal.
pub(crate) struct NewWorkspaceModal {
    project_id: String,
    project_name: String,
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    new_worktree: bool,
    launch_terminal: bool,
    base: Entity<InputField>,
    title: Entity<InputField>,
    creating: bool,
    error: Option<String>,
    /// Kept across retries, so trying again after a timeout can't create a second workspace.
    idempotency_key: String,
    focus_handle: FocusHandle,
}

impl NewWorkspaceModal {
    fn new(
        project: &paseo_client::ProjectDescriptor,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let base = cx.new(|cx| {
            InputField::new(window, cx, "Default: the repository's default branch")
                .label("Base branch")
        });
        let title = cx.new(|cx| InputField::new(window, cx, "Optional").label("Title"));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            project_id: project.id.clone(),
            project_name: project_label(project),
            root: project.root_path.clone(),
            workspace,
            new_worktree: project.kind == "git",
            launch_terminal: false,
            base,
            title,
            creating: false,
            error: None,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            focus_handle,
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.create(window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let base =
            Some(self.base.read(cx).text(cx).trim().to_owned()).filter(|base| !base.is_empty());
        if !self.launch_terminal {
            self.open_chat_draft(base, window, cx);
            return;
        }
        let root = self.root.display().to_string();
        let source = if self.new_worktree {
            paseo_client::WorkspaceSource::Worktree {
                cwd: root,
                project_id: Some(self.project_id.clone()),
                base_ref: base,
            }
        } else {
            paseo_client::WorkspaceSource::Directory {
                path: root,
                project_id: Some(self.project_id.clone()),
            }
        };
        let title = self.title.read(cx).text(cx);
        let idempotency_key = self.idempotency_key.clone();
        let task = store(cx).update(cx, |store, cx| {
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
                    if let Err(error) = modal
                        .workspace
                        .update(cx, |_, cx| terminal::new_terminal(directory, window, cx))
                    {
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

    /// A chat workspace starts with its first message, so this opens the agent draft set up the
    /// same way.
    fn open_chat_draft(
        &mut self,
        base: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (root, new_worktree) = (self.root.clone(), self.new_worktree);
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            if let Err(error) = workspace.update(cx, |workspace, cx| {
                let draft = crate::open_draft_in(workspace, root, window, cx);
                cx.spawn(async move |_, cx| {
                    let tab = draft.await?;
                    let composer =
                        tab.read_with(cx, |tab, cx| tab.view().read(cx).composer.clone());
                    composer.update(cx, |composer, cx| {
                        composer.set_new_worktree(new_worktree, cx);
                        if let Some(base) = base {
                            composer.set_worktree_base(
                                BaseRef {
                                    label: base.clone(),
                                    ref_name: base,
                                    detail: None,
                                },
                                cx,
                            );
                        }
                    });
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }) {
                log::debug!("Paseo workspace closed: {error}");
            }
        });
    }

    fn choice(
        id: &'static str,
        label: &'static str,
        selected: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        ui::Button::new(id, label)
            .style(if selected {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .toggle_state(selected)
            .on_click(cx.listener(move |modal, _, _, cx| {
                on_click(modal, cx);
                cx.notify();
            }))
    }
}

impl EventEmitter<DismissEvent> for NewWorkspaceModal {}
impl ModalView for NewWorkspaceModal {}

impl Focusable for NewWorkspaceModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NewWorkspaceModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let new_worktree = self.new_worktree;
        let launch_terminal = self.launch_terminal;
        let row = |label: &'static str| {
            h_flex().gap_2().child(
                div()
                    .w(px(90.))
                    .child(Label::new(label).color(Color::Muted)),
            )
        };
        v_flex()
            .key_context("PaseoRename")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(px(460.))
            .p_4()
            .gap_3()
            .elevation_3(cx)
            .rounded_lg()
            .child(
                Headline::new(format!("New workspace in {}", self.project_name))
                    .size(HeadlineSize::Small),
            )
            .child(
                row("Isolation")
                    .child(Self::choice(
                        "paseo-new-workspace-local",
                        "Local",
                        !new_worktree,
                        |modal, _| modal.new_worktree = false,
                        cx,
                    ))
                    .child(Self::choice(
                        "paseo-new-workspace-worktree",
                        "New worktree",
                        new_worktree,
                        |modal, _| modal.new_worktree = true,
                        cx,
                    )),
            )
            .when(new_worktree, |this| this.child(self.base.clone()))
            .child(
                row("Launch")
                    .child(Self::choice(
                        "paseo-new-workspace-chat",
                        "Chat",
                        !launch_terminal,
                        |modal, _| modal.launch_terminal = false,
                        cx,
                    ))
                    .child(Self::choice(
                        "paseo-new-workspace-terminal",
                        "Terminal",
                        launch_terminal,
                        |modal, _| modal.launch_terminal = true,
                        cx,
                    )),
            )
            .when(launch_terminal, |this| this.child(self.title.clone()))
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
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
                            } else if launch_terminal {
                                "Create"
                            } else {
                                "Open Draft"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.creating)
                        .on_click(cx.listener(|modal, _, window, cx| modal.create(window, cx))),
                    ),
            )
    }
}

/// The project header's right-click menu.
pub(crate) fn project_menu(
    menu: ContextMenu,
    project_id: &str,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> ContextMenu {
    let store = store(cx);
    let store = store.read(cx);
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
    let new_agent = (workspace.clone(), project.root_path.clone());
    let new_workspace = (workspace.clone(), project.clone());
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
            let (workspace, directory) = new_agent.clone();
            window.defer(cx, move |window, cx| {
                if let Err(error) = workspace.update(cx, |workspace, cx| {
                    crate::open_draft_in(workspace, directory, window, cx).detach_and_log_err(cx);
                }) {
                    log::debug!("Paseo workspace closed: {error}");
                }
            });
        })
        .entry("New Workspace…", None, move |window, cx| {
            let (workspace, project) = new_workspace.clone();
            window.defer(cx, move |window, cx| {
                let modal_workspace = workspace.clone();
                if let Err(error) = workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        NewWorkspaceModal::new(&project, modal_workspace, window, cx)
                    });
                }) {
                    log::debug!("Paseo workspace closed: {error}");
                }
            });
        })
        .entry("Paseo Worktrees…", None, move |window, cx| {
            let (workspace, project_id) = worktrees_handle.clone();
            window.defer(cx, move |window, cx| {
                if let Err(error) = workspace.update(cx, |workspace, cx| {
                    worktrees::open_worktrees(workspace, &project_id, window, cx);
                }) {
                    log::debug!("Paseo workspace closed: {error}");
                }
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
                    request(cx, move |session| async move {
                        session.rename_project(&project_id, name.as_deref()).await
                    });
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
            request(cx, move |session| async move {
                session.set_project_icon(&project_id, None).await
            });
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
                    request(cx, move |session| async move {
                        session.remove_project(&project_id).await
                    })
                },
                window,
                cx,
            );
        })
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
                    store(cx).update(cx, |store, cx| {
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
            Ok(bytes) => request(cx, move |session| async move {
                session.set_project_icon(&project_id, Some(&bytes)).await
            }),
            Err(error) => store(cx).update(cx, |store, cx| {
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
    crate::command_center::pick_directory(
        workspace,
        "Add a project: search directories, or type an absolute path…",
        |directory, _, cx| {
            let cwd = directory.display().to_string();
            request(cx, move |session| async move {
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
    crate::command_center::pick_directory(
        workspace,
        "New project: choose the parent directory…",
        move |parent: PathBuf, window, cx| {
            let parent = parent.display().to_string();
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
                    request(cx, move |session| async move {
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
    use serde_json::json;

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
