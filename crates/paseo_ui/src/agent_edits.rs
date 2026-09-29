use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use buffer_diff::DiffHunkStatus;
use buffer_diff::{BufferDiff, RestoreDiffOperations};
use editor::{
    Anchor, DiffHunkRenderer, Editor,
    actions::{GoToHunk, GoToPreviousHunk},
};
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Context, Entity, EntityId, EventEmitter,
    Focusable, KeyContext, Pixels, Subscription, Task, WeakEntity, Window, actions, prelude::*,
};
use language::Point;
use language::{Buffer, BufferEvent, ToOffset as _};
use project::Project;
use serde_json::Value;
use ui::{Button, IconButton, KeyBinding, Tooltip, prelude::*};
use workspace::{ItemHandle, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

use crate::store;
use crate::store::{PaseoStore, StoreEvent, agent_project_directory};
use crate::timeline::{FileEdit, StreamContent, ToolStatus, project_items, reverse_edits_tracking};

actions!(
    paseo_ui,
    [
        /// Keeps the Paseo agent edits under the cursor.
        KeepEdit,
        /// Rejects the Paseo agent edits under the cursor, restoring the text before them.
        RejectEdit,
        /// Keeps every Paseo agent edit in this file.
        KeepAllEdits,
        /// Rejects every Paseo agent edit in this file.
        RejectAllEdits,
    ]
);

const REFRESH_DELAY: Duration = Duration::from_millis(250);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_entity = cx.entity();
        let project = workspace.project().clone();
        let tracker = cx.new(|cx| AgentEdits::new(workspace_entity, project, window, cx));
        // The workspace's action handlers keep the tracker alive exactly as long as the workspace.
        workspace.register_action({
            let tracker = tracker.clone();
            move |_, _: &KeepAllEdits, window, cx| {
                tracker.update(cx, |tracker, cx| {
                    tracker.review_focused(true, true, window, cx)
                })
            }
        });
        workspace.register_action(move |_, _: &RejectAllEdits, window, cx| {
            tracker.update(cx, |tracker, cx| {
                tracker.review_focused(false, true, window, cx)
            })
        });
    })
    .detach();
}

/// A timeline edit's identity across restarts: its agent, the timeline epoch (sequences restart
/// when a timeline is replaced), the tool call item, and the edit's position within the call.
fn edit_key(agent_id: &str, epoch: &str, item_key: u64, index: usize) -> String {
    format!("{agent_id}|{epoch}|{item_key}|{index}")
}

/// The unreviewed edits of one file from every agent, oldest first.
type FileEdits = Vec<(String, FileEdit)>;

/// Completed edit and write tool calls from `agents`, by absolute path, leaving out edits the user
/// already kept or rejected.
fn unreviewed_edits(
    store: &PaseoStore,
    agents: &[(String, Option<PathBuf>)],
) -> BTreeMap<PathBuf, FileEdits> {
    let mut files: BTreeMap<
        PathBuf,
        Vec<(Option<chrono::DateTime<chrono::Utc>>, String, FileEdit)>,
    > = BTreeMap::new();
    for (agent_id, directory) in agents {
        let epoch = store.state.current_epoch(agent_id).unwrap_or_default();
        for item in project_items(store.entries_for(agent_id)) {
            let StreamContent::Tool(call) = &item.content else {
                continue;
            };
            if call.status != ToolStatus::Completed {
                continue;
            }
            let Some(path) = call
                .detail
                .get("filePath")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
            else {
                continue;
            };
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                match directory {
                    Some(directory) => directory.join(path),
                    None => continue,
                }
            };
            let Some(edits) = crate::timeline::tool_call_edits(&call.detail) else {
                continue;
            };
            for (index, edit) in edits.into_iter().enumerate() {
                // What a whole-file write replaced is unknown: showing it would mark the whole
                // file as new, and rejecting it would empty the file.
                if edit.whole_file {
                    continue;
                }
                let key = edit_key(agent_id, epoch, item.key, index);
                if store.reviewed_edits.contains(&key) {
                    continue;
                }
                files
                    .entry(path.clone())
                    .or_default()
                    .push((item.timestamp, key, edit));
            }
        }
    }
    files
        .into_iter()
        .map(|(path, mut edits)| {
            // Stable, so edits from one call stay in call order.
            edits.sort_by_key(|(timestamp, _, _)| *timestamp);
            let edits = edits
                .into_iter()
                .map(|(_, key, edit)| (key, edit))
                .collect();
            (path, edits)
        })
        .collect()
}

