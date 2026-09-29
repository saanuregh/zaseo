use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, SharedString, Subscription, TaskExt, Window, prelude::*, px,
};
use paseo_client::{AgentSummary, PaseoWorktree, WorkspaceDescriptor};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use ui::{Tooltip, prelude::*};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::store::{ConnectionStatus, PaseoStore};
use crate::timeline::{format_relative, parse_timestamp};

enum WorktreesState {
    Loading,
    Loaded(Vec<PaseoWorktree>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorktreeRow {
    path: PathBuf,
    folder_name: String,
    branch: Option<String>,
    short_head: Option<String>,
    created: Option<String>,
    workspace_names: Vec<String>,
    agent_count: usize,
}

fn worktree_rows(
    worktrees: &[PaseoWorktree],
    workspaces: &BTreeMap<String, WorkspaceDescriptor>,
    agents: &[AgentSummary],
    now: DateTime<Utc>,
) -> Vec<WorktreeRow> {
    worktrees
        .iter()
        .map(|worktree| {
            let using_workspaces: Vec<&WorkspaceDescriptor> = workspaces
                .values()
                .filter(|workspace| workspace.directory == worktree.path)
                .collect();
            let agent_count = agents
                .iter()
                .filter(|agent| {
                    agent.extra["workspaceId"]
                        .as_str()
                        .is_some_and(|workspace_id| {
                            using_workspaces
                                .iter()
                                .any(|workspace| workspace.id == workspace_id)
                        })
                })
                .count();
            WorktreeRow {
                path: worktree.path.clone(),
                folder_name: worktree
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| worktree.path.to_string_lossy().into_owned()),
                branch: worktree.branch.clone(),
                short_head: worktree
                    .head
                    .as_ref()
                    .map(|head| head.chars().take(8).collect()),
                created: parse_timestamp(&worktree.created_at)
                    .map(|created_at| format_relative(created_at, now)),
                workspace_names: using_workspaces
                    .iter()
                    .map(|workspace| workspace.name.clone())
                    .collect(),
                agent_count,
            }
        })
        .collect()
}

/// What archiving does, per the daemon: its agents and workspace are archived, then the folder is
/// force-removed once no active workspace uses it, which discards uncommitted changes in it.
fn archive_detail(row: &WorktreeRow) -> String {
    let branch = match &row.branch {
        Some(branch) => format!("The branch {branch} is kept."),
        None => {
            "It is on no branch, so commits not on another branch can become unreachable.".into()
        }
    };
    format!(
        "Archives its {} and its workspace. Once no other workspace uses it, the folder {} is removed from disk, including uncommitted and untracked changes. {branch}",
        agents_text(row.agent_count),
        row.path.display(),
    )
}

fn agents_text(count: usize) -> String {
    if count == 1 {
        "1 agent".into()
    } else {
        format!("{count} agents")
    }
}

pub struct PaseoWorktreesView {
    store: Entity<PaseoStore>,
    project_id: String,
    state: WorktreesState,
    archiving: HashSet<PathBuf>,
    notice: Option<Result<String, String>>,
    focus_handle: FocusHandle,
    _store_subscription: Subscription,
}

impl PaseoWorktreesView {
    fn new(project_id: String, cx: &mut Context<Self>) -> Self {
        let store = crate::store(cx);
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let mut view = Self {
            store,
            project_id,
            state: WorktreesState::Loading,
            archiving: HashSet::new(),
            notice: None,
            focus_handle: cx.focus_handle(),
            _store_subscription: store_subscription,
        };
        view.refresh(cx);
        view
    }

