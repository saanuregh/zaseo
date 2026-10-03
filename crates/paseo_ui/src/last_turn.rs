use anyhow::{Context as _, Result, anyhow};
use buffer_diff::{BufferDiff, RestoreDiffOperations};
use editor::{
    Editor, EditorEvent, HiddenUnstagedDiffHunkRenderer, MultiBuffer, multibuffer_context_lines,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, SharedString, Subscription, Task, Window,
};
use language::{
    Buffer, BufferEvent, Capability, DiskState, LineEnding, OffsetRangeExt as _, ReplicaId, Rope,
    TextBuffer,
};
use multi_buffer::PathKey;
use project::{Project, WorktreeId};
use std::any::{Any, TypeId};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ui::prelude::*;
use util::paths::PathStyle;
use util::rel_path::RelPath;
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

use crate::store::{ConnectionStatus, PaseoStore, StoreEvent, agent_is_running};
use crate::timeline::{
    self, StreamContent, Turn, TurnFileEdits, group_turns, latest_finished_turn, project_items,
    reverse_edits, snippet_texts, turn_edits,
};

const RECALCULATE_DIFF_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
enum LastTurnEdits {
    Ready(Vec<TurnFileEdits>),
    /// The loaded timeline page starts partway through the turn, or before it finished.
    NeedsOlderHistory,
}

fn last_finished_turn_edits(store: &PaseoStore, agent_id: &str) -> LastTurnEdits {
    let items = project_items(store.entries_for(agent_id));
    let running = store.agent(agent_id).is_some_and(agent_is_running);
    let turns = group_turns(&items);
    let has_older = store
        .paging
        .get(agent_id)
        .is_some_and(|paging| paging.has_older);
    // Turns after the first start at a user message, so only the first loaded turn can be cut
    // off by the page boundary.
    let complete = |turn: &Turn| {
        turn.items.start > 0
            || items
                .first()
                .is_some_and(|item| matches!(item.content, StreamContent::User { .. }))
    };
    match latest_finished_turn(turns.len(), running).and_then(|index| turns.get(index)) {
        Some(turn) if complete(turn) || !has_older => LastTurnEdits::Ready(
            items
                .get(turn.items.clone())
                .map(turn_edits)
                .unwrap_or_default(),
        ),
        None if !has_older => LastTurnEdits::Ready(Vec::new()),
        _ => LastTurnEdits::NeedsOlderHistory,
    }
}

/// What the shown edits were collected from: the timeline's revision and whether the agent ran.
fn timeline_signature(store: &PaseoStore, agent_id: &str) -> (u64, bool) {
    (
        store.state.timeline_revision(agent_id),
        store.agent(agent_id).is_some_and(agent_is_running),
    )
}

enum ViewState {
    Loading,
    Ready,
    Failed(SharedString),
}

/// A file as shown in the tab: its current text as the buffer and its text from before the turn
/// as the diff base.
struct LoadedFile {
    display_path: Arc<RelPath>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    base_text: Arc<str>,
    editable: bool,
}