/// Edits of the last refresh that are gone now or turned up somewhere else.
fn lost_edits(
    previous: &[(String, Range<text::Anchor>)],
    located: &[(String, Range<usize>)],
    edits: &FileEdits,
    snapshot: &text::BufferSnapshot,
) -> Vec<String> {
    previous
        .iter()
        .filter(|(key, _)| edits.iter().any(|(edit_key, _)| edit_key == key))
        .filter(|(key, anchors)| {
            let before = anchors.start.to_offset(snapshot)..anchors.end.to_offset(snapshot);
            match located.iter().find(|(found, _)| found == key) {
                Some((_, now)) => !touches(&before, now),
                None => true,
            }
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// A range widened to the whole lines it covers.
fn whole_lines(buffer: &text::BufferSnapshot, range: Range<usize>) -> Range<usize> {
    let start = buffer.offset_to_point(range.start);
    let end = buffer.offset_to_point(range.end);
    let start = buffer.point_to_offset(Point::new(start.row, 0));
    let end = buffer.point_to_offset(Point::new(end.row, buffer.line_len(end.row)));
    start..end
}

/// Empties a file's agent diff after its pending updates.
fn empty_diff(file: TrackedFile, cx: &mut Context<AgentEdits>) -> Task<()> {
    let snapshot = file.buffer.read(cx).text_snapshot();
    let text: Arc<str> = snapshot.text().into();
    let (diff, previous) = (file.diff, file.diff_update);
    cx.spawn(async move |_, cx| {
        previous.await;
        let update = diff.update(cx, |diff, cx| diff.set_base_text(Some(text), snapshot, cx));
        update.await;
    })
}

/// Whether an edit's range in the text touches a hunk's range. Both ends count, so an empty range
/// from a deletion matches the hunk at its position.
fn touches(edit: &Range<usize>, hunk: &Range<usize>) -> bool {
    edit.start <= hunk.end && hunk.start <= edit.end
}

struct TrackedFile {
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    /// Where each unreviewed edit sits in the buffer, anchored so later typing moves them along.
    located: Vec<(String, Range<text::Anchor>)>,
    has_hunks: bool,
    /// The base and buffer version of the last diff update, to skip repeating it.
    applied: Option<(Arc<str>, clock::Global)>,
    /// The latest diff update; each one waits for the one before, because a diff can't take a
    /// new base while computing another.
    diff_update: Task<()>,
    _buffer_subscription: Subscription,
}

struct AttachedEditor {
    editor: WeakEntity<Editor>,
    path: PathBuf,
    _subscriptions: Vec<gpui::Subscription>,
}

/// Highlights Paseo agents' unreviewed edits in this workspace's editors, the way Zed shows its own
/// agent's edits, rebuilding what each file looked like before them from the agents' timelines.
pub(crate) struct AgentEdits {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<PaseoStore>,
    files: HashMap<PathBuf, TrackedFile>,
    opening: HashSet<PathBuf>,
    watched: HashSet<String>,
    editors: HashMap<EntityId, AttachedEditor>,
    /// Set while a refresh waits to run: streaming agents and typing send events faster than the
    /// delay, so new ones join the waiting refresh instead of pushing it back.
    refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentEdits {
    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = store(cx);
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| this.schedule_refresh(cx)),
            cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                if let StoreEvent::TimelineChanged(timeline_id) = event
                    && this.watched.contains(timeline_id)
                {
                    this.schedule_refresh(cx);
                }
            }),
            cx.subscribe_in(
                &workspace,
                window,
                |this, _, event: &workspace::Event, _, cx| {
                    if matches!(event, workspace::Event::ItemAdded { .. }) {
                        this.schedule_refresh(cx);
                    }
                },
            ),
            cx.on_release(|this, cx| {
                let watched = std::mem::take(&mut this.watched);
                this.store.update(cx, |store, cx| {
                    for agent_id in &watched {
                        store.unwatch(agent_id, cx);
                    }
                });
            }),
        ];
        let mut this = Self {
            workspace: workspace.downgrade(),
            project,
            store,
            files: HashMap::new(),
            opening: HashSet::new(),
            watched: HashSet::new(),
            editors: HashMap::new(),
            refresh_task: None,
            _subscriptions: subscriptions,
        };
        this.schedule_refresh(cx);
        this
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            return;
        }
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DELAY).await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.refresh(cx)
            }) {
                log::debug!("Paseo agent edits released: {error}");
            }
        }));
    }

    /// Agents working in this project, with their directories. Only a local project and agents on
    /// this machine are matched: paths alone can't tell two machines' files apart.
    fn project_agents(&self, cx: &App) -> Vec<(String, Option<PathBuf>)> {
        let store = self.store.read(cx);
        let project = self.project.read(cx);
        if !store.is_local_host() || !project.is_local() {
            return Vec::new();
        }
        let roots = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect::<Vec<_>>();
        store
            .state
            .agents
            .iter()
            .filter(|agent| {
                agent_project_directory(agent)
                    .into_iter()
                    .chain(agent.directory.clone())
                    .any(|directory| roots.iter().any(|root| directory.starts_with(root)))
            })
            .map(|agent| (agent.id.clone(), agent.directory.clone()))
            .collect()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let agents = self.project_agents(cx);
        let agent_ids = agents
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect::<HashSet<_>>();
        let (added, removed): (Vec<_>, Vec<_>) = (
            agent_ids.difference(&self.watched).cloned().collect(),
            self.watched.difference(&agent_ids).cloned().collect(),
        );
        if !added.is_empty() || !removed.is_empty() {
            self.store.update(cx, |store, cx| {
                for agent_id in &added {
                    store.watch(agent_id, cx);
                }
                for agent_id in &removed {
                    store.unwatch(agent_id, cx);
                }
            });
            self.watched = agent_ids;
        }
        let edits = unreviewed_edits(self.store.read(cx), &agents);
        let stale = self
            .files
            .keys()
            .filter(|path| !edits.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in stale {
            if let Some(file) = self.files.remove(&path) {
                // Emptied rather than dropped: an editor keeps showing a diff until another
                // replaces it, and a file outside git gets none.
                empty_diff(file, cx).detach();
            }
        }
        for (path, file_edits) in edits {
            if self.files.contains_key(&path) {
                self.update_file(&path, &file_edits, cx);
            } else {
                self.open_file(path, file_edits, cx);
            }
        }
        self.sync_editors(cx);
    }

    fn open_file(&mut self, path: PathBuf, edits: FileEdits, cx: &mut Context<Self>) {
        let Some(project_path) = self.project.read(cx).find_project_path(&path, cx) else {
            return;
        };
        if !self.opening.insert(path.clone()) {
            return;
        }
        let open = self
            .project
            .update(cx, |project, cx| project.open_buffer(project_path, cx));
        cx.spawn(async move |this, cx| {
            let buffer = open.await;
            this.update(cx, |this, cx| {
                this.opening.remove(&path);
                let buffer = match buffer {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        log::debug!(
                            "Paseo agent edits could not open {}: {error:#}",
                            path.display()
                        );
                        return;
                    }
                };
                let diff = cx.new(|cx| {
                    let buffer = buffer.read(cx);
                    let mut diff = BufferDiff::new(
                        &buffer.text_snapshot(),
                        buffer.language().cloned(),
                        buffer.language_registry(),
                        cx,
                    );
                    diff.set_operations(Arc::new(RestoreDiffOperations));
                    diff
                });
                let subscription = cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
                    if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                        this.schedule_refresh(cx);
                    }
                });
                this.files.insert(
                    path.clone(),
                    TrackedFile {
                        buffer,
                        diff,
                        located: Vec::new(),
                        has_hunks: false,
                        applied: None,
                        diff_update: Task::ready(()),
                        _buffer_subscription: subscription,
                    },
                );
                this.update_file(&path, &edits, cx);
                this.sync_editors(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    /// Rebuilds the text before the file's unreviewed edits and diffs the buffer against it.
    fn update_file(&mut self, path: &Path, edits: &FileEdits, cx: &mut Context<Self>) {
        let Some(file) = self.files.get_mut(path) else {
            return;
        };
        let snapshot = file.buffer.read(cx).text_snapshot();
        let text = snapshot.text();
        let (base, located) = reverse_edits_tracking(&text, edits);
        let lost = lost_edits(&file.located, &located, edits, &snapshot);
        if !lost.is_empty() {
            // An edit that vanished or moved was undone or rewritten; left in, it could later
            // match identical text elsewhere and be rejected there.
            self.store
                .update(cx, |store, cx| store.mark_edits_reviewed(lost, cx));
            return;
        }
        file.located = located
            .into_iter()
            .map(|(key, range)| {
                (
                    key,
                    snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
                )
            })
            .collect();
        let has_hunks = base != text;
        let changed = has_hunks != file.has_hunks;
        file.has_hunks = has_hunks;
        let version = snapshot.version().clone();
        let unchanged = file
            .applied
            .as_ref()
            .is_some_and(|(applied, applied_version)| {
                applied.as_ref() == base && *applied_version == version
            });
        if !unchanged {
            let base: Arc<str> = base.into();
            file.applied = Some((base.clone(), version));
            let previous = std::mem::replace(&mut file.diff_update, Task::ready(()));
            let diff = file.diff.clone();
            file.diff_update = cx.spawn(async move |_, cx| {
                previous.await;
                let update =
                    diff.update(cx, |diff, cx| diff.set_base_text(Some(base), snapshot, cx));
                update.await;
            });
        }
        if changed {
            self.sync_editors(cx);
        }
    }

    /// Attaches the agent diff to open editors of files with unreviewed edits, and gives the
    /// others back their git diff.
    fn sync_editors(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open_editors = workspace
            .read(cx)
            .items_of_type::<Editor>(cx)
            .collect::<Vec<_>>();
        let mut live = HashSet::new();
        for editor in open_editors {
            let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
                continue;
            };
            let tracked = self
                .files
                .iter()
                .find(|(_, file)| file.has_hunks && file.buffer == buffer)
                .map(|(path, file)| (path.clone(), file.diff.clone()));
            let editor_id = editor.entity_id();
            match tracked {
                Some((path, diff)) => {
                    live.insert(editor_id);
                    self.attach(&editor, &buffer, path, diff, cx);
                }
                None => {
                    if self.editors.contains_key(&editor_id) {
                        self.detach(&editor, &buffer, cx);
                    }
                }
            }
        }
        self.editors.retain(|editor_id, attached| {
            live.contains(editor_id) || attached.editor.upgrade().is_some()
        });
    }

    fn attach(
        &mut self,
        editor: &Entity<Editor>,
        buffer: &Entity<Buffer>,
        path: PathBuf,
        diff: Entity<BufferDiff>,
        cx: &mut Context<Self>,
    ) {
        let buffer_id = buffer.read(cx).remote_id();
        let shows_ours = editor
            .read(cx)
            .buffer()
            .read(cx)
            .diff_for(buffer_id)
            .is_some_and(|shown| shown == diff);
        if !shows_ours {
            // The editor also loads its git diff when it opens, which would replace this one.
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .update(cx, |multi_buffer, cx| multi_buffer.add_diff(diff, cx));
            });
        }
        if self.editors.contains_key(&editor.entity_id()) {
            return;
        }
        let tracker = cx.weak_entity();
        let renderer: Arc<dyn DiffHunkRenderer> = Arc::new(AgentEditHunkRenderer {
            tracker: tracker.clone(),
        });
        let subscriptions = editor.update(cx, |editor, cx| {
            editor.set_diff_hunk_renderer(Some(renderer), cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.register_addon(AgentEditsAddon {
                tracker: tracker.clone(),
            });
            let editor_handle = cx.entity().downgrade();
            let keep = {
                let (tracker, editor_handle) = (tracker.clone(), editor_handle.clone());
                editor.register_action(move |_: &KeepEdit, window, cx| {
                    review_at_cursor(&tracker, &editor_handle, true, window, cx)
                })
            };
            let reject = {
                let tracker = tracker.clone();
                editor.register_action(move |_: &RejectEdit, window, cx| {
                    review_at_cursor(&tracker, &editor_handle, false, window, cx)
                })
            };
            vec![keep, reject]
        });
        self.editors.insert(
            editor.entity_id(),
            AttachedEditor {
                editor: editor.downgrade(),
                path,
                _subscriptions: subscriptions,
            },
        );
    }

    fn detach(&mut self, editor: &Entity<Editor>, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        self.editors.remove(&editor.entity_id());
        editor.update(cx, |editor, cx| {
            editor.set_diff_hunk_renderer(None, cx);
            editor.unregister_addon::<AgentEditsAddon>();
            editor.buffer().update(cx, |multi_buffer, cx| {
                multi_buffer.set_all_diff_hunks_collapsed(cx)
            });
        });
        let git_diff = self.project.update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        });
        let editor = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let git_diff = match git_diff.await {
                Ok(git_diff) => git_diff,
                Err(error) => {
                    log::debug!("No git diff to restore after agent edits: {error:#}");
                    return anyhow::Ok(());
                }
            };
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .update(cx, |multi_buffer, cx| multi_buffer.add_diff(git_diff, cx));
            })
        })
        .detach_and_log_err(cx);
    }

    fn path_for_editor(&self, editor: &Entity<Editor>) -> Option<PathBuf> {
        self.editors
            .get(&editor.entity_id())
            .map(|attached| attached.path.clone())
    }

    /// How many unreviewed edits the editor's file has.
    fn edit_count(&self, editor: &Entity<Editor>) -> usize {
        self.path_for_editor(editor)
            .and_then(|path| self.files.get(&path))
            .map_or(0, |file| file.located.len())
    }

    /// Keeps or rejects the edits touching `hunk_ranges` in the editor's file, or all of them.
    fn review(
        &mut self,
        editor: &Entity<Editor>,
        hunk_ranges: Option<Vec<Range<Anchor>>>,
        keep: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .path_for_editor(editor)
            .and_then(|path| self.files.get(&path))
        else {
            return;
        };
        let buffer = file.buffer.read(cx).snapshot();
        // Diff hunks cover whole lines, while an edit can start mid-line.
        let line_ranges = hunk_ranges.as_ref().map(|ranges| {
            let multi_buffer = editor.read(cx).buffer().read(cx);
            ranges
                .iter()
                .filter_map(|range| {
                    let (start_buffer, start) =
                        multi_buffer.text_anchor_for_position(range.start, cx)?;
                    let (end_buffer, end) = multi_buffer.text_anchor_for_position(range.end, cx)?;
                    (start_buffer == file.buffer && end_buffer == file.buffer).then(|| {
                        whole_lines(&buffer, start.to_offset(&buffer)..end.to_offset(&buffer))
                    })
                })
                .collect::<Vec<_>>()
        });
        let keys = file
            .located
            .iter()
            .filter(|(_, anchors)| {
                let edit = anchors.start.to_offset(&buffer)..anchors.end.to_offset(&buffer);
                line_ranges
                    .as_ref()
                    .is_none_or(|ranges| ranges.iter().any(|lines| touches(&edit, lines)))
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if !keep {
            let ranges = hunk_ranges.unwrap_or_else(|| vec![Anchor::Min..Anchor::Max]);
            editor.update(cx, |editor, cx| {
                editor.restore_diff_hunks_in_ranges(ranges, window, cx)
            });
        }
        self.store
            .update(cx, |store, cx| store.mark_edits_reviewed(keys, cx));
    }

    /// Keeps or rejects in the focused editor: the hunks under its selections, or all.
    fn review_focused(
        &mut self,
        keep: bool,
        all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .editors
            .values()
            .filter_map(|attached| attached.editor.upgrade())
            .find(|editor| editor.focus_handle(cx).contains_focused(window, cx))
        else {
            return;
        };
        let ranges = (!all).then(|| selected_ranges(&editor, cx));
        self.review(&editor, ranges, keep, window, cx);
    }
}

fn selected_ranges(editor: &Entity<Editor>, cx: &mut App) -> Vec<Range<Anchor>> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor
            .selections
            .all::<Point>(&editor.display_snapshot(cx))
            .into_iter()
            .map(|selection| {
                snapshot.anchor_before(selection.start)..snapshot.anchor_after(selection.end)
            })
            .collect()
    })
}

