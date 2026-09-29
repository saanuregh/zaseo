use chrono::Utc;
use gpui::{
    Action as _, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, ListAlignment, ListState, Pixels, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window, list, prelude::*, px,
};
use gpui::{Image, ImageFormat, ImageSource, Resource, SharedUri};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use paseo_client::{PermissionRequest, PermissionResponse};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{
    CommonAnimationExt, ContextMenu, IconButton, Indicator, PopoverMenu, Tooltip, WithScrollbar,
    prelude::*, utils::WithRemSize,
};
use workspace::{
    Item, ItemId, SerializableItem, Workspace, WorkspaceId,
    item::{ItemEvent, TabContentParams},
};

use crate::composer::{Composer, ComposerEvent};
use crate::store::{
    AgentBucket, PaseoStore, StoreEvent, agent_branch, agent_bucket, agent_is_running,
    agent_project_name, agent_provider, agent_title, agent_turn_started_at, subagent_bucket,
    subagent_title,
};
use crate::timeline::{
    FileChange, StreamItem, Turn, group_turns, parse_timestamp, project_items, turn_changes,
};
use crate::{
    ArchiveAgent, CopyAgentId, FocusComposer, RenameAgent, ScrollToBottom, connection_picker,
    open_draft, store,
};

pub(crate) const CONTENT_MAX_WIDTH: f32 = 920.;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Row {
    LoadOlder {
        loading: bool,
    },
    Item {
        index: usize,
        item: StreamItem,
        expanded: bool,
        streaming: bool,
    },
    TurnFooter {
        turn: usize,
        duration_seconds: Option<i64>,
        finished_at: Option<chrono::DateTime<Utc>>,
    },
    Working {
        since: Option<chrono::DateTime<Utc>>,
    },
    /// The files the latest finished turn changed, like waku's changed-files card.
    Changes {
        files: Vec<FileChange>,
        expanded: Vec<bool>,
        show_all: bool,
    },
    Spacer,
}

pub struct AgentView {
    pub(crate) store: Entity<PaseoStore>,
    pub(crate) agent_id: Option<String>,
    pub(crate) composer: Entity<Composer>,
    focus_handle: FocusHandle,
    pub(crate) list_state: ListState,
    pub(crate) rows: Vec<Row>,
    pub(crate) items: Vec<StreamItem>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) markdown: HashMap<(u64, u8), Entity<Markdown>>,
    pub(crate) expanded: HashSet<u64>,
    pub(crate) expanded_changes: HashSet<String>,
    pub(crate) show_all_changes: bool,
    subagents_expanded: bool,
    /// The agent's directory, whose daemon terminals this view keeps listed.
    terminal_directory: Option<String>,
    pub(crate) workspace: Option<WeakEntity<Workspace>>,
    question_answers: HashMap<(String, usize), Vec<String>>,
    ticker: Option<Task<()>>,
    title: SharedString,
    bucket: Option<AgentBucket>,
    /// Images agents link by path, fetched from the daemon's host once each and read by the
    /// markdown image resolver, which can't fetch on its own.
    images: Rc<RefCell<HashMap<String, Arc<Image>>>>,
    requested_images: HashSet<String>,
    code_spans: Rc<RefCell<HashMap<String, Option<SharedString>>>>,
    /// The chat font size of the last render; row heights are remeasured when it changes.
    font_size: Pixels,
    /// Messages parse in the background, so a newly loaded conversation stays hidden until its
    /// rows have their text; otherwise they grow and jump when it arrives.
    reveal: Reveal,
    last_rebuild_inputs: Option<RebuildInputs>,
    /// Markdown created but not yet parsed once, which renders with no height.
    unparsed_markdown: HashSet<(u64, u8)>,
    subagent_scroll: ScrollHandle,
    /// Per permission request, so each preview keeps its own scroll position.
    permission_scrolls: RefCell<HashMap<String, ScrollHandle>>,
    _subscriptions: Vec<Subscription>,
}

/// What `AgentView::rebuild` reads from the store, compared to skip rebuilds for other agents.
#[derive(PartialEq)]
struct RebuildInputs {
    generation: u64,
    agent: Option<paseo_client::AgentSummary>,
    subagent: Option<paseo_client::ProviderSubagent>,
    epoch: Option<String>,
    entries: usize,
    has_older: bool,
    loading_older: bool,
    has_permission: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reveal {
    Pending,
    Parsing(Instant),
    Shown,
}

const REVEAL_WAIT_FOR_SUBAGENTS: Duration = Duration::from_millis(400);

pub enum AgentViewEvent {
    TabChanged,
}

impl EventEmitter<AgentViewEvent> for AgentView {}

impl AgentView {
    pub fn new(
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = store(cx);
        let composer =
            cx.new(|cx| Composer::new(store.clone(), agent_id.clone(), directory, window, cx));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(2048.));
        list_state.set_follow_mode(gpui::FollowMode::Tail);
        let mut subscriptions = vec![
            cx.observe(&store, |view: &mut Self, _, cx| {
                view.rebuild_if_inputs_changed(cx)
            }),
            cx.subscribe(&store, |view: &mut Self, _, event: &StoreEvent, cx| {
                if let StoreEvent::TimelineChanged(timeline_id) = event
                    && view.agent_id.as_deref() == Some(timeline_id.as_str())
                {
                    view.rebuild(cx);
                }
            }),
            cx.subscribe_in(&composer, window, Self::handle_composer_event),
        ];
        let focus_handle = cx.focus_handle();
        subscriptions.push(
            cx.on_focus_in(&focus_handle, window, |view: &mut Self, _, cx| {
                view.mark_focused(cx);
            }),
        );
        if let Some(agent_id) = agent_id.clone() {
            store.update(cx, |store, cx| store.watch(&agent_id, cx));
        }
        cx.on_release({
            let store = store.clone();
            let agent_id = agent_id.clone();
            move |view: &mut Self, cx| {
                let agent_id = view.agent_id.clone().or(agent_id);
                if let Some(agent_id) = agent_id {
                    store.update(cx, |store, cx| store.unwatch(&agent_id, cx));
                }
                if let Some(directory) = view.terminal_directory.take() {
                    store.update(cx, |store, cx| store.unwatch_terminals(&directory, cx));
                }
            }
        })
        .detach();
        {
            let this = cx.weak_entity();
            list_state.set_scroll_handler(move |event, _window, cx| {
                if event.visible_range.start == 0 {
                    if let Err(error) = this.update(cx, |view, cx| view.load_older(cx)) {
                        log::debug!("Paseo agent view released: {error}");
                    }
                }
                if let Err(error) = this.update(cx, |_, cx| cx.notify()) {
                    log::debug!("Paseo agent view released: {error}");
                }
            });
        }
        let mut view = Self {
            store,
            agent_id,
            composer,
            focus_handle,
            list_state,
            rows: Vec::new(),
            items: Vec::new(),
            turns: Vec::new(),
            markdown: HashMap::new(),
            images: Rc::default(),
            requested_images: HashSet::new(),
            code_spans: Rc::default(),
            font_size: crate::chat_font_size(cx),
            reveal: Reveal::Pending,
            last_rebuild_inputs: None,
            unparsed_markdown: HashSet::new(),
            subagent_scroll: ScrollHandle::new(),
            permission_scrolls: RefCell::default(),
            expanded: HashSet::new(),
            expanded_changes: HashSet::new(),
            show_all_changes: false,
            subagents_expanded: false,
            terminal_directory: None,
            workspace,
            question_answers: HashMap::new(),
            ticker: None,
            title: "New agent".into(),
            bucket: None,
            _subscriptions: subscriptions,
        };
        view.rebuild(cx);
        view
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Subagent tabs show a provider subagent's conversation, which can't be messaged.
    pub(crate) fn is_subagent(&self) -> bool {
        self.agent_id
            .as_deref()
            .and_then(paseo_client::parse_subagent_timeline_id)
            .is_some()
    }