/// The files an agent's latest finished turn changed, as a multibuffer diff against their text
/// from before the turn.
pub struct LastTurnView {
    store: Entity<PaseoStore>,
    project: Entity<Project>,
    agent_id: String,
    editor: Entity<Editor>,
    multibuffer: Entity<MultiBuffer>,
    shown_edits: Option<Vec<TurnFileEdits>>,
    /// Timeline revision and running state when the edits were last collected; the store
    /// notifies on every streamed chunk of any agent, and collecting walks the whole timeline.
    /// The revision also changes when entries are replaced in place.
    timeline_signature: Option<(u64, bool)>,
    state: ViewState,
    file_count: usize,
    load_task: Option<Task<()>>,
    diff_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LastTurnView {
    fn new(
        store: Entity<PaseoStore>,
        agent_id: String,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
            editor.disable_diagnostics(cx);
            editor.set_expand_all_diff_hunks(cx);
            editor
        });
        let subscriptions = vec![
            cx.observe_in(&store, window, |view, _, window, cx| {
                view.store_changed(window, cx)
            }),
            cx.subscribe_in(&store, window, |view, _, event: &StoreEvent, window, cx| {
                if let StoreEvent::TimelineChanged(timeline_id) = event
                    && *timeline_id == view.agent_id
                {
                    view.store_changed(window, cx)
                }
            }),
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                if event == &(EditorEvent::SelectionsChanged { local: true }) {
                    cx.emit(event.clone())
                }
            }),
        ];
        let mut view = Self {
            store,
            project,
            agent_id,
            editor,
            multibuffer,
            shown_edits: None,
            timeline_signature: None,
            state: ViewState::Loading,
            file_count: 0,
            load_task: None,
            diff_tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        view.store_changed(window, cx);
        view
    }

    fn store_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if store.status != ConnectionStatus::Connected {
            if self.shown_edits.is_none() {
                self.state =
                    ViewState::Failed("Connect to a Paseo host to review the last turn".into());
                cx.notify();
            }
            return;
        }
        let signature = timeline_signature(store, &self.agent_id);
        let running = signature.1;
        let was_running = self
            .timeline_signature
            .is_some_and(|(_, was_running)| was_running);
        if self.timeline_signature == Some(signature) {
            return;
        }
        self.timeline_signature = Some(signature);
        // Chunks of a running turn change the timeline, but not the turn before it.
        if running && was_running && self.shown_edits.is_some() {
            return;
        }
        match last_finished_turn_edits(store, &self.agent_id) {
            LastTurnEdits::Ready(edits) => {
                if self.shown_edits.as_ref() != Some(&edits) {
                    self.load(edits, window, cx);
                }
            }
            LastTurnEdits::NeedsOlderHistory => {
                let agent_id = self.agent_id.clone();
                self.store
                    .update(cx, |store, cx| store.load_older(&agent_id, cx));
            }
        }
    }

    fn load(&mut self, edits: Vec<TurnFileEdits>, window: &mut Window, cx: &mut Context<Self>) {
        self.shown_edits = Some(edits.clone());
        self.state = ViewState::Loading;
        let directory = self.store.read(cx).timeline_directory(&self.agent_id);
        let project = self.project.clone();
        let store = self.store.clone();
        self.load_task = Some(cx.spawn_in(window, async move |view, cx| {
            let mut files = Vec::with_capacity(edits.len());
            let mut first_error = None;
            for file in &edits {
                match load_file(file, directory.as_deref(), &project, &store, cx).await {
                    Ok(loaded) => files.push(loaded),
                    Err(error) => {
                        log::warn!("Paseo last turn skipped {}: {error:#}", file.path);
                        first_error.get_or_insert(error);
                    }
                }
            }
            if let Err(error) = view.update(cx, |view, cx| {
                // A turn whose every file failed would otherwise read as one that changed nothing.
                match first_error.filter(|_| files.is_empty()) {
                    Some(error) => {
                        view.state = ViewState::Failed(
                            format!("Could not load the last turn: {error:#}").into(),
                        );
                        cx.notify();
                    }
                    None => view.show(files, cx),
                }
            }) {
                log::debug!("Paseo last turn tab closed while loading: {error}");
            }
        }));
        cx.notify();
    }

    fn show(&mut self, files: Vec<LoadedFile>, cx: &mut Context<Self>) {
        let context_lines = multibuffer_context_lines(cx);
        self.diff_tasks.clear();
        self.multibuffer
            .update(cx, |multibuffer, cx| multibuffer.clear(cx));
        let mut file_count = 0;
        for (index, file) in files.into_iter().enumerate() {
            let snapshot = file.buffer.read(cx).snapshot();
            let ranges: Vec<_> = file
                .diff
                .read(cx)
                .snapshot(cx)
                .hunks(&snapshot)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect();
            if ranges.is_empty() {
                continue;
            }
            file_count += 1;
            let path_key = PathKey::with_sort_prefix(index as u64, file.display_path.clone());
            self.multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.set_excerpts_for_path(
                    path_key,
                    file.buffer.clone(),
                    ranges,
                    context_lines,
                    cx,
                );
                multibuffer.add_diff(file.diff.clone(), cx);
            });
            if file.editable {
                self.diff_tasks.push(follow_buffer_edits(file, cx));
            }
        }
        self.file_count = file_count;
        self.state = ViewState::Ready;
        cx.emit(EditorEvent::TitleChanged);
        cx.notify();
    }

    fn title(&self, cx: &App) -> SharedString {
        let store = self.store.read(cx);
        let agent_title = store
            .agent(&self.agent_id)
            .filter(|agent| {
                agent
                    .title
                    .as_deref()
                    .is_some_and(|title| !title.is_empty())
            })
            .map(|agent| store.display_title(agent));
        match agent_title {
            Some(agent_title) => format!("Last turn · {agent_title}").into(),
            None => "Last turn".into(),
        }
    }

    fn centered(content: AnyElement, cx: &App) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(content)
            .into_any_element()
    }
}