    fn project_name(&self, cx: &App) -> Option<String> {
        let project = self.store.read(cx).state.projects.get(&self.project_id)?;
        Some(
            project
                .custom_name
                .clone()
                .unwrap_or_else(|| project.display_name.clone()),
        )
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root_path) = self
            .store
            .read(cx)
            .state
            .projects
            .get(&self.project_id)
            .map(|project| project.root_path.to_string_lossy().into_owned())
        else {
            self.state = WorktreesState::Failed("This project is not known to the host".into());
            cx.notify();
            return;
        };
        self.state = WorktreesState::Loading;
        cx.notify();
        let generation = self.store.read(cx).connection_generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move {
                session.paseo_worktrees(&root_path).await
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| {
                // A reply from before a host switch or reconnect belongs to the old host.
                if !view.store.read(cx).is_current_connection(generation) {
                    return;
                }
                view.state = match result {
                    Ok(worktrees) => WorktreesState::Loaded(worktrees),
                    Err(error) => WorktreesState::Failed(format!("{error:#}")),
                };
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn confirm_archive(&mut self, row: &WorktreeRow, window: &mut Window, cx: &mut Context<Self>) {
        let path = row.path.clone();
        let view = cx.weak_entity();
        crate::workspace_tools::confirm_then(
            "Archive this worktree?",
            &archive_detail(row),
            "Archive",
            move |cx| {
                if let Err(error) = view.update(cx, |view, cx| view.archive(path, cx)) {
                    log::debug!("Paseo worktrees tab closed: {error}");
                }
            },
            window,
            cx,
        );
    }

    fn archive(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.archiving.insert(path.clone()) {
            return;
        }
        self.notice = None;
        cx.notify();
        let path_text = path.to_string_lossy().into_owned();
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, |session| async move {
                session.archive_paseo_worktree(&path_text).await
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| {
                view.archiving.remove(&path);
                match result {
                    Ok(archived_agents) => {
                        if let WorktreesState::Loaded(worktrees) = &mut view.state {
                            worktrees.retain(|worktree| worktree.path != path);
                        }
                        view.notice = Some(Ok(format!(
                            "Archived {}",
                            agents_text(archived_agents.len())
                        )));
                        view.refresh(cx);
                    }
                    Err(error) => view.notice = Some(Err(format!("{error:#}"))),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_message(message: impl Into<SharedString>) -> AnyElement {
        Label::new(message.into())
            .color(Color::Muted)
            .into_any_element()
    }

    fn render_error(title: &str, error: String, cx: &App) -> AnyElement {
        v_flex()
            .p_4()
            .gap_2()
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().status().error_border)
            .bg(cx.theme().status().error_background)
            .child(Label::new(title.to_owned()).weight(FontWeight::SEMIBOLD))
            .child(Label::new(error).size(LabelSize::Small))
            .into_any_element()
    }

    fn render_row(&self, index: usize, row: WorktreeRow, cx: &mut Context<Self>) -> AnyElement {
        let archiving = self.archiving.contains(&row.path);
        let mut metadata = vec![row.branch.clone().unwrap_or_else(|| "detached".into())];
        metadata.extend(row.short_head.clone());
        metadata.extend(
            row.created
                .as_ref()
                .map(|created| format!("created {created}")),
        );
        let usage = if row.workspace_names.is_empty() {
            "No workspace".to_owned()
        } else {
            format!(
                "{} · {}",
                row.workspace_names.join(", "),
                agents_text(row.agent_count)
            )
        };
        let full_path: SharedString = row.path.to_string_lossy().into_owned().into();
        h_flex()
            .id(("paseo-worktree", index))
            .p_3()
            .gap_2()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .tooltip(Tooltip::text(full_path))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(row.folder_name.clone()).truncate())
                    .child(
                        Label::new(metadata.join(" · "))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(usage).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                Button::new(
                    ("paseo-worktree-archive", index),
                    if archiving { "Archiving…" } else { "Archive" },
                )
                .disabled(archiving)
                .tooltip(Tooltip::text(
                    "Archive this worktree's agents and remove it from disk",
                ))
                .on_click(
                    cx.listener(move |view, _, window, cx| view.confirm_archive(&row, window, cx)),
                ),
            )
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for PaseoWorktreesView {}

impl Focusable for PaseoWorktreesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PaseoWorktreesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.store.read(cx).status == ConnectionStatus::Connected;
        let project_name = self.project_name(cx);
        let loading = matches!(self.state, WorktreesState::Loading);
        let body = if project_name.is_none() {
            Self::render_message("This project is not known to the host")
        } else if !connected {
            Self::render_message("Connect to this host to see its worktrees")
        } else {
            match &self.state {
                WorktreesState::Loading => crate::render_loading("Loading worktrees…"),
                WorktreesState::Failed(error) => {
                    Self::render_error("Unable to load worktrees", error.clone(), cx)
                }
                WorktreesState::Loaded(worktrees) if worktrees.is_empty() => {
                    Self::render_message("No Paseo worktrees")
                }
                WorktreesState::Loaded(worktrees) => {
                    let store = self.store.read(cx);
                    let rows = worktree_rows(
                        worktrees,
                        &store.state.workspaces,
                        &store.state.agents,
                        Utc::now(),
                    );
                    v_flex()
                        .gap_2()
                        .children(
                            rows.into_iter()
                                .enumerate()
                                .map(|(index, row)| self.render_row(index, row, cx)),
                        )
                        .into_any_element()
                }
            }
        };
        let notice = self.notice.clone().map(|notice| match notice {
            Ok(message) => Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Success)
                .into_any_element(),
            Err(error) => Self::render_error("Unable to archive the worktree", error, cx),
        });
        div()
            .id("paseo-worktrees")
            .key_context("PaseoWorktrees")
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
                                            Headline::new("Paseo worktrees")
                                                .size(HeadlineSize::Small),
                                        )
                                        .child(
                                            Label::new(project_name.unwrap_or_default())
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    Button::new(
                                        "paseo-worktrees-refresh",
                                        if loading { "Refreshing…" } else { "Refresh" },
                                    )
                                    .start_icon(Icon::new(IconName::RotateCw).size(IconSize::Small))
                                    .disabled(loading || !connected)
                                    .tooltip(Tooltip::text("List this project's worktrees again"))
                                    .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
                                ),
                        )
                        .children(notice)
                        .child(body),
                ),
            )
    }
}