fn review_at_cursor(
    tracker: &WeakEntity<AgentEdits>,
    editor: &WeakEntity<Editor>,
    keep: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    let ranges = selected_ranges(&editor, cx);
    if let Err(error) = tracker.update(cx, |tracker, cx| {
        tracker.review(&editor, Some(ranges), keep, window, cx)
    }) {
        log::debug!("Paseo agent edits released: {error}");
    }
}

/// Marks an editor showing agent edits, for the Keep and Reject key bindings and the toolbar.
struct AgentEditsAddon {
    tracker: WeakEntity<AgentEdits>,
}

impl editor::Addon for AgentEditsAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("paseo_agent_edits");
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct AgentEditHunkRenderer {
    tracker: WeakEntity<AgentEdits>,
}

impl DiffHunkRenderer for AgentEditHunkRenderer {
    fn render_hunk_controls(
        &self,
        row: u32,
        _status: &DiffHunkStatus,
        hunk_range: Range<Anchor>,
        _is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let focus_handle = editor.read(cx).focus_handle(cx);
        let button = |label: &'static str, keep: bool, cx: &App| {
            let (tracker, editor) = (self.tracker.clone(), editor.downgrade());
            let range = hunk_range.clone();
            let action: Box<dyn gpui::Action> = if keep {
                KeepEdit.boxed_clone()
            } else {
                RejectEdit.boxed_clone()
            };
            Button::new((label, row as u64), label)
                .key_binding(
                    KeyBinding::for_action_in(action.as_ref(), &focus_handle, cx)
                        .map(|binding| binding.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    let Some(editor) = editor.upgrade() else {
                        return;
                    };
                    let ranges = vec![range.clone()];
                    if let Err(error) = tracker.update(cx, |tracker, cx| {
                        tracker.review(&editor, Some(ranges), keep, window, cx)
                    }) {
                        log::debug!("Paseo agent edits released: {error}");
                    }
                })
        };
        h_flex()
            .h(line_height)
            .mr_0p5()
            .gap_1()
            .px_0p5()
            .pb_1()
            .border_x_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .rounded_b_md()
            .bg(cx.theme().colors().editor_background)
            .block_mouse_except_scroll()
            .child(button("Reject", false, cx))
            .child(button("Keep", true, cx))
            .into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }
}