    pub(crate) fn open_subagent(
        &mut self,
        parent_agent_id: &str,
        subagent_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let parent_agent_id = parent_agent_id.to_owned();
        let subagent_id = subagent_id.to_owned();
        // Opening reads every agent tab, including this one, which is mid-update here.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                crate::open_subagent(workspace, &parent_agent_id, &subagent_id, window, cx)
            });
        });
    }

    pub(crate) fn directory(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .timeline_directory(self.agent_id.as_deref()?)
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        let handle = self.input_focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Where typing goes: the composer, or the view itself in a subagent tab, which renders no
    /// composer.
    fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        if self.is_subagent() {
            self.focus_handle.clone()
        } else {
            self.composer.focus_handle(cx)
        }
    }

    pub(crate) fn mark_focused(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.agent_id.clone() else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.set_focused_agent(agent_id.clone(), cx);
            store.clear_attention(&agent_id, cx);
        });
    }

    fn handle_composer_event(
        &mut self,
        _composer: &Entity<Composer>,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::AgentCreated(agent_id) => {
                self.agent_id = Some(agent_id.clone());
                self.store.update(cx, |store, cx| {
                    store.watch(agent_id, cx);
                    store.set_focused_agent(agent_id.clone(), cx);
                });
                self.rebuild(cx);
                if let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
                    let view = cx.entity();
                    let agent_id = agent_id.clone();
                    // Deferred because the workspace may be mid-update while the composer emits.
                    window.defer(cx, move |window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            let tab = workspace
                                .items_of_type::<AgentTab>(cx)
                                .find(|tab| tab.read(cx).view == view);
                            if let Some(tab) = tab {
                                crate::follow_created_agent(workspace, tab, &agent_id, window, cx);
                            }
                        });
                    });
                }
            }
            ComposerEvent::ClearRequested { directory } => {
                let directory = directory.clone();
                if let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
                    workspace.update(cx, |workspace, cx| {
                        open_draft(workspace, directory, window, cx);
                    });
                }
            }
            ComposerEvent::Submitted => {
                self.list_state.scroll_to_end();
                cx.notify();
            }
        }
    }

    pub(crate) fn agent<'a>(&self, cx: &'a App) -> Option<&'a paseo_client::AgentSummary> {
        self.agent_id
            .as_deref()
            .and_then(|agent_id| self.store.read(cx).agent(agent_id))
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.agent_id.clone() {
            self.store
                .update(cx, |store, cx| store.load_older(&agent_id, cx));
        }
    }

    pub(crate) fn toggle_expanded(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.rebuild(cx);
    }

    /// Recomputes the display rows from the store and splices only the rows that changed, so the
    /// list keeps measured heights and its tail-follow position while chunks stream in.
    /// The store notifies for every agent's changes; rebuilding re-projects the whole timeline,
    /// so skip it when nothing this view reads has changed.
    fn rebuild_if_inputs_changed(&mut self, cx: &mut Context<Self>) {
        if self.rebuild_inputs(cx) != self.last_rebuild_inputs {
            self.rebuild(cx);
        }
    }

    fn rebuild_inputs(&self, cx: &App) -> Option<RebuildInputs> {
        let agent_id = self.agent_id.as_deref()?;
        let store = self.store.read(cx);
        let paging = store.paging.get(agent_id);
        Some(RebuildInputs {
            generation: store.connection_generation,
            agent: store.agent(agent_id).cloned(),
            subagent: store.subagent(agent_id).cloned(),
            epoch: store.state.current_epoch(agent_id).map(str::to_owned),
            entries: store.entries_for(agent_id).count(),
            has_older: paging.is_some_and(|paging| paging.has_older),
            loading_older: paging.is_some_and(|paging| paging.loading_older),
            has_permission: store
                .state
                .permissions
                .values()
                .any(|request| request.agent_id == agent_id),
        })
    }

    pub(crate) fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.last_rebuild_inputs = self.rebuild_inputs(cx);
        let (items, running, turn_started, has_older, loading_older, title, bucket) = {
            let store = self.store.read(cx);
            let Some(agent_id) = self.agent_id.as_deref() else {
                self.apply_rows(Vec::new(), cx);
                return;
            };
            let items = project_items(store.entries_for(agent_id));
            let agent = store.agent(agent_id);
            let subagent = store.subagent(agent_id);
            let paging = store.paging.get(agent_id);
            let has_permission = store
                .state
                .permissions
                .values()
                .any(|request| request.agent_id == *agent_id);
            (
                items,
                agent.is_some_and(agent_is_running)
                    || subagent.is_some_and(|subagent| subagent.status == "running"),
                agent.and_then(agent_turn_started_at).or_else(|| {
                    subagent.and_then(|subagent| parse_timestamp(&subagent.created_at))
                }),
                paging.is_some_and(|paging| paging.has_older),
                paging.is_some_and(|paging| paging.loading_older),
                agent
                    .map(agent_title)
                    .or_else(|| subagent.map(subagent_title)),
                agent
                    .map(|agent| agent_bucket(agent, has_permission))
                    .or_else(|| subagent.map(subagent_bucket)),
            )
        };
        self.sync_terminal_directory(cx);
        if let Some(title) = title {
            let title = SharedString::from(title);
            if title != self.title || bucket != self.bucket {
                self.title = title;
                self.bucket = bucket;
                cx.emit(AgentViewEvent::TabChanged);
            }
        }
        let turns = group_turns(&items);
        let mut rows = Vec::with_capacity(items.len() + turns.len() + 2);
        if has_older {
            rows.push(Row::LoadOlder {
                loading: loading_older,
            });
        }
        let last_turn = turns.len().saturating_sub(1);
        let last_finished_turn = crate::timeline::latest_finished_turn(turns.len(), running);
        for (turn_index, turn) in turns.iter().enumerate() {
            let live_turn = running && turn_index == last_turn;
            for index in turn.items.clone() {
                let Some(item) = items.get(index) else {
                    continue;
                };
                let streaming = live_turn && index + 1 == turn.items.end;
                rows.push(Row::Item {
                    index,
                    expanded: crate::stream::is_expandable(item)
                        && self.expanded.contains(&item.key),
                    item: item.clone(),
                    streaming,
                });
            }
            if Some(turn_index) == last_finished_turn {
                let files = items
                    .get(turn.items.clone())
                    .map(turn_changes)
                    .unwrap_or_default();
                if !files.is_empty() {
                    rows.push(Row::Changes {
                        expanded: files
                            .iter()
                            .map(|file| self.expanded_changes.contains(&file.path))
                            .collect(),
                        files,
                        show_all: self.show_all_changes,
                    });
                }
            }
            if !live_turn {
                let duration_seconds = turn
                    .started_at
                    .zip(turn.ended_at)
                    .map(|(start, end)| (end - start).num_seconds())
                    .filter(|seconds| *seconds > 0);
                rows.push(Row::TurnFooter {
                    turn: turn_index,
                    duration_seconds,
                    finished_at: turn.ended_at,
                });
            }
        }
        if running {
            rows.push(Row::Working {
                since: turn_started.or_else(|| turns.last().and_then(|turn| turn.started_at)),
            });
        }
        rows.push(Row::Spacer);
        self.items = items;
        self.turns = turns;
        self.apply_rows(rows, cx);
        self.update_ticker(running, cx);
    }

    fn sync_terminal_directory(&mut self, cx: &mut Context<Self>) {
        let directory = self
            .agent(cx)
            .and_then(|agent| agent.directory.as_ref())
            .and_then(|directory| directory.to_str())
            .map(str::to_owned);
        if directory.is_none() || directory == self.terminal_directory {
            return;
        }
        let previous = std::mem::replace(&mut self.terminal_directory, directory.clone());
        self.store.update(cx, |store, cx| {
            if let Some(previous) = previous {
                store.unwatch_terminals(&previous, cx);
            }
            if let Some(directory) = directory {
                store.watch_terminals(&directory, cx);
            }
        });
    }

    fn apply_rows(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        let first_changed = self
            .rows
            .iter()
            .zip(rows.iter())
            .position(|(old, new)| old != new)
            .unwrap_or(self.rows.len().min(rows.len()));
        if first_changed < self.rows.len() || rows.len() != self.rows.len() {
            self.list_state
                .splice(first_changed..self.rows.len(), rows.len() - first_changed);
            self.rows = rows;
        }
        let live_keys = self
            .items
            .iter()
            .map(|item| item.key)
            .collect::<HashSet<_>>();
        self.markdown.retain(|(key, _), _| live_keys.contains(key));
        cx.notify();
    }

    fn update_ticker(&mut self, running: bool, cx: &mut Context<Self>) {
        if !running {
            self.ticker = None;
            return;
        }
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update(cx, |view, cx| {
                        if let Some(index) = view
                            .rows
                            .iter()
                            .position(|row| matches!(row, Row::Working { .. }))
                        {
                            view.list_state.splice(index..index + 1, 1);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub(crate) fn markdown_for(
        &mut self,
        key: u64,
        role: u8,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        if let Some(markdown) = self.markdown.get(&(key, role)) {
            let source = markdown.read(cx).source();
            if source.as_ref() != text {
                let delta = text
                    .strip_prefix(source.as_ref())
                    .filter(|_| !source.is_empty());
                markdown.update(cx, |markdown, cx| match delta {
                    Some(delta) => markdown.append(delta, cx),
                    None => markdown.reset(text.to_owned().into(), cx),
                });
            }
            return markdown.clone();
        }
        let markdown = cx.new(|cx| Markdown::new(text.to_owned().into(), None, None, cx));
        self.markdown.insert((key, role), markdown.clone());
        self.unparsed_markdown.insert((key, role));
        self.load_images(text, cx);
        markdown
    }

    /// A markdown element that shows the images this view fetched for its links.
    pub(crate) fn markdown_element(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
    ) -> MarkdownElement {
        let images = self.images.clone();
        let links = ThreadLinks {
            store: self.store.clone(),
            agent_id: self.agent_id.clone(),
            workspace: self.workspace.clone(),
            code_spans: self.code_spans.clone(),
        };
        MarkdownElement::new(markdown, style)
            .image_resolver(move |destination, _| {
                if destination.starts_with("http://") || destination.starts_with("https://") {
                    return Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
                        destination.to_owned(),
                    ))));
                }
                images
                    .borrow()
                    .get(destination)
                    .map(|image| ImageSource::Image(image.clone()))
            })
            .on_url_click({
                let links = links.clone();
                move |url, window, cx| links.open(&url, window, cx)
            })
            .on_code_span_link(move |text, cx| links.code_span_link(text, cx))
    }

    fn load_images(&mut self, text: &str, cx: &mut Context<Self>) {
        let directory = self.directory(cx);
        for destination in image_destinations(text) {
            let Some(path) = image_path(destination, directory.as_deref()) else {
                continue;
            };
            if !self.requested_images.insert(destination.to_owned()) {
                continue;
            }
            let destination = destination.to_owned();
            let file = self.store.update(cx, |store, cx| {
                store.session_request(
                    cx,
                    move |session| async move { session.read_file(&path).await },
                )
            });
            cx.spawn(async move |this, cx| {
                let image = match file.await {
                    Ok(file) => match ImageFormat::from_mime_type(&file.mime_type) {
                        Some(format) => Arc::new(Image::from_bytes(format, file.bytes)),
                        None => {
                            log::debug!("Paseo image {destination} is not a supported image");
                            return;
                        }
                    },
                    Err(error) => {
                        log::debug!("Paseo could not load image {destination}: {error}");
                        return;
                    }
                };
                if let Err(error) = this.update(cx, |view, cx| {
                    view.images.borrow_mut().insert(destination, image);
                    // A loaded image changes its row's height.
                    view.list_state.remeasure();
                    cx.notify();
                }) {
                    log::debug!("Paseo agent view released: {error}");
                }
            })
            .detach();
        }
    }

    fn copy_agent_id(&mut self, _: &CopyAgentId, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = &self.agent_id {
            cx.write_to_clipboard(ClipboardItem::new_string(agent_id.clone()));
        }
    }

    fn archive(&mut self, _: &ArchiveAgent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.agent_id.clone() {
            self.store
                .update(cx, |store, cx| store.archive(&agent_id, cx));
        }
    }

    fn rename(&mut self, _: &RenameAgent, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(agent_id), Some(workspace)) = (
            self.agent_id.clone(),
            self.workspace.as_ref().and_then(WeakEntity::upgrade),
        ) else {
            return;
        };
        let current = self.title.to_string();
        workspace.update(cx, |workspace, cx| {
            connection_picker::open_rename(workspace, agent_id, current, window, cx);
        });
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        cx.notify();
    }

    pub(crate) fn toggle_answer(
        &mut self,
        request_id: &str,
        question: usize,
        label: String,
        multi_select: bool,
        cx: &mut Context<Self>,
    ) {
        let answers = self
            .question_answers
            .entry((request_id.to_owned(), question))
            .or_default();
        if let Some(position) = answers.iter().position(|answer| answer == &label) {
            answers.remove(position);
        } else if multi_select {
            answers.push(label);
        } else {
            *answers = vec![label];
        }
        cx.notify();
    }

    fn respond(
        &mut self,
        request: &PermissionRequest,
        allow: bool,
        action_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let is_question = request.extra.get("kind").and_then(Value::as_str) == Some("question");
        let response = if allow {
            let updated_input = is_question.then(|| {
                let mut input = request
                    .extra
                    .get("input")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::Object(Default::default()));
                let mut answers = serde_json::Map::new();
                for (index, question) in questions(request).iter().enumerate() {
                    if let Some(selected) = self
                        .question_answers
                        .get(&(request.request_id.clone(), index))
                    {
                        answers.insert(question.header.clone(), Value::String(selected.join(", ")));
                    }
                }
                if let Some(object) = input.as_object_mut() {
                    object.insert("answers".into(), Value::Object(answers));
                }
                input
            });
            PermissionResponse::Allow {
                selected_action_id: action_id,
                updated_input,
            }
        } else {
            PermissionResponse::Deny {
                selected_action_id: action_id,
                message: Some(
                    if is_question {
                        "Dismissed by user"
                    } else {
                        "Denied by user"
                    }
                    .into(),
                ),
            }
        };
        let request_id = request.request_id.clone();
        self.question_answers
            .retain(|(id, _), _| id != &request.request_id);
        self.store.update(cx, |store, cx| {
            store.respond_permission(request_id, response, cx)
        });
    }

    fn primary_permission(&self, cx: &App) -> Option<PermissionRequest> {
        let agent_id = self.agent_id.as_deref()?;
        self.store
            .read(cx)
            .permissions_for(agent_id)
            .into_iter()
            .next()
    }

    fn allow_first(&mut self, _: &crate::AllowPermission, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.primary_permission(cx) {
            let action = permission_actions(&request)
                .into_iter()
                .find(|action| action.allow);
            self.respond(&request, true, action.map(|action| action.id), cx);
        }
    }

    fn deny_first(&mut self, _: &crate::DenyPermission, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.primary_permission(cx) {
            let action = permission_actions(&request)
                .into_iter()
                .find(|action| !action.allow);
            self.respond(&request, false, action.map(|action| action.id), cx);
        }
    }

    /// Paseo's subagent track above the composer: a summary pill that expands to one row per
    /// subagent, each opening the subagent's conversation.
    fn render_subagent_track(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let parent_agent_id = self.agent_id.clone()?;
        let store = self.store.read(cx);
        let subagents = track_subagents(
            store.state.subagents_for(&parent_agent_id),
            &store.archived_subagents,
        );
        if subagents.is_empty() {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let summary = subagent_track_summary(&subagents);
        let finished = subagents
            .iter()
            .filter(|subagent| subagent.status != "running")
            .map(|subagent| paseo_client::subagent_timeline_id(&parent_agent_id, &subagent.id))
            .collect::<Vec<_>>();
        let now = Utc::now();
        // Collapsed, the track still stacks running subagents on one line each, so their progress
        // shows without a click.
        let compact = !self.subagents_expanded;
        let listed = subagents
            .iter()
            .filter(|subagent| !compact || subagent.status == "running")
            .copied()
            .collect::<Vec<_>>();
        let rows = (!listed.is_empty()).then(|| {
            listed
                .iter()
                .enumerate()
                .map(|(index, subagent)| {
                    let detail = if compact {
                        subagent
                            .tool_call_id
                            .as_deref()
                            .and_then(|tool_call_id| self.latest_subagent_action(tool_call_id))
                            .or_else(|| subagent.subtitle.clone())
                    } else {
                        subagent.subtitle.clone()
                    };
                    let timeline_id =
                        paseo_client::subagent_timeline_id(&parent_agent_id, &subagent.id);
                    let open = (parent_agent_id.clone(), subagent.id.clone());
                    let updated = parse_timestamp(&subagent.updated_at)
                        .map(|time| crate::timeline::format_relative(time, now));
                    h_flex()
                        .id(("paseo-subagent-row", index))
                        .group("paseo-subagent-row")
                        .px_3()
                        .py_1()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.ghost_element_hover))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.open_subagent(&open.0, &open.1, window, cx);
                        }))
                        .child(bucket_indicator(subagent_bucket(subagent)))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .when(compact, |this| this.flex_row().items_center().gap_2())
                                .when(!compact, |this| this.flex_col())
                                .child(
                                    div().flex_none().max_w(relative(0.5)).child(
                                        Label::new(subagent_title(subagent))
                                            .size(LabelSize::Default)
                                            .truncate(),
                                    ),
                                )
                                .children(detail.map(|detail| {
                                    div().min_w_0().child(
                                        Label::new(detail)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                })),
                        )
                        .children(updated.map(|updated| {
                            Label::new(updated)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        }))
                        .when(subagent.status != "running", |this| {
                            this.child(
                                div().visible_on_hover("paseo-subagent-row").child(
                                    IconButton::new(
                                        ("paseo-archive-subagent", index),
                                        IconName::Archive,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Archive subagent"))
                                    .on_click(cx.listener(
                                        move |view, _, _, cx| {
                                            cx.stop_propagation();
                                            let timeline_id = timeline_id.clone();
                                            view.store.update(cx, |store, cx| {
                                                store.archive_subagents([timeline_id], cx)
                                            });
                                        },
                                    )),
                                ),
                            )
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        });
        Some(
            v_flex()
                .id("paseo-subagent-track")
                .w_full()
                .rounded(rems_from_px(8_f32))
                .border_1()
                .border_color(colors.border_variant)
                .overflow_hidden()
                .child(
                    h_flex()
                        .id("paseo-subagent-track-header")
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .cursor_pointer()
                        .bg(colors.element_background)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.subagents_expanded = !view.subagents_expanded;
                            cx.notify();
                        }))
                        .child(
                            Icon::new(IconName::ZedAgent)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(summary).size(LabelSize::Default))
                        .child(div().flex_1())
                        .when(self.subagents_expanded && !finished.is_empty(), |this| {
                            this.child(
                                IconButton::new(
                                    "paseo-archive-finished-subagents",
                                    IconName::Archive,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Archive finished subagents"))
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        cx.stop_propagation();
                                        let finished = finished.clone();
                                        view.store.update(cx, |store, cx| {
                                            store.archive_subagents(finished, cx)
                                        });
                                    },
                                )),
                            )
                        })
                        .child(
                            Icon::new(if self.subagents_expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronUp
                            })
                            .size(IconSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .when_some(rows, |this, rows| {
                    this.child(
                        v_flex()
                            .id("paseo-subagent-rows")
                            .max_h(rems_from_px(240_f32))
                            .overflow_y_scroll()
                            .track_scroll(&self.subagent_scroll)
                            .children(rows)
                            .vertical_scrollbar_for(&self.subagent_scroll, window, cx),
                    )
                })
                .into_any_element(),
        )
    }

    /// The latest `[Tool] summary` line the subagent's tool call has logged in this thread.
    fn latest_subagent_action(&self, tool_call_id: &str) -> Option<String> {
        let call = self
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.content {
                crate::timeline::StreamContent::Tool(call) if call.call_id == tool_call_id => {
                    Some(call)
                }
                _ => None,
            })?;
        let log = call.detail.get("log").and_then(Value::as_str)?;
        Some(crate::timeline::parse_subagent_log(log).0.pop()?.describe())
    }

    fn render_subagent_header(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let subagent = store.subagent(self.agent_id.as_deref()?)?;
        let provider = store
            .provider(&subagent.provider)
            .and_then(|provider| provider.label.clone())
            .unwrap_or_else(|| subagent.provider.clone());
        let subtitle = std::iter::once(format!("{provider} subagent"))
            .chain(subagent.subtitle.clone())
            .collect::<Vec<_>>()
            .join(" · ");
        Some(
            h_flex()
                .h(rems_from_px(36_f32))
                .flex_none()
                .px_3()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(bucket_indicator(subagent_bucket(subagent)))
                .child(Label::new(self.title.clone()).truncate())
                .child(
                    Label::new(subtitle)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .into_any_element(),
        )
    }

    fn render_header(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.is_subagent() {
            return self.render_subagent_header(cx);
        }
        let agent = self.agent(cx)?;
        let provider = self.store.read(cx).provider(agent_provider(agent));
        let subtitle = header_subtitle(agent, provider);
        let directory = agent
            .directory
            .as_ref()
            .map(|directory| directory.display().to_string());
        let focus = self.focus_handle.clone();
        let bucket = self.bucket.unwrap_or(AgentBucket::Done);
        let this = cx.weak_entity();
        Some(
            h_flex()
                .h(rems_from_px(36_f32))
                .flex_none()
                .px_3()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(bucket_indicator(bucket))
                .child(
                    h_flex()
                        .min_w_0()
                        .flex_1()
                        .gap_2()
                        .child(
                            div()
                                .id("paseo-agent-title")
                                .min_w_0()
                                .cursor_pointer()
                                .tooltip({
                                    let title = self.title.clone();
                                    move |_window, cx| {
                                        Tooltip::with_meta(
                                            title.clone(),
                                            None,
                                            "Click to rename",
                                            cx,
                                        )
                                    }
                                })
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.rename(&RenameAgent, window, cx)
                                }))
                                .child(Label::new(self.title.clone()).truncate()),
                        )
                        .child(
                            div()
                                .id("paseo-agent-subtitle")
                                .min_w_0()
                                // The title gives way first so the project, model and mode stay visible.
                                .flex_shrink_0()
                                .max_w(relative(0.6))
                                .when_some(directory, |this, directory| {
                                    this.tooltip(Tooltip::text(directory))
                                })
                                .child(
                                    Label::new(subtitle)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        ),
                )
                .child(
                    PopoverMenu::new("paseo-agent-menu")
                        .trigger(
                            IconButton::new("paseo-agent-menu-button", IconName::Ellipsis)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted),
                        )
                        .anchor(gpui::Anchor::TopRight)
                        .menu(move |window, cx| {
                            let focus = focus.clone();
                            let this = this.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                let reload = this.clone();
                                menu.context(focus)
                                    .action("Rename Agent", RenameAgent.boxed_clone())
                                    .action("Copy Agent ID", CopyAgentId.boxed_clone())
                                    .action("Fork Agent", crate::ForkAgent.boxed_clone())
                                    .entry("Reload Agent", None, move |_, cx| {
                                        if let Err(error) = reload.update(cx, |view, cx| {
                                            if let Some(agent_id) = view.agent_id.clone() {
                                                view.store.update(cx, |store, cx| {
                                                    store.load_tail(agent_id, cx)
                                                });
                                            }
                                        }) {
                                            log::debug!("Paseo agent view released: {error}");
                                        }
                                    })
                                    .separator()
                                    .action("Archive Agent", ArchiveAgent.boxed_clone())
                            }))
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_permissions(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let agent_id = self.agent_id.as_deref()?;
        let requests = self.store.read(cx).permissions_for(agent_id);
        self.permission_scrolls
            .borrow_mut()
            .retain(|request_id, _| {
                requests
                    .iter()
                    .any(|request| &request.request_id == request_id)
            });
        if requests.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .gap_2()
                .children(requests.into_iter().enumerate().map(|(index, request)| {
                    self.render_permission(index == 0, request, window, cx)
                }))
                .into_any_element(),
        )
    }

    fn render_permission(
        &self,
        primary: bool,
        request: PermissionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = request
            .extra
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let colors = cx.theme().colors();
        // A question's full text is in the card body, so its header only names the kind.
        let title = match kind {
            "plan" => "Plan".to_owned(),
            "question" => "Question".to_owned(),
            _ => request.title.clone(),
        };
        let request_id = request.request_id.clone();
        let mut card = v_flex()
            .id(SharedString::from(format!("paseo-permission-{request_id}")))
            .w_full()
            .p_3()
            .gap_2()
            .rounded_lg()
            .bg(colors.surface_background)
            .border_1()
            .border_color(if primary {
                cx.theme().status().warning.opacity(0.5)
            } else {
                colors.border
            })
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(match kind {
                            "question" => IconName::Chat,
                            "plan" => IconName::ListTodo,
                            _ => IconName::Warning,
                        })
                        .size(IconSize::Small)
                        .color(Color::Warning),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(title)
                                .weight(gpui::FontWeight::MEDIUM)
                                .truncate(),
                        ),
                    ),
            )
            .when_some(request.description.clone(), |this, description| {
                this.child(
                    Label::new(description)
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                )
            });
        if kind == "question" {
            card = card.child(self.render_questions(&request, cx));
        } else if let Some(preview) = crate::stream::permission_preview(&request, cx) {
            let scroll = self
                .permission_scrolls
                .borrow_mut()
                .entry(request_id.clone())
                .or_default()
                .clone();
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "paseo-permission-preview-{request_id}"
                    )))
                    .max_h(rems_from_px(200_f32))
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .rounded_md()
                    .bg(colors.editor_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .p_2()
                    .child(preview)
                    .vertical_scrollbar_for(&scroll, window, cx),
            );
        }
        let actions = permission_actions(&request);
        let mut buttons = h_flex().gap_1p5().flex_wrap();
        let first_allow = actions.iter().position(|action| action.allow);
        let first_deny = actions.iter().position(|action| !action.allow);
        for (index, action) in actions.into_iter().enumerate() {
            let request = request.clone();
            let action_id = action.id.clone();
            let allow = action.allow;
            let binding: Option<Box<dyn gpui::Action>> = if !primary {
                None
            } else if Some(index) == first_allow {
                Some(Box::new(crate::AllowPermission))
            } else if Some(index) == first_deny {
                Some(Box::new(crate::DenyPermission))
            } else {
                None
            };
            let focus = self.focus_handle.clone();
            buttons = buttons.child(
                ui::Button::new(
                    SharedString::from(format!("paseo-permission-{}-{}", request_id, action.id)),
                    action.label,
                )
                .label_size(LabelSize::Default)
                .style(match action.variant.as_deref() {
                    Some("primary") => ButtonStyle::Filled,
                    _ if allow && Some(index) == first_allow => ButtonStyle::Filled,
                    _ => ButtonStyle::Outlined,
                })
                .color(if action.variant.as_deref() == Some("danger") {
                    Color::Error
                } else {
                    Color::Default
                })
                .start_icon(
                    Icon::new(if allow {
                        IconName::Check
                    } else {
                        IconName::Close
                    })
                    .size(IconSize::Small),
                )
                .when_some(binding, |button, binding| {
                    button.key_binding(
                        ui::KeyBinding::for_action_in(binding.as_ref(), &focus, cx)
                            .size(rems_from_px(10_f32)),
                    )
                })
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.respond(&request, allow, Some(action_id.clone()), cx);
                })),
            );
        }
        card.child(
            Label::new(if kind == "question" {
                "Answer to continue"
            } else {
                "How would you like to proceed?"
            })
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(buttons)
        .into_any_element()
    }

    fn render_questions(&self, request: &PermissionRequest, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        v_flex()
            .gap_2()
            .children(
                questions(request)
                    .into_iter()
                    .enumerate()
                    .map(|(index, question)| {
                        let selected = self
                            .question_answers
                            .get(&(request.request_id.clone(), index))
                            .cloned()
                            .unwrap_or_default();
                        v_flex()
                            .gap_1()
                            .when(!question.header.is_empty(), |this| {
                                this.child(
                                    Label::new(question.header.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .child(Label::new(question.question.clone()).size(LabelSize::Default))
                            .children(question.options.into_iter().enumerate().map(
                                |(option_index, option)| {
                                    let is_selected = selected.contains(&option.0);
                                    let request_id = request.request_id.clone();
                                    let label = option.0.clone();
                                    let multi_select = question.multi_select;
                                    h_flex()
                                        .id(SharedString::from(format!(
                                            "paseo-question-{request_id}-{index}-{option_index}"
                                        )))
                                        .gap_2()
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(if is_selected {
                                            colors.border_focused
                                        } else {
                                            colors.border_variant
                                        })
                                        .when(is_selected, |this| this.bg(colors.element_selected))
                                        .hover(|style| style.bg(colors.element_hover))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.toggle_answer(
                                                &request_id,
                                                index,
                                                label.clone(),
                                                multi_select,
                                                cx,
                                            )
                                        }))
                                        .child(
                                            Icon::new(if is_selected {
                                                IconName::Check
                                            } else if multi_select {
                                                IconName::SquarePlus
                                            } else {
                                                IconName::Circle
                                            })
                                            .size(IconSize::Small)
                                            .color(
                                                if is_selected {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                },
                                            ),
                                        )
                                        .child(
                                            v_flex()
                                                .min_w_0()
                                                .child(
                                                    Label::new(option.0).size(LabelSize::Default),
                                                )
                                                .when_some(option.1, |this, description| {
                                                    this.child(
                                                        Label::new(description)
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                }),
                                        )
                                },
                            ))
                    }),
            )
            .into_any_element()
    }

    fn render_draft_landing(&self, cx: &Context<Self>) -> AnyElement {
        let composer = self.composer.read(cx);
        let directory = composer.draft_directory.clone();
        let label = directory
            .as_ref()
            .and_then(|directory| directory.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Choose a project".into());
        let full = directory
            .as_ref()
            .map(|directory| directory.to_string_lossy().into_owned());
        let status = self.store.read(cx).status;
        let isolation = composer.can_create_worktree(cx).then(|| {
            let new_worktree = composer.uses_new_worktree(cx);
            let option = |id: &'static str, label: &'static str, icon: IconName, selected: bool| {
                ui::Button::new(id, label)
                    .style(if selected {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Subtle
                    })
                    .toggle_state(selected)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            };
            h_flex()
                .gap_1()
                .child(
                    Label::new("Isolation")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    option(
                        "paseo-isolation-local",
                        "Local",
                        IconName::Folder,
                        !new_worktree,
                    )
                    .tooltip(Tooltip::text("Work directly in the project directory"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.composer
                            .update(cx, |composer, cx| composer.set_new_worktree(false, cx));
                    })),
                )
                .child(
                    option(
                        "paseo-isolation-worktree",
                        "New worktree",
                        IconName::GitBranch,
                        new_worktree,
                    )
                    .tooltip(Tooltip::text(
                        "Start the agent on a new branch in its own git worktree",
                    ))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.composer
                            .update(cx, |composer, cx| composer.set_new_worktree(true, cx));
                    })),
                )
                .when(new_worktree, |row| {
                    let base = composer.worktree_base();
                    let label = base
                        .map(|base| base.label.clone())
                        .unwrap_or_else(|| "Default branch".into());
                    let tooltip = base
                        .map(|base| format!("Branch off {}", base.ref_name))
                        .unwrap_or_else(|| "Branch off the repository's default branch".into());
                    row.child(
                        Label::new("Base")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .ml_2(),
                    )
                    .child(
                        ui::Button::new("paseo-worktree-base", label)
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .disabled(directory.is_none())
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.choose_worktree_base(window, cx);
                            })),
                    )
                })
        });
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::Sparkle)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Headline::new("New agent").size(HeadlineSize::Medium))
            .child(
                ui::Button::new("paseo-draft-directory", label)
                    .style(ButtonStyle::Outlined)
                    .start_icon(
                        Icon::new(IconName::Folder)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(full, |button, full| button.tooltip(Tooltip::text(full)))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.choose_directory(window, cx);
                    })),
            )
            .children(isolation)
            .child(
                Label::new(if status == crate::store::ConnectionStatus::Connected {
                    "Describe the task. The first message starts the agent."
                } else {
                    "Connect to a Paseo host to start agents."
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    pub(crate) fn choose_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let composer = self.composer.downgrade();
        workspace.update(cx, |workspace, cx| {
            crate::command_center::choose_directory(workspace, composer, window, cx);
        });
    }

    fn choose_worktree_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let composer = self.composer.read(cx);
        let Some(directory) = composer
            .draft_directory
            .as_ref()
            .and_then(|directory| directory.to_str())
            .map(str::to_owned)
        else {
            return;
        };
        let selected = composer.worktree_base().cloned();
        let composer = self.composer.downgrade();
        workspace.update(cx, |workspace, cx| {
            crate::command_center::choose_worktree_base(
                workspace, composer, directory, selected, window, cx,
            );
        });
    }

    fn render_scroll_to_bottom(&self, cx: &Context<Self>) -> Option<AnyElement> {
        // Heights of unmeasured rows are unknown, so "not following" alone decides visibility.
        if self.list_state.is_following_tail()
            || self.rows.len() < 3
            || self.list_state.is_scrolled_to_end() == Some(true)
        {
            return None;
        }
        let colors = cx.theme().colors();
        Some(
            div()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("paseo-scroll-bottom")
                        .size(rems_from_px(32_f32))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(colors.elevated_surface_background)
                        .border_1()
                        .border_color(colors.border)
                        .shadow_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.element_hover))
                        .tooltip(Tooltip::text("Scroll to latest"))
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.scroll_to_bottom(&ScrollToBottom, window, cx)
                        }))
                        .child(Icon::new(IconName::ArrowDown).size(IconSize::Small)),
                )
                .into_any_element(),
        )
    }
}