impl Item for PaseoWorktreesView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.project_name(cx) {
            Some(name) => format!("Paseo Worktrees · {name}").into(),
            None => "Paseo Worktrees".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch))
    }
}

/// Opens the Paseo worktrees tab for a project, reusing an open one for the same project.
pub(crate) fn open_worktrees(
    workspace: &mut Workspace,
    project_id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<PaseoWorktreesView>(cx)
        .find(|view| view.read(cx).project_id == project_id);
    if let Some(existing) = existing {
        existing.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project_id = project_id.to_owned();
    let view = cx.new(|cx| PaseoWorktreesView::new(project_id, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(id: &str, name: &str, directory: &str) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            id: id.into(),
            project_id: "project".into(),
            project_display_name: "zaseo".into(),
            project_root_path: PathBuf::from("/repo"),
            directory: PathBuf::from(directory),
            kind: "worktree".into(),
            worktree_slug: None,
            name: name.into(),
            title: None,
            pinned_at: None,
            labels: Vec::new(),
            status: "done".into(),
            activity_at: None,
            diff_stat: None,
            scripts: Vec::new(),
            current_branch: None,
            is_paseo_worktree: true,
            extra: json!({}),
        }
    }

    fn agent(id: &str, workspace_id: &str) -> AgentSummary {
        AgentSummary {
            id: id.into(),
            title: None,
            status: "idle".into(),
            directory: None,
            project: None,
            extra: json!({"workspaceId": workspace_id}),
        }
    }

    #[test]
    fn worktree_rows_name_branch_and_age() {
        let now = parse_timestamp("2026-09-28T12:00:00Z").expect("time");
        let worktrees = vec![
            PaseoWorktree {
                path: PathBuf::from("/worktrees/zaseo/bright-fox"),
                created_at: "2026-09-28T10:00:00Z".into(),
                branch: Some("feature/fox".into()),
                head: Some("0123456789abcdef".into()),
            },
            PaseoWorktree {
                path: PathBuf::from("/worktrees/zaseo/quiet-owl"),
                created_at: String::new(),
                branch: None,
                head: None,
            },
        ];
        let workspaces = BTreeMap::from([
            (
                "w1".to_owned(),
                workspace("w1", "Fox work", "/worktrees/zaseo/bright-fox"),
            ),
            ("w2".to_owned(), workspace("w2", "Root", "/repo")),
        ]);
        let agents = vec![agent("a1", "w1"), agent("a2", "w1"), agent("a3", "w2")];

        let rows = worktree_rows(&worktrees, &workspaces, &agents, now);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].folder_name, "bright-fox");
        assert_eq!(rows[0].branch.as_deref(), Some("feature/fox"));
        assert_eq!(rows[0].short_head.as_deref(), Some("01234567"));
        assert_eq!(
            rows[0].created,
            Some(format_relative(
                parse_timestamp("2026-09-28T10:00:00Z").expect("time"),
                now
            ))
        );
        assert_eq!(rows[0].workspace_names, vec!["Fox work".to_owned()]);
        assert_eq!(rows[0].agent_count, 2);
        assert_eq!(rows[1].folder_name, "quiet-owl");
        assert_eq!(rows[1].branch, None);
        assert_eq!(rows[1].short_head, None);
        assert_eq!(rows[1].created, None);
        assert!(rows[1].workspace_names.is_empty());
        assert_eq!(rows[1].agent_count, 0);
    }
}