/// Keeps a project buffer's diff current as the user edits or reverts hunks, one recalculation at
/// a time because `BufferDiff::set_base_text` must not run concurrently.
fn follow_buffer_edits(file: LoadedFile, cx: &mut Context<LastTurnView>) -> Task<()> {
    let (edited_sender, edited_receiver) = async_channel::bounded::<()>(1);
    let subscription = cx.subscribe(&file.buffer, move |_, _, event: &BufferEvent, _| {
        if matches!(event, BufferEvent::Edited { .. }) {
            // A full channel already has a recalculation queued.
            edited_sender.try_send(()).ok();
        }
    });
    cx.spawn(async move |_, cx| {
        let _subscription = subscription;
        while edited_receiver.recv().await.is_ok() {
            cx.background_executor()
                .timer(RECALCULATE_DIFF_DEBOUNCE)
                .await;
            let snapshot = file
                .buffer
                .read_with(cx, |buffer, _| buffer.text_snapshot());
            let recalculated = file.diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(file.base_text.clone()), snapshot, cx)
            });
            recalculated.await;
        }
    })
}

async fn load_file(
    file: &TurnFileEdits,
    directory: Option<&Path>,
    project: &Entity<Project>,
    store: &Entity<PaseoStore>,
    cx: &mut AsyncApp,
) -> Result<LoadedFile> {
    let absolute_path = match Path::new(&file.path) {
        path if path.is_absolute() => path.to_path_buf(),
        path => directory
            .map(|directory| directory.join(path))
            .with_context(|| format!("{} is relative and the agent has no directory", file.path))?,
    };
    let display_path = display_path(&timeline::relative_path(&file.path, directory));
    let current = open_current(&absolute_path, &display_path, project, store, cx).await;
    let base = match &current {
        Ok((buffer, _)) => {
            let text = buffer.read_with(cx, |buffer, _| buffer.text());
            reverse_edits(&text, &file.edits)
        }
        Err(error) => {
            log::debug!("Paseo last turn could not read {}: {error:#}", file.path);
            None
        }
    };
    let (buffer, base_text, editable) = match (current, base) {
        (Ok((buffer, editable)), Some(base)) => (buffer, base, editable),
        _ => {
            let (base, snippets) = snippet_texts(&file.edits);
            let buffer =
                snapshot_buffer(snippets, &absolute_path, &display_path, project, cx).await;
            (buffer, base, false)
        }
    };
    let base_text: Arc<str> = base_text.into();
    let diff = build_diff(&buffer, base_text.clone(), cx).await;
    Ok(LoadedFile {
        display_path,
        buffer,
        diff,
        base_text,
        editable,
    })
}