pub(crate) struct PermissionActionChoice {
    pub id: String,
    pub label: String,
    pub allow: bool,
    pub variant: Option<String>,
}

pub(crate) fn permission_actions(request: &PermissionRequest) -> Vec<PermissionActionChoice> {
    let actions = request
        .extra
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|action| {
            Some(PermissionActionChoice {
                id: action.get("id")?.as_str()?.to_owned(),
                label: action
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("Continue")
                    .to_owned(),
                allow: action.get("behavior").and_then(Value::as_str) != Some("deny"),
                variant: action
                    .get("variant")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect::<Vec<_>>();
    if !actions.is_empty() {
        return actions;
    }
    let kind = request.extra.get("kind").and_then(Value::as_str);
    vec![
        PermissionActionChoice {
            id: "reject".into(),
            label: if kind == Some("question") {
                "Dismiss".into()
            } else {
                "Deny".into()
            },
            allow: false,
            variant: None,
        },
        PermissionActionChoice {
            id: "accept".into(),
            label: match kind {
                Some("plan") => "Implement".into(),
                Some("question") => "Submit".into(),
                _ => "Allow".into(),
            },
            allow: true,
            variant: Some("primary".into()),
        },
    ]
}

pub(crate) struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<(String, Option<String>)>,
    pub multi_select: bool,
}

pub(crate) fn questions(request: &PermissionRequest) -> Vec<Question> {
    request
        .extra
        .get("input")
        .and_then(|input| input.get("questions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|question| Question {
            header: question
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            question: question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            options: question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    Some((
                        option.get("label")?.as_str()?.to_owned(),
                        option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    ))
                })
                .collect(),
            multi_select: question.get("multiSelect").and_then(Value::as_bool) == Some(true),
        })
        .collect()
}