/// The pane toolbar for an editor showing agent edits: their count, Keep All and Reject All, and
/// hunk navigation.
pub struct AgentEditsToolbar {
    editor: Option<WeakEntity<Editor>>,
    _subscription: Option<Subscription>,
}

impl AgentEditsToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            editor: None,
            _subscription: None,
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for AgentEditsToolbar {}

impl ToolbarItemView for AgentEditsToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) else {
            self.editor = None;
            self._subscription = None;
            return ToolbarItemLocation::Hidden;
        };
        // Agent edits can attach after the editor becomes active, so the toolbar follows it and
        // draws nothing until they do.
        self._subscription = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.editor = Some(editor.downgrade());
        ToolbarItemLocation::PrimaryRight
    }
}

impl Render for AgentEditsToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) else {
            return div().into_any_element();
        };
        let Some(tracker) = editor
            .read(cx)
            .addon::<AgentEditsAddon>()
            .and_then(|addon| addon.tracker.upgrade())
        else {
            return div().into_any_element();
        };
        let count = tracker.read(cx).edit_count(&editor);
        if count == 0 {
            return div().into_any_element();
        }
        let focus_handle = editor.focus_handle(cx);
        let navigation = |id: &'static str,
                          icon: IconName,
                          tooltip: &'static str,
                          action: Box<dyn gpui::Action>| {
            let tooltip_focus = focus_handle.clone();
            let click_focus = focus_handle.clone();
            let tooltip_action = action.boxed_clone();
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .tooltip(move |_window, cx| {
                    Tooltip::for_action_in(tooltip, tooltip_action.as_ref(), &tooltip_focus, cx)
                })
                .on_click(move |_, window, cx| {
                    click_focus.dispatch_action(action.as_ref(), window, cx)
                })
        };
        let review = |id: &'static str, label: &'static str, action: Box<dyn gpui::Action>| {
            let focus_handle = focus_handle.clone();
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .key_binding(
                    KeyBinding::for_action_in(action.as_ref(), &focus_handle, cx)
                        .map(|binding| binding.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.focus(&focus_handle, cx);
                    window.dispatch_action(action.boxed_clone(), cx)
                })
        };
        h_flex()
            .gap_1()
            .child(
                Label::new(if count == 1 {
                    "1 agent edit".to_owned()
                } else {
                    format!("{count} agent edits")
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(navigation(
                "paseo-previous-edit",
                IconName::ArrowUp,
                "Previous Hunk",
                GoToPreviousHunk.boxed_clone(),
            ))
            .child(navigation(
                "paseo-next-edit",
                IconName::ArrowDown,
                "Next Hunk",
                GoToHunk.boxed_clone(),
            ))
            .child(review(
                "paseo-reject-all-edits",
                "Reject All",
                RejectAllEdits.boxed_clone(),
            ))
            .child(review(
                "paseo-keep-all-edits",
                "Keep All",
                KeepAllEdits.boxed_clone(),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_entry(
        agent_id: &str,
        sequence: u64,
        timestamp: &str,
        old: &str,
        new: &str,
    ) -> paseo_client::TimelineEntry {
        paseo_client::TimelineEntry {
            agent_id: agent_id.into(),
            epoch: "epoch".into(),
            sequence,
            timestamp: timestamp.into(),
            payload: paseo_client::TimelinePayload::Tool(serde_json::json!({
                "type": "tool_call",
                "callId": format!("call-{agent_id}-{sequence}"),
                "name": "edit",
                "status": "completed",
                "detail": {"type": "edit", "filePath": "src/main.rs", "oldString": old, "newString": new},
                "error": null,
            })),
            extra: serde_json::json!({}),
        }
    }

    #[test]
    fn two_agents_merge_into_one_file_in_time_order() {
        let mut store = PaseoStore::default();
        store
            .state
            .insert_entry(edit_entry("second", 1, "2026-09-28T10:05:00Z", "b", "B"));
        store
            .state
            .insert_entry(edit_entry("first", 1, "2026-09-28T10:00:00Z", "a", "A"));
        let agents = [
            ("first".to_owned(), Some(PathBuf::from("/repo"))),
            ("second".to_owned(), Some(PathBuf::from("/repo"))),
        ];
        let files = unreviewed_edits(&store, &agents);
        let edits = &files[Path::new("/repo/src/main.rs")];
        assert_eq!(
            edits
                .iter()
                .map(|(_, edit)| edit.new_text.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"],
            "relative paths resolve against each agent's directory, and edits keep time order"
        );

        store.reviewed_edits.insert(edits[0].0.clone());
        let files = unreviewed_edits(&store, &agents);
        assert_eq!(
            files[Path::new("/repo/src/main.rs")]
                .iter()
                .map(|(_, edit)| edit.new_text.as_str())
                .collect::<Vec<_>>(),
            ["B"],
            "a kept edit stays out"
        );
    }

    #[test]
    fn whole_file_writes_are_left_out() {
        let mut store = PaseoStore::default();
        let mut write = edit_entry("agent", 1, "2026-09-28T10:00:00Z", "", "");
        write.payload = paseo_client::TimelinePayload::Tool(serde_json::json!({
            "type": "tool_call",
            "callId": "call-write",
            "name": "write",
            "status": "completed",
            "detail": {"type": "write", "filePath": "/repo/new.rs", "content": "fn main() {}\n"},
            "error": null,
        }));
        store.state.insert_entry(write);
        let files = unreviewed_edits(
            &store,
            &[("agent".to_owned(), Some(PathBuf::from("/repo")))],
        );
        assert!(
            files.is_empty(),
            "what a write replaced is unknown, so rejecting it could only empty the file"
        );
    }

    #[test]
    fn edits_touching_a_hunk_include_deletions_at_its_edges() {
        assert!(touches(&(10..20), &(15..30)));
        assert!(
            touches(&(10..10), &(10..12)),
            "a deletion at the hunk's start"
        );
        assert!(
            touches(&(12..12), &(10..12)),
            "a deletion at the hunk's end"
        );
        assert!(!touches(&(0..5), &(10..12)));
    }
}