/// The file's current buffer and whether it can be edited: the project's own buffer when the
/// project holds the file, otherwise a read-only copy read through the daemon.
async fn open_current(
    absolute_path: &Path,
    display_path: &Arc<RelPath>,
    project: &Entity<Project>,
    store: &Entity<PaseoStore>,
    cx: &mut AsyncApp,
) -> Result<(Entity<Buffer>, bool)> {
    // A remote agent's paths can also exist in a local project, and diffing or reverting the local
    // copy would be wrong, so a local project serves only agents on this machine.
    let is_local_host = store.read_with(cx, |store, _| store.is_local_host());
    let project_path = project.read_with(cx, |project, cx| {
        (is_local_host || project.is_via_remote_server())
            .then(|| project.find_project_path(absolute_path, cx))
            .flatten()
    });
    if let Some(project_path) = project_path {
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;
        return Ok((buffer, true));
    }
    let path = absolute_path
        .to_str()
        .ok_or_else(|| anyhow!("{} is not valid UTF-8", absolute_path.display()))?
        .to_owned();
    let content = store
        .update(cx, |store, cx| {
            store.session_request(
                cx,
                move |session| async move { session.read_file(&path).await },
            )
        })
        .await?;
    let text = String::from_utf8(content.bytes).context("the file is not UTF-8 text")?;
    let buffer = snapshot_buffer(text, absolute_path, display_path, project, cx).await;
    Ok((buffer, false))
}

/// A read-only buffer named after the file, so the multibuffer header shows its path and the
/// text gets the file's syntax highlighting.
async fn snapshot_buffer(
    mut text: String,
    absolute_path: &Path,
    display_path: &Arc<RelPath>,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Entity<Buffer> {
    let registry = project.read_with(cx, |project, _| project.languages().clone());
    let language = registry
        .load_language_for_file_path(absolute_path)
        .await
        .ok();
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let file = Arc::new(SnapshotFile {
        path: display_path.clone(),
        full_path: absolute_path.to_path_buf(),
    });
    cx.new(|cx| {
        let text_buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            Rope::from(text),
        );
        let mut buffer = Buffer::build(text_buffer, Some(file), Capability::ReadOnly, cx);
        buffer.set_language_registry(registry);
        buffer.set_language(language, cx);
        buffer
    })
}

async fn build_diff(
    buffer: &Entity<Buffer>,
    base_text: Arc<str>,
    cx: &mut AsyncApp,
) -> Entity<BufferDiff> {
    let (snapshot, language, registry) = buffer.read_with(cx, |buffer, _| {
        (
            buffer.text_snapshot(),
            buffer.language().cloned(),
            buffer.language_registry(),
        )
    });
    let diff = cx.new(|cx| {
        let mut diff = BufferDiff::new(&snapshot, language, registry, cx);
        diff.set_operations(Arc::new(RestoreDiffOperations));
        diff
    });
    diff.update(cx, |diff, cx| {
        diff.set_base_text(Some(base_text), snapshot, cx)
    })
    .await;
    diff
}

fn display_path(path: &str) -> Arc<RelPath> {
    RelPath::new(Path::new(path.trim_start_matches('/')), PathStyle::Unix)
        .map(|path| path.into_owned().into())
        .unwrap_or_else(|_| RelPath::empty_arc())
}

/// Names a buffer that is not backed by a project file: a file read through the daemon, or the
/// edit snippets of a file that changed after the turn.
struct SnapshotFile {
    path: Arc<RelPath>,
    full_path: PathBuf,
}

impl language::File for SnapshotFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic { was_deleted: false }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::Unix
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.full_path.clone()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.path.file_name().unwrap_or_default()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        WorktreeId::from_usize(usize::MAX)
    }

    fn to_proto(&self, _: &App) -> language::proto::File {
        language::proto::File {
            worktree_id: WorktreeId::from_usize(usize::MAX).to_proto(),
            entry_id: None,
            path: self.path.as_unix_str().to_owned(),
            mtime: None,
            is_deleted: false,
            is_historic: true,
        }
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        false
    }
}