pub(crate) fn bucket_indicator(bucket: AgentBucket) -> AnyElement {
    match bucket {
        AgentBucket::Running => Icon::new(IconName::LoadCircle)
            .size(IconSize::XSmall)
            .color(Color::Info)
            .with_rotate_animation(2)
            .into_any_element(),
        AgentBucket::NeedsInput => Indicator::dot().color(Color::Warning).into_any_element(),
        AgentBucket::Failed => Indicator::dot().color(Color::Error).into_any_element(),
        AgentBucket::Attention => Indicator::dot().color(Color::Success).into_any_element(),
        AgentBucket::Done => Indicator::dot().color(Color::Muted).into_any_element(),
    }
}

impl Focusable for AgentView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let is_draft = self.agent_id.is_none();
        let empty = self.items.is_empty();
        let body = if is_draft && empty {
            self.render_draft_landing(cx)
        } else if empty {
            let store = self.store.read(cx);
            let loaded = self
                .agent_id
                .as_deref()
                .and_then(|agent_id| store.paging.get(agent_id))
                .is_some_and(|paging| paging.loaded);
            let message = if !store.connected() {
                "Waiting for the Paseo connection…"
            } else if !loaded {
                "Loading conversation…"
            } else {
                "Start chatting with this agent…"
            };
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element()
        } else {
            // Rows create their markdown while the list lays out, and a batch of never-parsed
            // messages renders with no height, so the list stays hidden until they have text.
            // A single new message is left visible so streaming never blinks.
            let markdown = &self.markdown;
            self.unparsed_markdown.retain(|key| {
                markdown
                    .get(key)
                    .is_some_and(|markdown| markdown.read(cx).is_parsing())
            });
            let unparsed = self.unparsed_markdown.len();
            // The subagent bar arrives with its own request; waiting briefly for it keeps it from
            // pushing the composer up just after the conversation appears.
            let subagents_loading = self
                .agent_id
                .as_ref()
                .is_some_and(|agent_id| self.store.read(cx).subagents_loading.contains(agent_id));
            let hidden = match self.reveal {
                Reveal::Pending => {
                    self.reveal = Reveal::Parsing(Instant::now());
                    true
                }
                Reveal::Parsing(started)
                    if unparsed > 0
                        || (subagents_loading && started.elapsed() < REVEAL_WAIT_FOR_SUBAGENTS) =>
                {
                    true
                }
                Reveal::Parsing(_) | Reveal::Shown => {
                    self.reveal = Reveal::Shown;
                    unparsed > 1
                }
            };
            if hidden {
                window.request_animation_frame();
            }
            // `list` ignores visibility, but a hidden div still lays its children out.
            div()
                .size_full()
                .when(hidden, |this| this.invisible())
                .child(
                    list(
                        self.list_state.clone(),
                        cx.processor(|view, index, window, cx| view.render_row(index, window, cx)),
                    )
                    .size_full(),
                )
                .into_any_element()
        };
        let error = self.store.read(cx).state.error.clone();
        let font_size = crate::chat_font_size(cx);
        if font_size != self.font_size {
            self.font_size = font_size;
            self.list_state.remeasure();
        }
        WithRemSize::new(font_size).size_full().child(
            v_flex()
                .key_context("PaseoAgentView")
                .track_focus(&self.focus_handle)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|view, _, window, cx| {
                        // Clicking the conversation keeps Paseo shortcuts scoped to this agent.
                        if !view.focus_handle.contains_focused(window, cx) {
                            view.focus_composer(window, cx);
                        }
                    }),
                )
                .on_action(cx.listener(Self::copy_agent_id))
                .on_action(
                    cx.listener(|view, _: &crate::ForkAgent, window, cx| view.fork(window, cx)),
                )
                .on_action(cx.listener(Self::archive))
                .on_action(cx.listener(Self::rename))
                .on_action(cx.listener(Self::scroll_to_bottom))
                .on_action(cx.listener(Self::allow_first))
                .on_action(cx.listener(Self::deny_first))
                .on_action(cx.listener(|view, _: &FocusComposer, window, cx| {
                    view.focus_composer(window, cx)
                }))
                .size_full()
                .bg(colors.editor_background)
                .children(self.render_header(cx))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(body)
                        .children(self.render_scroll_to_bottom(cx)),
                )
                .child(
                    h_flex().w_full().justify_center().px_4().pb_3().child(
                        v_flex()
                            .w_full()
                            .max_w(rems_from_px(CONTENT_MAX_WIDTH))
                            .gap_2()
                            .when_some(error, |this, error| {
                                this.child(
                                    h_flex()
                                        .gap_2()
                                        .px_3()
                                        .py_1p5()
                                        .rounded_md()
                                        .bg(cx.theme().status().error_background)
                                        .child(
                                            Icon::new(IconName::XCircle)
                                                .size(IconSize::Small)
                                                .color(Color::Error),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(Label::new(error).size(LabelSize::Small)),
                                        )
                                        .child(
                                            IconButton::new("paseo-dismiss-error", IconName::Close)
                                                .icon_size(IconSize::XSmall)
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.store.update(cx, |store, cx| {
                                                        store.dismiss_error(cx)
                                                    })
                                                })),
                                        ),
                                )
                            })
                            .when(!self.is_subagent(), |this| {
                                this.children(self.render_subagent_track(window, cx))
                                    .children(self.render_permissions(window, cx))
                                    .child(self.composer.clone())
                            }),
                    ),
                ),
        )
    }
}