impl EventEmitter<EditorEvent> for LastTurnView {}

impl Focusable for LastTurnView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for LastTurnView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            ViewState::Loading => {
                Self::centered(crate::render_loading("Loading the last turn…"), cx)
            }
            // Only a failed file load can be retried; a disconnected host reloads on reconnect.
            ViewState::Failed(error) if self.shown_edits.is_some() => Self::centered(
                crate::render_error(
                    "Unable to load the last turn",
                    error.clone(),
                    Some(
                        h_flex()
                            .child(
                                Button::new("paseo-last-turn-retry", "Try again")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        if let Some(edits) = view.shown_edits.clone() {
                                            view.load(edits, window, cx);
                                        }
                                    })),
                            )
                            .into_any_element(),
                    ),
                    cx,
                ),
                cx,
            ),
            ViewState::Failed(error) => Self::centered(
                crate::render_error("Unable to load the last turn", error.clone(), None, cx),
                cx,
            ),
            ViewState::Ready if self.file_count == 0 => Self::centered(
                crate::render_message("The last turn changed no files", None, None),
                cx,
            ),
            ViewState::Ready => self.editor.clone().into_any_element(),
        }
    }
}

impl Item for LastTurnView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDiff).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let files = if self.file_count == 1 {
            "1 file".to_owned()
        } else {
            format!("{} files", self.file_count)
        };
        Some(format!("{} ({files})", self.title(cx)).into())
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.title(cx)
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(
        &self,
        cx: &App,
    ) -> Option<(Vec<language::HighlightedText>, Option<gpui::Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

/// Opens the current agent's latest finished turn as a diff tab, reusing its open tab.
pub fn open_last_turn(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some((store, agent_id)) = crate::current_agent(workspace, cx) else {
        workspace.show_error(anyhow!("Open a Paseo agent to review its last turn"), cx);
        return;
    };
    let existing = workspace.items_of_type::<LastTurnView>(cx).find(|view| {
        let view = view.read(cx);
        view.agent_id == agent_id && view.store == store
    });
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let view = cx.new(|cx| LastTurnView::new(store, agent_id, project, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::FileEdit;
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;

    fn string_edit(old_text: &str, new_text: &str) -> FileEdit {
        FileEdit {
            old_text: old_text.into(),
            new_text: new_text.into(),
            line_hint: None,
            whole_file: false,
        }
    }

    async fn project_with_file(cx: &mut TestAppContext, text: &str) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "main.rs": text })).await;
        Project::test(fs, [Path::new("/project")], cx).await
    }

    async fn load(
        project: &Entity<Project>,
        path: &str,
        edits: Vec<FileEdit>,
        cx: &mut TestAppContext,
    ) -> LoadedFile {
        let store = cx.new(|_| {
            let mut store = PaseoStore::default();
            store.active_profile = Some(settings::PaseoConnectionProfile {
                name: "Local".into(),
                target_uri: "ws://127.0.0.1:6767/ws".into(),
                editor_ssh_uri: None,
                client_id: "test-client".into(),
            });
            store
        });
        let file = TurnFileEdits {
            path: path.into(),
            edits,
        };
        let mut async_cx = cx.to_async();
        let loaded = load_file(
            &file,
            Some(Path::new("/project")),
            project,
            &store,
            &mut async_cx,
        )
        .await;
        cx.run_until_parked();
        loaded.expect("file loads")
    }

    fn hunk_count(file: &LoadedFile, cx: &mut TestAppContext) -> usize {
        cx.update(|cx| {
            let snapshot = file.buffer.read(cx).snapshot();
            file.diff.read(cx).snapshot(cx).hunks(&snapshot).count()
        })
    }

    fn timeline_entry(sequence: u64, item: serde_json::Value) -> paseo_client::TimelineEntry {
        let payload = match item.get("type").and_then(serde_json::Value::as_str) {
            Some("tool_call") => paseo_client::TimelinePayload::Tool(item),
            _ => paseo_client::TimelinePayload::Message(item),
        };
        paseo_client::TimelineEntry {
            agent_id: "agent".into(),
            epoch: "epoch".into(),
            sequence,
            timestamp: "2026-09-28T00:00:00Z".into(),
            payload,
            extra: json!({}),
        }
    }

    fn edit_entry(sequence: u64) -> paseo_client::TimelineEntry {
        timeline_entry(
            sequence,
            json!({"type":"tool_call","callId":format!("call-{sequence}"),"name":"edit","status":"completed","detail":{"type":"edit","filePath":"/project/main.rs","oldString":"one","newString":"two"},"error":null}),
        )
    }

    #[test]
    fn last_turn_asks_for_older_history_when_the_page_starts_mid_turn() {
        let mut store = PaseoStore::default();
        store.state.insert_entry(edit_entry(5));
        store.paging.entry("agent".into()).or_default().has_older = true;
        assert_eq!(
            last_finished_turn_edits(&store, "agent"),
            LastTurnEdits::NeedsOlderHistory
        );

        store.state.insert_entry(timeline_entry(
            4,
            json!({"type":"user_message","text":"Fix it"}),
        ));
        let LastTurnEdits::Ready(edits) = last_finished_turn_edits(&store, "agent") else {
            panic!("a turn that starts with the user's message is complete");
        };
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "/project/main.rs");
    }

    #[test]
    fn last_turn_notices_an_entry_replaced_in_place() {
        let mut store = PaseoStore::default();
        store.state.insert_entry(edit_entry(5));
        let before = timeline_signature(&store, "agent");
        store.state.insert_entry(timeline_entry(
            5,
            json!({"type":"user_message","text":"Replaced"}),
        ));
        assert_eq!(store.entries_for("agent").count(), 1);
        assert_ne!(timeline_signature(&store, "agent"), before);
    }

    #[test]
    fn last_turn_uses_a_partial_page_when_no_older_history_exists() {
        let mut store = PaseoStore::default();
        store.state.insert_entry(edit_entry(5));
        assert!(matches!(
            last_finished_turn_edits(&store, "agent"),
            LastTurnEdits::Ready(edits) if edits.len() == 1
        ));
    }

    #[gpui::test]
    async fn last_turn_diffs_a_project_file_against_its_text_before_the_turn(
        cx: &mut TestAppContext,
    ) {
        let project = project_with_file(cx, "fn main() {\n    two();\n}\n").await;
        let file = load(
            &project,
            "/project/main.rs",
            vec![string_edit("one();", "two();")],
            cx,
        )
        .await;
        assert!(file.editable);
        assert_eq!(&*file.base_text, "fn main() {\n    one();\n}\n");
        assert_eq!(file.display_path.as_unix_str(), "main.rs");
        assert_eq!(hunk_count(&file, cx), 1);
    }

    #[gpui::test]
    async fn last_turn_shows_snippets_when_the_file_changed_after_the_turn(
        cx: &mut TestAppContext,
    ) {
        let project = project_with_file(cx, "fn main() {}\n").await;
        let file = load(
            &project,
            "/project/main.rs",
            vec![string_edit("one();", "two();")],
            cx,
        )
        .await;
        assert!(!file.editable);
        assert_eq!(&*file.base_text, "one();\n");
        cx.update(|cx| assert_eq!(file.buffer.read(cx).text(), "two();\n"));
        assert_eq!(hunk_count(&file, cx), 1);
    }

    #[gpui::test]
    async fn last_turn_shows_snippets_for_files_it_cannot_read(cx: &mut TestAppContext) {
        let project = project_with_file(cx, "").await;
        let file = load(
            &project,
            "/elsewhere/lib.rs",
            vec![string_edit("old", "new")],
            cx,
        )
        .await;
        assert!(!file.editable);
        assert_eq!(file.display_path.as_unix_str(), "elsewhere/lib.rs");
        assert_eq!(hunk_count(&file, cx), 1);
    }
}