pub struct AgentTab {
    view: Entity<AgentView>,
    _subscription: Subscription,
}

impl AgentTab {
    pub fn new(
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| AgentView::new(agent_id, directory, workspace, window, cx));
        let subscription = cx.subscribe(&view, |_, _, event: &AgentViewEvent, cx| match event {
            AgentViewEvent::TabChanged => cx.emit(ItemEvent::UpdateTab),
        });
        Self {
            view,
            _subscription: subscription,
        }
    }

    pub fn agent_id(&self, cx: &App) -> Option<String> {
        self.view.read(cx).agent_id.clone()
    }

    pub fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).composer.focus_handle(cx)
    }

    pub fn view(&self) -> &Entity<AgentView> {
        &self.view
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn composer_text(&self, cx: &App) -> String {
        self.view.read(cx).composer.read(cx).text(cx)
    }
}

impl EventEmitter<ItemEvent> for AgentTab {}

impl Focusable for AgentTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).input_focus_handle(cx)
    }
}

impl Render for AgentTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.view.clone()
    }
}

impl Item for AgentTab {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let view = self.view.read(cx);
        h_flex()
            .gap_1p5()
            .child(match view.bucket {
                Some(bucket) => bucket_indicator(bucket),
                None => Icon::new(IconName::Sparkle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            })
            .child(
                Label::new(view.title.clone())
                    .single_line()
                    .color(params.text_color()),
            )
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.view.read(cx).title.clone()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let view = self.view.read(cx);
        let agent = view.agent(cx)?;
        Some(
            format!(
                "{} · {}",
                agent_title(agent),
                agent
                    .directory
                    .as_ref()
                    .map(|directory| directory.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
            .into(),
        )
    }
}

impl SerializableItem for AgentTab {
    fn serialized_item_kind() -> &'static str {
        "PaseoAgentTab"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = persistence::AgentTabDb::global(cx);
        workspace::delete_unloaded_items(alive_items, workspace_id, "paseo_agent_tabs", &db, cx)
    }

    fn deserialize(
        _project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = persistence::AgentTabDb::global(cx);
        window.spawn(cx, async move |cx| {
            let agent_id = db
                .get_agent_tab(item_id, workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("No Paseo agent tab to restore"))?;
            cx.update(|window, cx| {
                cx.new(|cx| AgentTab::new(Some(agent_id), None, Some(workspace), window, cx))
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let agent_id = self
            .agent_id(cx)
            .filter(|agent_id| paseo_client::parse_subagent_timeline_id(agent_id).is_none())?;
        let db = persistence::AgentTabDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_agent_tab(item_id, workspace_id, agent_id).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

mod persistence {
    use db::{query, sqlez::domain::Domain, sqlez_macros::sql};
    use workspace::WorkspaceDb;

    pub struct AgentTabDb(db::sqlez::thread_safe_connection::ThreadSafeConnection);

    impl Domain for AgentTabDb {
        const NAME: &str = stringify!(AgentTabDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE paseo_agent_tabs (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                agent_id TEXT NOT NULL,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(AgentTabDb, [WorkspaceDb]);

    impl AgentTabDb {
        query! {
            pub async fn save_agent_tab(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                agent_id: String
            ) -> Result<()> {
                INSERT OR REPLACE INTO paseo_agent_tabs(item_id, workspace_id, agent_id)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_agent_tab(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<String>> {
                SELECT agent_id
                FROM paseo_agent_tabs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

/// The link targets of markdown images (`![alt](target)`) in `text`.
fn image_destinations(text: &str) -> Vec<&str> {
    let mut destinations = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("![") {
        rest = &rest[start + 2..];
        let Some(close) = rest.find("](") else {
            break;
        };
        rest = &rest[close + 2..];
        let Some(end) = rest.find(')') else {
            break;
        };
        let target = rest[..end].trim();
        let target = match target.strip_prefix('<') {
            Some(bracketed) => bracketed.split('>').next().unwrap_or(""),
            None => target.split_whitespace().next().unwrap_or(""),
        };
        if !target.is_empty() {
            destinations.push(target);
        }
        rest = &rest[end + 1..];
    }
    destinations
}

/// The absolute path on the daemon's host for an image link, resolving relative links against
/// the agent's directory. Web links load directly, so they have none.
fn image_path(destination: &str, directory: Option<&std::path::Path>) -> Option<String> {
    if destination.starts_with("http://")
        || destination.starts_with("https://")
        || destination.starts_with("data:")
    {
        return None;
    }
    if destination.starts_with("file://") {
        let path = url::Url::parse(destination).ok()?.to_file_path().ok()?;
        return path.to_str().map(str::to_owned);
    }
    if destination.starts_with('/') {
        return Some(destination.to_owned());
    }
    let joined = directory?.join(destination);
    joined.to_str().map(str::to_owned)
}

/// The subagents the track lists: the agent's subagents minus the ones the user archived.
fn track_subagents<'a>(
    subagents: &'a [paseo_client::ProviderSubagent],
    archived: &std::collections::BTreeSet<String>,
) -> Vec<&'a paseo_client::ProviderSubagent> {
    subagents
        .iter()
        .filter(|subagent| {
            !archived.contains(&paseo_client::subagent_timeline_id(
                &subagent.parent_agent_id,
                &subagent.id,
            ))
        })
        .collect()
}

/// Paseo's track pill text: `3 subagents · 1 working · 1 failed`.
fn subagent_track_summary(subagents: &[&paseo_client::ProviderSubagent]) -> String {
    let count = |status: &str| {
        subagents
            .iter()
            .filter(|subagent| subagent.status == status)
            .count()
    };
    let total = if subagents.len() == 1 {
        "1 subagent".to_owned()
    } else {
        format!("{} subagents", subagents.len())
    };
    let working = count("running");
    let failed = count("failed");
    std::iter::once(total)
        .chain((working > 0).then(|| format!("{working} working")))
        .chain((failed > 0).then(|| format!("{failed} failed")))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The header's `project › worktree · branch · provider · model · mode` line, with labels from the
/// provider snapshot where it has them.
fn header_subtitle(
    agent: &paseo_client::AgentSummary,
    provider: Option<&paseo_client::Provider>,
) -> String {
    let project = match crate::store::agent_worktree_name(agent) {
        Some(worktree) => format!("{} › {worktree}", agent_project_name(agent)),
        None => agent_project_name(agent),
    };
    let text = |key: &str| {
        agent
            .extra
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let provider_label = provider
        .and_then(|provider| provider.label.clone())
        .filter(|label| !label.is_empty())
        .or_else(|| text("provider").map(str::to_owned));
    let model = text("model").or_else(|| {
        agent
            .extra
            .get("runtimeInfo")
            .and_then(|runtime| runtime.get("model"))
            .and_then(Value::as_str)
    });
    let model_label = model.map(|model| {
        provider
            .map(crate::composer::provider_models)
            .and_then(|models| models.into_iter().find(|choice| choice.id == model))
            .map_or_else(|| model.to_owned(), |choice| choice.label)
    });
    let mode_label = text("currentModeId").map(|mode| {
        crate::composer::choices(agent.extra.get("availableModes"))
            .into_iter()
            .find(|choice| choice.id == mode)
            .map_or_else(|| mode.to_owned(), |choice| choice.label)
    });
    std::iter::once(project)
        .chain(agent_branch(agent))
        .chain(provider_label)
        .chain(model_label)
        .chain(mode_label)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Opens the files that thread links and `path:line` code spans point at.
#[derive(Clone)]
struct ThreadLinks {
    store: Entity<PaseoStore>,
    agent_id: Option<String>,
    workspace: Option<WeakEntity<Workspace>>,
    /// Resolved code spans, because resolving runs on every render of every span.
    code_spans: Rc<RefCell<HashMap<String, Option<SharedString>>>>,
}

/// Enough resolved code spans for a long thread; past this the cache starts over.
const CODE_SPAN_CACHE_LIMIT: usize = 4096;

impl ThreadLinks {
    fn directory(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .timeline_directory(self.agent_id.as_deref()?)
    }

    fn workspace(&self) -> Option<Entity<Workspace>> {
        self.workspace.as_ref().and_then(WeakEntity::upgrade)
    }

    /// Code spans become links only when they name a file in the open project, because this
    /// runs on every render of every span.
    /// Code spans become links when they name an existing file or project folder, as a `file://`
    /// URL with the line, so a click opens the file the span resolved to.
    fn code_span_link(&self, text: &str, cx: &App) -> Option<SharedString> {
        let text = text.trim();
        if !is_path_like(text) {
            return None;
        }
        if let Some(cached) = self.code_spans.borrow().get(text) {
            return cached.clone();
        }
        let resolved = self.resolve_code_span(text, cx);
        let mut code_spans = self.code_spans.borrow_mut();
        if code_spans.len() >= CODE_SPAN_CACHE_LIMIT {
            code_spans.clear();
        }
        code_spans.insert(text.to_owned(), resolved.clone());
        resolved
    }

    fn resolve_code_span(&self, text: &str, cx: &App) -> Option<SharedString> {
        let target = link_target(text, self.directory(cx).as_deref())?;
        let project = self.workspace()?.read(cx).project().read(cx);
        let entry = project
            .find_project_path(&target.path, cx)
            .and_then(|project_path| project.entry_for_path(&project_path, cx));
        let path = match entry {
            Some(_) => target.path,
            // Ignored folders such as `.notes` have no project entries until expanded.
            None if self.store.read(cx).is_local_host()
                && std::fs::metadata(&target.path).is_ok_and(|metadata| metadata.is_file()) =>
            {
                target.path
            }
            // Agents often name a file without its folders, like `producer.rs:83`.
            None if !text.contains(['/', '\\']) => {
                unique_project_file(project, target.path.file_name()?.to_str()?, cx)?
            }
            None => return None,
        };
        let mut url = url::Url::from_file_path(&path).ok()?;
        if let Some(row) = target.row {
            url.set_fragment(Some(&format!("L{row}")));
        }
        Some(url.to_string().into())
    }

    fn open(&self, url: &str, window: &mut Window, cx: &mut App) {
        let Some(target) = link_target(url, self.directory(cx).as_deref()) else {
            // Web, mail and other scheme links go to the system; a scheme is at least two
            // characters, so a Windows drive letter is not one.
            if url::Url::parse(url).is_ok_and(|parsed| parsed.scheme().len() > 1) {
                cx.open_url(url);
            }
            return;
        };
        let Some(workspace) = self.workspace() else {
            return;
        };
        let is_local_host = self.store.read(cx).is_local_host();
        let point = target.row.map(|row| {
            language::Point::new(
                row.saturating_sub(1),
                target.column.unwrap_or(1).saturating_sub(1),
            )
        });
        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            let project_path = project.read(cx).find_project_path(&target.path, cx);
            let entry = project_path
                .as_ref()
                .and_then(|project_path| project.read(cx).entry_for_path(project_path, cx))
                .map(|entry| (entry.id, entry.is_dir()));
            // Opening a missing path would create an empty buffer, and agents often link files
            // that a later step moved or deleted. Ignored folders have no entries until expanded,
            // so a missing entry is checked on disk when the files are on this machine.
            let opened = match (project_path, entry) {
                (Some(_), Some((entry_id, true))) => {
                    project.update(cx, |_, cx| {
                        cx.emit(project::Event::RevealInProjectPanel(entry_id));
                    });
                    return;
                }
                (Some(project_path), Some(_)) => {
                    workspace.open_path(project_path, None, true, window, cx)
                }
                (_, None) if is_local_host => {
                    let fs = workspace.app_state().fs.clone();
                    let path = target.path.clone();
                    let workspace = cx.weak_entity();
                    window.spawn(cx, async move |cx| {
                        match fs.metadata(&path).await? {
                            None => anyhow::bail!("{} no longer exists", path.display()),
                            Some(metadata) if metadata.is_dir => {
                                anyhow::bail!("{} is a folder", path.display())
                            }
                            Some(_) => {}
                        }
                        workspace
                            .update_in(cx, |workspace, window, cx| {
                                workspace.open_abs_path(
                                    path,
                                    workspace::OpenOptions {
                                        focus: Some(true),
                                        ..Default::default()
                                    },
                                    window,
                                    cx,
                                )
                            })?
                            .await
                    })
                }
                (Some(_), None) => {
                    workspace.show_error(
                        anyhow::anyhow!(
                            "{} was not found in the open project",
                            target.path.display()
                        ),
                        cx,
                    );
                    return;
                }
                (None, _) => {
                    workspace.show_error(
                        anyhow::anyhow!("{} is not in the open project", target.path.display()),
                        cx,
                    );
                    return;
                }
            };
            let workspace = cx.weak_entity();
            window
                .spawn(cx, async move |cx| {
                    let item = match opened.await {
                        Ok(item) => item,
                        Err(error) => {
                            workspace
                                .update(cx, |workspace, cx| workspace.show_error(error, cx))?;
                            return Ok(());
                        }
                    };
                    if let Some(point) = point
                        && let Some(editor) = item.downcast::<editor::Editor>()
                    {
                        editor.update_in(cx, |editor, window, cx| {
                            editor.change_selections(
                                editor::SelectionEffects::scroll(
                                    editor::scroll::Autoscroll::center(),
                                ),
                                window,
                                cx,
                                |selections| selections.select_ranges([point..point]),
                            );
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
        });
    }
}

/// The project's only file with this name, or `None` when there is none or more than one.
fn unique_project_file(project: &project::Project, file_name: &str, cx: &App) -> Option<PathBuf> {
    let mut matches = project.visible_worktrees(cx).flat_map(|worktree| {
        let worktree = worktree.read(cx);
        worktree
            .files(false, 0)
            .filter(|entry| entry.path.file_name() == Some(file_name))
            .take(2)
            .map(|entry| worktree.absolutize(&entry.path))
            .collect::<Vec<_>>()
    });
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
}

/// A file a thread link names, with its one-based line and column.
#[derive(Debug, PartialEq)]
struct LinkTarget {
    path: PathBuf,
    row: Option<u32>,
    column: Option<u32>,
}

/// Resolves a link or code span to a file: a `file://` URL, an absolute path, or a path relative
/// to the agent's directory, with an optional `:line:column` suffix or `#L<line>` fragment.
/// Web and other scheme links name no file.
fn link_target(link: &str, directory: Option<&std::path::Path>) -> Option<LinkTarget> {
    let (link, fragment) = match link.split_once('#') {
        Some((link, fragment)) => (link, Some(fragment)),
        None => (link, None),
    };
    let path_text = if link.starts_with("file://") {
        let path = url::Url::parse(link).ok()?.to_file_path().ok()?;
        path.to_str()?.to_owned()
    } else if link.is_empty() || link.contains("://") || link.starts_with("mailto:") {
        return None;
    } else {
        link.to_owned()
    };
    let parsed = util::paths::PathWithPosition::parse_str(&path_text);
    let (row, column) = match fragment.and_then(fragment_line) {
        Some(row) => (Some(row), None),
        None => (parsed.row, parsed.column),
    };
    let path = if parsed.path.is_absolute() {
        parsed.path
    } else {
        directory?.join(parsed.path)
    };
    Some(LinkTarget { path, row, column })
}

/// The line of a `#L12`, `#L12C3` or `#L12-L20` fragment.
fn fragment_line(fragment: &str) -> Option<u32> {
    let digits: String = fragment
        .strip_prefix('L')?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn is_path_like(text: &str) -> bool {
    if text.len() < 3
        || text.contains("://")
        || text.contains(char::is_whitespace)
        || text.contains('|')
        || text.chars().all(|character| character.is_ascii_digit())
    {
        return false;
    }
    let path = util::paths::PathWithPosition::parse_str(text).path;
    path.to_string_lossy().contains('/')
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| !extension.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subagent(id: &str, status: &str) -> paseo_client::ProviderSubagent {
        paseo_client::ProviderSubagent {
            id: id.into(),
            parent_agent_id: "parent".into(),
            parent_subagent_id: None,
            provider: "claude".into(),
            title: None,
            description: Some(format!("Task {id}")),
            status: status.into(),
            created_at: "2026-09-28T10:00:00Z".into(),
            updated_at: "2026-09-28T10:00:00Z".into(),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
        }
    }

    #[test]
    fn subagent_track_summary_counts_working_and_failed() {
        let subagents = [
            subagent("a", "running"),
            subagent("b", "completed"),
            subagent("c", "failed"),
        ];
        let listed = subagents.iter().collect::<Vec<_>>();
        assert_eq!(
            subagent_track_summary(&listed),
            "3 subagents · 1 working · 1 failed"
        );
        assert_eq!(subagent_track_summary(&listed[1..2]), "1 subagent");
    }

    #[test]
    fn archived_subagents_leave_the_track() {
        let subagents = [subagent("a", "completed"), subagent("b", "running")];
        let archived = [paseo_client::subagent_timeline_id("parent", "a")]
            .into_iter()
            .collect();
        let listed = track_subagents(&subagents, &archived);
        assert_eq!(
            listed
                .iter()
                .map(|subagent| subagent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn header_subtitle_names_worktree_provider_model_and_mode() {
        let agent = paseo_client::AgentSummary {
            id: "agent".into(),
            title: None,
            status: "idle".into(),
            directory: Some(PathBuf::from(
                "/home/me/.paseo/worktrees/abc/prolific-snake",
            )),
            project: Some(serde_json::json!({
                "projectName": "axon",
                "checkout": {
                    "currentBranch": "feature",
                    "isPaseoOwnedWorktree": true,
                    "worktreeRoot": "/home/me/.paseo/worktrees/abc/prolific-snake",
                },
            })),
            extra: serde_json::json!({
                "provider": "claude",
                "model": "opus",
                "currentModeId": "plan",
                "availableModes": [{"id": "plan", "label": "Plan"}],
            }),
        };
        let provider = paseo_client::Provider {
            id: "claude".into(),
            label: Some("Claude".into()),
            status: "ready".into(),
            extra: serde_json::json!({"models": [{"id": "opus", "label": "Opus 4.1"}]}),
        };
        assert_eq!(
            header_subtitle(&agent, Some(&provider)),
            "axon › prolific-snake · feature · Claude · Opus 4.1 · Plan"
        );
        assert_eq!(
            header_subtitle(&agent, None),
            "axon › prolific-snake · feature · claude · opus · Plan"
        );
    }

    #[test]
    fn link_target_resolves_paths_and_positions() {
        let directory = Some(std::path::Path::new("/repo"));
        let target = |link: &str| link_target(link, directory);
        assert_eq!(
            target("src/main.rs:12:3"),
            Some(LinkTarget {
                path: PathBuf::from("/repo/src/main.rs"),
                row: Some(12),
                column: Some(3),
            })
        );
        assert_eq!(
            target("file:///repo/a%20b.rs#L40-L42"),
            Some(LinkTarget {
                path: PathBuf::from("/repo/a b.rs"),
                row: Some(40),
                column: None,
            })
        );
        assert_eq!(
            target("/etc/hosts"),
            Some(LinkTarget {
                path: PathBuf::from("/etc/hosts"),
                row: None,
                column: None,
            })
        );
        assert_eq!(target("https://example.com/a.rs"), None);
        assert_eq!(target("mailto:someone@example.com"), None);
        assert_eq!(target("#section"), None);
        assert_eq!(link_target("src/main.rs", None), None);
    }

    #[gpui::test]
    async fn unique_project_file_finds_a_bare_file_name(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            serde_json::json!({
                "src": { "producer.rs": "", "mod_a": { "lib.rs": "" }, "mod_b": { "lib.rs": "" } },
            }),
        )
        .await;
        let project = project::Project::test(fs, [std::path::Path::new("/repo")], cx).await;
        cx.run_until_parked();
        cx.update(|cx| {
            let project = project.read(cx);
            assert_eq!(
                unique_project_file(project, "producer.rs", cx),
                Some(PathBuf::from("/repo/src/producer.rs"))
            );
            assert_eq!(unique_project_file(project, "lib.rs", cx), None);
            assert_eq!(unique_project_file(project, "missing.rs", cx), None);
        });
    }

    #[test]
    fn is_path_like_accepts_only_file_shaped_spans() {
        assert!(is_path_like("src/main.rs:42"));
        assert!(is_path_like("Cargo.toml"));
        assert!(is_path_like("crates/paseo_ui"));
        assert!(!is_path_like("cargo test"));
        assert!(!is_path_like("1234"));
        assert!(!is_path_like("Option"));
        assert!(!is_path_like("https://example.com"));
    }

    #[test]
    fn image_links_resolve_to_daemon_paths() {
        let text = "Shot: ![Image](file:///tmp/paseo-attachments-x/a%20b.png) and \
            ![plot](<out/plot.png> \"title\") and ![web](https://example.com/c.png) ![broken";
        let destinations = image_destinations(text);
        assert_eq!(
            destinations,
            vec![
                "file:///tmp/paseo-attachments-x/a%20b.png",
                "out/plot.png",
                "https://example.com/c.png",
            ]
        );
        let directory = std::path::Path::new("/work/project");
        assert_eq!(
            image_path(destinations[0], Some(directory)).as_deref(),
            Some("/tmp/paseo-attachments-x/a b.png")
        );
        assert_eq!(
            image_path(destinations[1], Some(directory)).as_deref(),
            Some("/work/project/out/plot.png")
        );
        assert_eq!(image_path(destinations[1], None), None);
        assert_eq!(image_path(destinations[2], Some(directory)), None);
    }
    use serde_json::json;

    fn request(extra: Value) -> PermissionRequest {
        PermissionRequest {
            agent_id: "agent".into(),
            request_id: "request".into(),
            title: "Run command".into(),
            description: None,
            extra,
        }
    }

    #[test]
    fn permission_actions_fall_back_to_paseo_defaults() {
        let plan = permission_actions(&request(json!({"kind":"plan"})));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].label, "Implement");
        assert!(!plan[0].allow);
        let custom = permission_actions(&request(json!({"actions":[
            {"id":"always","label":"Always allow","behavior":"allow"},
            {"id":"no","label":"No","behavior":"deny","variant":"danger"}
        ]})));
        assert_eq!(custom[0].id, "always");
        assert!(!custom[1].allow);
    }

    #[test]
    fn questions_parse_options() {
        let parsed = questions(&request(json!({"kind":"question","input":{"questions":[
            {"header":"Color","question":"Pick one","options":[{"label":"Red"},{"label":"Blue","description":"Calm"}],"multiSelect":true}
        ]}})));
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].multi_select);
        assert_eq!(parsed[0].options[1].1.as_deref(), Some("Calm"));
    }

    #[gpui::test]
    fn buffer_zoom_zooms_the_chat(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            let ui_font_size =
                <theme_settings::ThemeSettings as settings::Settings>::get_global(cx)
                    .ui_font_size(cx);
            assert_eq!(crate::chat_font_size(cx), ui_font_size);
            theme_settings::increase_buffer_font_size(cx);
            theme_settings::increase_buffer_font_size(cx);
            assert_eq!(crate::chat_font_size(cx), ui_font_size + px(2.));
            theme_settings::reset_buffer_font_size(cx);
            assert_eq!(crate::chat_font_size(cx), ui_font_size);
        });
    }
}
