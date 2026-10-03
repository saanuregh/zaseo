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
use settings::Settings as _;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
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
    notifications::NotifyTaskExt as _,
};

use crate::composer::{Composer, ComposerEvent};
use crate::sidebar::{AgentAlert, pulse_in_alert_color};
use crate::store::{
    AgentBucket, PaseoStore, StoreEvent, agent_branch, agent_bucket, agent_is_running,
    agent_project_name, agent_provider, agent_turn_started_at, agent_worktree_name,
    subagent_bucket, subagent_title,
};
use crate::stream::{ToolSummary, tool_summary};
use crate::timeline::{
    FileChange, Segment, StreamContent, StreamItem, TimelineProjection, ToolKind, ToolStatus, Turn,
    group_turns, latest_finished_turn, parse_timestamp, tool_group_label, tool_kind, tool_runs,
    turn_changes, turn_text,
};
use crate::{
    ArchiveAgent, CopyAgentId, FocusComposer, RenameAgent, ScrollToBottom, hosts, workspace_tools,
};
use editor::Editor;

/// The chat column's widest: the configured number of characters at the chat's size, taking a
/// character as a monospace font's 0.6 em, so it grows with zoom. Longer lines are slower to read.
pub(crate) fn content_max_width(cx: &App) -> Pixels {
    let characters = crate::PaseoSettings::get_global(cx).chat.line_length as f32;
    crate::chat_font_size(cx) * (characters * 0.6)
}

/// Whether an item is the agent thinking, which the chat can hide outside an open fold.
fn is_thinking(item: &StreamItem) -> bool {
    match &item.content {
        StreamContent::Reasoning { .. } => true,
        StreamContent::Tool(call) => tool_kind(call) == ToolKind::Thinking,
        _ => false,
    }
}

/// The steps of a finished turn that its "Worked for" line folds away: everything between the
/// user's message and the agent's final answer, or after the message when no answer came.
/// `None` when there is nothing between them.
pub(crate) fn folded_work<Item: std::borrow::Borrow<StreamItem>>(
    items: &[Item],
    turn: Range<usize>,
) -> Option<Range<usize>> {
    let item_at =
        |index: usize| -> Option<&StreamItem> { items.get(index).map(|item| item.borrow()) };
    let is_user = |index: &usize| {
        item_at(*index).is_some_and(|item| matches!(item.content, StreamContent::User { .. }))
    };
    let work_start = turn.clone().find(|index| !is_user(index))?;
    let final_answer = (work_start..turn.end).rev().find(|index| {
        item_at(*index).is_some_and(|item| {
            matches!(&item.content, StreamContent::Assistant { text } if !text.trim().is_empty())
        })
    });
    let work = work_start..final_answer.unwrap_or(turn.end);
    (!work.is_empty()).then_some(work)
}

/// Rows share their items and changed files with the view, so building and comparing them stays
/// cheap; `Rc<T>` compares by pointer before value when `T: Eq`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Row {
    LoadOlder {
        loading: bool,
    },
    Item {
        item: Rc<StreamItem>,
        /// A tool call's header, worked out when the rows are built.
        tool: Option<Rc<ToolSummary>>,
        expanded: bool,
        streaming: bool,
    },
    ToolGroup {
        key: u64,
        label: String,
        running: bool,
        failed: bool,
        expanded: bool,
    },
    /// A finished turn's "Worked for" line, which folds its steps before the final answer.
    TurnFold {
        key: u64,
        duration_seconds: Option<i64>,
        expanded: bool,
    },
    TurnFooter {
        /// The key of the turn's first item, which stays put when older turns load above.
        first_item_key: Option<u64>,
        /// Whether the turn has assistant text to copy, which `AgentView::turn_copy_text` holds.
        has_text: bool,
        duration_seconds: Option<i64>,
        finished_at: Option<chrono::DateTime<Utc>>,
    },
    Working {
        since: Option<chrono::DateTime<Utc>>,
        /// Off when the step just above already spins, so one spinner shows the work.
        spinner: bool,
    },
    /// The files the latest finished turn changed, like waku's changed-files card.
    Changes {
        files: Rc<Vec<FileChange>>,
        expanded: Vec<bool>,
        show_all: bool,
    },
    Spacer,
}

/// What a row shows, stable while its content changes (streaming text, a timer ticking), so a
/// row counts as new only when it first appears.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RowIdentity {
    Item(u64),
    ToolGroup(u64),
    TurnFold(u64),
    TurnFooter(Option<u64>),
    Working,
    Changes,
}

impl Row {
    pub(crate) fn identity(&self) -> Option<RowIdentity> {
        match self {
            Row::Item { item, .. } => Some(RowIdentity::Item(item.key)),
            Row::ToolGroup { key, .. } => Some(RowIdentity::ToolGroup(*key)),
            Row::TurnFold { key, .. } => Some(RowIdentity::TurnFold(*key)),
            Row::TurnFooter { first_item_key, .. } => {
                Some(RowIdentity::TurnFooter(*first_item_key))
            }
            Row::Working { .. } => Some(RowIdentity::Working),
            Row::Changes { .. } => Some(RowIdentity::Changes),
            Row::LoadOlder { .. } | Row::Spacer => None,
        }
    }
}

/// More rows than this arriving at once is a load (opening a chat, its history arriving), not
/// the conversation moving, so they appear without animating.
const MAX_ANIMATED_ARRIVALS: usize = 8;

/// The rows in `new` that should ease in: ones `old` didn't have, except on the first build,
/// for older history loaded above the rows already shown, and for bulk loads.
pub(crate) fn arriving_rows(old: &[Row], new: &[Row]) -> Vec<RowIdentity> {
    let old_identities = old.iter().filter_map(Row::identity).collect::<HashSet<_>>();
    if old_identities.is_empty() {
        return Vec::new();
    }
    let first_kept = new.iter().position(|row| {
        row.identity()
            .is_some_and(|identity| old_identities.contains(&identity))
    });
    let arriving = new
        .iter()
        .enumerate()
        .filter(|(index, _)| first_kept.is_some_and(|first_kept| *index > first_kept))
        .filter_map(|(_, row)| row.identity())
        .filter(|identity| !old_identities.contains(identity))
        .collect::<Vec<_>>();
    if arriving.len() > MAX_ANIMATED_ARRIVALS {
        Vec::new()
    } else {
        arriving
    }
}

/// The old rows to replace and how many new rows replace them, leaving out the rows both start
/// and end with so those keep their measured heights; `None` when nothing changed.
pub(crate) fn row_splice(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let common_prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    if common_prefix == old.len() && common_prefix == new.len() {
        return None;
    }
    let common_suffix = old
        .iter()
        .skip(common_prefix)
        .rev()
        .zip(new.iter().skip(common_prefix).rev())
        .take_while(|(old, new)| old == new)
        .count();
    Some((
        common_prefix..old.len() - common_suffix,
        new.len() - common_prefix - common_suffix,
    ))
}

/// A section the user opened, whose body fades in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OpenedSection {
    Step(u64),
    Change(String),
    Subagents,
}

pub struct AgentView {
    pub(crate) store: Entity<PaseoStore>,
    pub(crate) agent_id: Option<String>,
    pub(crate) composer: Entity<Composer>,
    focus_handle: FocusHandle,
    pub(crate) list_state: ListState,
    /// Shared so a row renders from a borrow while the view is mutably borrowed.
    pub(crate) rows: Rc<Vec<Row>>,
    pub(crate) projection: TimelineProjection,
    pub(crate) turns: Vec<Turn>,
    row_caches: RowCaches,
    /// Set while a rebuild waits for the end of the effect cycle.
    rebuild_pending: bool,
    pub(crate) markdown: HashMap<(u64, u8), Entity<Markdown>>,
    /// The item each markdown was last synced from. Items are immutable once shared, so the same
    /// item means the same text, and a render skips scanning and comparing it.
    markdown_sources: HashMap<(u64, u8), Rc<StreamItem>>,
    /// The image destinations each markdown linked when created, so fetched images are dropped
    /// with the last markdown that shows them.
    markdown_images: HashMap<(u64, u8), Vec<String>>,
    /// The chat's markdown style, built once per render and shared by its rows.
    pub(crate) markdown_style: Option<MarkdownStyle>,
    pub(crate) expanded: HashSet<u64>,
    /// Open tool groups, keyed by their first call's key. Kept apart from `expanded`, which holds
    /// that call's own open state under the same key.
    pub(crate) expanded_groups: HashSet<u64>,
    /// Finished turns the user opened or closed away from the setting's default, keyed by the
    /// first folded item.
    pub(crate) expanded_turns: HashSet<u64>,
    pub(crate) expanded_changes: HashSet<String>,
    pub(crate) show_all_changes: bool,
    subagents_expanded: bool,
    /// When each row that arrived while the chat was open appeared, so it eases in once.
    pub(crate) row_appeared_at: HashMap<RowIdentity, Instant>,
    /// When each section was last opened, so its body fades in once.
    pub(crate) opened_at: HashMap<OpenedSection, Instant>,
    /// The agent's directory, whose daemon terminals this view keeps listed.
    terminal_directory: Option<String>,
    pub(crate) workspace: Option<WeakEntity<Workspace>>,
    question_answers: HashMap<(String, usize), Vec<String>>,
    question_index: HashMap<String, usize>,
    question_inputs: RefCell<HashMap<(String, usize), (Entity<Editor>, Subscription)>>,
    ticker: Option<Task<()>>,
    title: SharedString,
    bucket: Option<AgentBucket>,
    /// Images agents link by path, fetched from the daemon's host once each and read by the
    /// markdown image resolver, which can't fetch on its own.
    images: Rc<RefCell<HashMap<String, Arc<Image>>>>,
    requested_images: HashSet<String>,
    code_spans: Rc<RefCell<CodeSpanCache>>,
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
    /// The chat settings the rows were last built with.
    chat_settings: Option<crate::ChatSettings>,
    _subscriptions: Vec<Subscription>,
}

/// What `AgentView::rebuild` reads from the store, compared to skip rebuilds for other agents.
/// Rows read agent data only through it, so any change that affects them also changes it.
#[derive(Clone, PartialEq)]
struct RebuildInputs {
    generation: u64,
    epoch: Option<String>,
    timeline_revision: u64,
    timeline_rewrite: u64,
    has_older: bool,
    loading_older: bool,
    running: bool,
    turn_started: Option<chrono::DateTime<Utc>>,
    title: Option<String>,
    bucket: Option<AgentBucket>,
    directory: Option<PathBuf>,
}

/// Where a turn sits: still running, or the latest finished one, whose changed files show.
#[derive(Clone, Copy)]
struct TurnPlacement {
    live: bool,
    latest_finished: bool,
}

/// Row parts that are slow to work out and rarely change, kept between rebuilds.
#[derive(Default)]
struct RowCaches {
    /// Finished turns by their first item's key.
    turns: HashMap<u64, TurnCache>,
    /// Tool call headers by item key, with the item they were worked out from.
    tools: HashMap<u64, (Rc<StreamItem>, Rc<ToolSummary>)>,
    /// The agent directory tool paths were made relative to.
    tools_directory: Option<PathBuf>,
}

/// A finished turn's copy text and changed files, valid while the turn holds the same items.
struct TurnCache {
    items: Vec<Rc<StreamItem>>,
    copy_text: SharedString,
    changes: Option<Rc<Vec<FileChange>>>,
}

impl RowCaches {
    fn turn(&mut self, turn_items: &[Rc<StreamItem>]) -> Option<&mut TurnCache> {
        let key = turn_items.first()?.key;
        let current = self.turns.get(&key).is_some_and(|cache| {
            cache.items.len() == turn_items.len()
                && cache
                    .items
                    .iter()
                    .zip(turn_items)
                    .all(|(cached, item)| Rc::ptr_eq(cached, item))
        });
        if !current {
            self.turns.insert(
                key,
                TurnCache {
                    items: turn_items.to_vec(),
                    copy_text: turn_text(turn_items).into(),
                    changes: None,
                },
            );
        }
        self.turns.get_mut(&key)
    }

    fn turn_changes(&mut self, turn_items: &[Rc<StreamItem>]) -> Rc<Vec<FileChange>> {
        match self.turn(turn_items) {
            Some(cache) => cache
                .changes
                .get_or_insert_with(|| Rc::new(turn_changes(turn_items)))
                .clone(),
            None => Rc::default(),
        }
    }

    fn tool(&mut self, item: &Rc<StreamItem>) -> Option<Rc<ToolSummary>> {
        let StreamContent::Tool(call) = &item.content else {
            return None;
        };
        if let Some((cached_item, summary)) = self.tools.get(&item.key)
            && Rc::ptr_eq(cached_item, item)
        {
            return Some(summary.clone());
        }
        let summary = Rc::new(tool_summary(call, self.tools_directory.as_deref()));
        self.tools.insert(item.key, (item.clone(), summary.clone()));
        Some(summary)
    }

    fn set_directory(&mut self, directory: Option<PathBuf>) {
        if directory != self.tools_directory {
            self.tools.clear();
            self.tools_directory = directory;
        }
    }

    fn retain(&mut self, live_keys: &HashSet<u64>) {
        self.turns.retain(|key, _| live_keys.contains(key));
        self.tools.retain(|key, _| live_keys.contains(key));
    }
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
    /// A chat on its agent's host, or a draft on the default host.
    pub fn new(
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = agent_id
            .as_deref()
            .and_then(|agent_id| hosts::store_for_agent(agent_id, cx))
            .unwrap_or_else(|| hosts::default_store(cx));
        Self::on_host(store, agent_id, directory, workspace, window, cx)
    }

    pub(crate) fn on_host(
        store: Entity<PaseoStore>,
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer =
            cx.new(|cx| Composer::new(store.clone(), agent_id.clone(), directory, window, cx));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(2048.));
        list_state.set_follow_mode(gpui::FollowMode::Tail);
        let mut subscriptions = vec![
            cx.observe(&store, |view: &mut Self, _, cx| {
                view.forget_resolved_questions(cx);
                view.rebuild_if_inputs_changed(cx)
            }),
            cx.subscribe(&store, |view: &mut Self, _, event: &StoreEvent, cx| {
                if let StoreEvent::TimelineChanged(timeline_id) = event
                    && view.agent_id.as_deref() == Some(timeline_id.as_str())
                {
                    view.schedule_rebuild(cx);
                }
            }),
            cx.subscribe_in(&composer, window, Self::handle_composer_event),
            // Folding and thinking settings change which rows exist.
            cx.observe_global::<settings::SettingsStore>(|view: &mut Self, cx| {
                let chat = crate::PaseoSettings::get_global(cx).chat.clone();
                if view.chat_settings.as_ref() != Some(&chat) {
                    view.chat_settings = Some(chat);
                    view.rebuild(cx);
                }
            }),
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
            rows: Rc::default(),
            projection: TimelineProjection::default(),
            turns: Vec::new(),
            row_caches: RowCaches::default(),
            rebuild_pending: false,
            markdown: HashMap::new(),
            markdown_sources: HashMap::new(),
            markdown_images: HashMap::new(),
            markdown_style: None,
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
            expanded_groups: HashSet::new(),
            expanded_turns: HashSet::new(),
            expanded_changes: HashSet::new(),
            show_all_changes: false,
            subagents_expanded: false,
            row_appeared_at: HashMap::new(),
            opened_at: HashMap::new(),
            terminal_directory: None,
            workspace,
            question_answers: HashMap::new(),
            question_index: HashMap::new(),
            question_inputs: RefCell::default(),
            ticker: None,
            title: "New agent".into(),
            bucket: None,
            chat_settings: None,
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
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let parent_agent_id = parent_agent_id.to_owned();
        let subagent_id = subagent_id.to_owned();
        crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
            crate::open_subagent(workspace, &parent_agent_id, &subagent_id, window, cx)
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
        hosts::focus_agent_on(&self.store, agent_id.clone(), cx);
        self.store
            .update(cx, |store, cx| store.clear_attention(&agent_id, cx));
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
                self.store.update(cx, |store, cx| store.watch(agent_id, cx));
                hosts::focus_agent_on(&self.store, agent_id.clone(), cx);
                self.rebuild(cx);
                if let Some(workspace) = self.workspace.clone() {
                    let view = cx.entity();
                    let agent_id = agent_id.clone();
                    crate::defer_workspace_update(
                        workspace,
                        window,
                        cx,
                        move |workspace, window, cx| {
                            let tab = workspace
                                .items_of_type::<AgentTab>(cx)
                                .find(|tab| tab.read(cx).view == view);
                            if let Some(tab) = tab {
                                crate::follow_created_agent(workspace, tab, &agent_id, window, cx);
                            }
                        },
                    );
                }
            }
            ComposerEvent::ClearRequested {
                directory,
                workspace_id,
            } => {
                let (directory, workspace_id) = (directory.clone(), workspace_id.clone());
                if let Some(workspace) = self.workspace.clone() {
                    crate::defer_workspace_update(
                        workspace,
                        window,
                        cx,
                        move |workspace, window, cx| {
                            crate::open_draft_joining(
                                workspace,
                                directory,
                                workspace_id,
                                window,
                                cx,
                            );
                        },
                    );
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

    pub(crate) fn load_older(&mut self, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.agent_id.clone() {
            self.store
                .update(cx, |store, cx| store.load_older(&agent_id, cx));
        }
    }

    pub(crate) fn toggle_turn(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_turns.remove(&key) {
            self.expanded_turns.insert(key);
        }
        self.rebuild(cx);
    }

    pub(crate) fn toggle_group(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_groups.remove(&key) {
            self.expanded_groups.insert(key);
        }
        self.rebuild(cx);
    }

    pub(crate) fn toggle_expanded(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
            self.mark_opened(OpenedSection::Step(key));
        }
        self.rebuild(cx);
    }

    pub(crate) fn mark_opened(&mut self, section: OpenedSection) {
        let now = Instant::now();
        self.opened_at
            .retain(|_, opened_at| now.duration_since(*opened_at) < crate::stream::ENTRANCE * 2);
        self.opened_at.insert(section, now);
    }

    /// The store notifies for every agent's changes, so skip rebuilding when nothing this view
    /// reads has changed.
    fn rebuild_if_inputs_changed(&mut self, cx: &mut Context<Self>) {
        if !self.rebuild_pending && self.rebuild_inputs(cx) != self.last_rebuild_inputs {
            self.schedule_rebuild(cx);
        }
    }

    /// Rebuilds once the current effect cycle ends, so the store changes of one cycle cost one
    /// rebuild.
    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        if self.rebuild_pending {
            return;
        }
        self.rebuild_pending = true;
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Err(error) = this.update(cx, |view, cx| {
                if view.rebuild_pending {
                    view.rebuild(cx);
                }
            }) {
                log::debug!("Paseo agent view released before rebuilding: {error}");
            }
        });
    }

    fn rebuild_inputs(&self, cx: &App) -> Option<RebuildInputs> {
        let agent_id = self.agent_id.as_deref()?;
        let store = self.store.read(cx);
        let agent = store.agent(agent_id);
        let subagent = store.subagent(agent_id);
        let paging = store.paging.get(agent_id);
        let has_permission = store
            .state
            .permissions
            .values()
            .any(|request| request.agent_id == agent_id);
        Some(RebuildInputs {
            generation: store.connection_generation,
            epoch: store.state.current_epoch(agent_id).map(str::to_owned),
            timeline_revision: store.state.timeline_revision(agent_id),
            timeline_rewrite: store.state.timeline_rewrite(agent_id),
            has_older: paging.is_some_and(|paging| paging.has_older),
            loading_older: paging.is_some_and(|paging| paging.loading_older),
            running: agent.is_some_and(agent_is_running)
                || subagent.is_some_and(|subagent| subagent.status == "running"),
            turn_started: agent
                .and_then(agent_turn_started_at)
                .or_else(|| subagent.and_then(|subagent| parse_timestamp(&subagent.created_at))),
            title: agent
                .map(|agent| store.display_title(agent))
                .or_else(|| subagent.map(subagent_title)),
            bucket: agent
                .map(|agent| agent_bucket(agent, has_permission))
                .or_else(|| subagent.map(subagent_bucket)),
            directory: store.timeline_directory(agent_id),
        })
    }

    /// Recomputes the display rows from the store and splices only the rows that changed, so the
    /// list keeps measured heights and its tail-follow position while chunks stream in.
    pub(crate) fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild_pending = false;
        let inputs = self.rebuild_inputs(cx);
        self.last_rebuild_inputs = inputs.clone();
        let (Some(agent_id), Some(inputs)) = (self.agent_id.clone(), inputs) else {
            let live_keys = self.live_item_keys();
            self.apply_rows(Vec::new(), &live_keys, cx);
            return;
        };
        let store = self.store.read(cx);
        self.projection.sync(
            inputs.epoch.as_deref(),
            inputs.timeline_revision,
            inputs.timeline_rewrite,
            || store.entries_for(&agent_id),
        );
        self.sync_terminal_directory(cx);
        if let Some(title) = inputs.title.clone() {
            let title = SharedString::from(title);
            if title != self.title || inputs.bucket != self.bucket {
                self.title = title;
                self.bucket = inputs.bucket;
                cx.emit(AgentViewEvent::TabChanged);
            }
        }
        self.turns = group_turns(self.projection.items());
        let live_keys = self.live_item_keys();
        let chat = crate::PaseoSettings::get_global(cx).chat.clone();
        let rows = self.build_rows(&inputs, &live_keys, &chat);
        self.apply_rows(rows, &live_keys, cx);
        self.update_ticker(inputs.running, cx);
    }

    fn live_item_keys(&self) -> HashSet<u64> {
        self.projection
            .items()
            .iter()
            .map(|item| item.key)
            .collect()
    }

    fn build_rows(
        &mut self,
        inputs: &RebuildInputs,
        live_keys: &HashSet<u64>,
        chat: &crate::ChatSettings,
    ) -> Vec<Row> {
        let mut caches = std::mem::take(&mut self.row_caches);
        caches.set_directory(inputs.directory.clone());
        let items = self.projection.items();
        let mut rows = Vec::with_capacity(items.len() + self.turns.len() + 2);
        if inputs.has_older {
            rows.push(Row::LoadOlder {
                loading: inputs.loading_older,
            });
        }
        let last_turn = self.turns.len().saturating_sub(1);
        let last_finished_turn = latest_finished_turn(self.turns.len(), inputs.running);
        for (turn_index, turn) in self.turns.iter().enumerate() {
            let placement = TurnPlacement {
                live: inputs.running && turn_index == last_turn,
                latest_finished: Some(turn_index) == last_finished_turn,
            };
            self.push_turn_rows(&mut rows, turn, placement, chat, &mut caches);
        }
        if inputs.running {
            let step_spins = rows.last().is_some_and(|row| match row {
                Row::ToolGroup { running, .. } => *running,
                Row::Item {
                    item, streaming, ..
                } => match &item.content {
                    StreamContent::Tool(call) => call.status == ToolStatus::Running,
                    StreamContent::Reasoning { .. } => *streaming,
                    _ => false,
                },
                _ => false,
            });
            rows.push(Row::Working {
                spinner: !step_spins,
                since: inputs
                    .turn_started
                    .or_else(|| self.turns.last().and_then(|turn| turn.started_at)),
            });
        }
        rows.push(Row::Spacer);
        caches.retain(live_keys);
        self.row_caches = caches;
        rows
    }

    /// One turn's rows: its items and tool groups, its fold line when finished, the changed
    /// files of the latest finished turn, and its footer.
    fn push_turn_rows(
        &self,
        rows: &mut Vec<Row>,
        turn: &Turn,
        placement: TurnPlacement,
        chat: &crate::ChatSettings,
        caches: &mut RowCaches,
    ) {
        let items = self.projection.items();
        let item_row = |index: usize, caches: &mut RowCaches| {
            let item = items.get(index)?;
            Some(Row::Item {
                expanded: crate::stream::is_expandable(item) && self.expanded.contains(&item.key),
                tool: caches.tool(item),
                item: item.clone(),
                streaming: placement.live && index + 1 == turn.items.end,
            })
        };
        let fold = if placement.live {
            None
        } else {
            folded_work(items, turn.items.clone())
        };
        let duration_seconds = turn
            .started_at
            .zip(turn.ended_at)
            .map(|(start, end)| (end - start).num_seconds())
            .filter(|seconds| *seconds > 0);
        let fold_key = fold
            .as_ref()
            .and_then(|work| items.get(work.start))
            .map(|item| item.key);
        let fold_open = fold_key
            .is_some_and(|key| self.expanded_turns.contains(&key) == chat.fold_finished_turns);
        for segment in tool_runs(items, turn.items.clone()) {
            let start = match &segment {
                Segment::Item(index) => *index,
                Segment::ToolRun(run) => run.start,
            };
            let in_open_fold = fold
                .as_ref()
                .is_some_and(|work| work.contains(&start) && fold_open);
            if let (Some(work), Some(key)) = (&fold, fold_key) {
                if start == work.start {
                    rows.push(Row::TurnFold {
                        key,
                        duration_seconds,
                        expanded: fold_open,
                    });
                }
                if work.contains(&start) && !fold_open {
                    continue;
                }
            }
            if !chat.show_thinking
                && !in_open_fold
                && matches!(&segment, Segment::Item(index) if items.get(*index).is_some_and(|item| is_thinking(item)))
            {
                continue;
            }
            let run = match segment {
                Segment::Item(index) => {
                    rows.extend(item_row(index, caches));
                    continue;
                }
                Segment::ToolRun(run) => run,
            };
            let Some(run_items) = items.get(run.clone()) else {
                continue;
            };
            let Some(key) = run_items.first().map(|item| item.key) else {
                continue;
            };
            let calls = run_items
                .iter()
                .filter_map(|item| match &item.content {
                    StreamContent::Tool(call) => Some(call),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let expanded = self.expanded_groups.contains(&key);
            rows.push(Row::ToolGroup {
                key,
                label: tool_group_label(calls.iter().copied()),
                running: calls.iter().any(|call| call.status == ToolStatus::Running),
                failed: calls.iter().any(|call| call.status == ToolStatus::Failed),
                expanded,
            });
            if expanded {
                rows.extend(run.filter_map(|index| item_row(index, caches)));
            }
        }
        let turn_items = items.get(turn.items.clone()).unwrap_or_default();
        if placement.latest_finished {
            let files = caches.turn_changes(turn_items);
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
        if !placement.live {
            rows.push(Row::TurnFooter {
                first_item_key: turn_items.first().map(|item| item.key),
                has_text: caches
                    .turn(turn_items)
                    .is_some_and(|cache| !cache.copy_text.is_empty()),
                // The fold line above already says how long the turn took.
                duration_seconds: duration_seconds.filter(|_| fold.is_none()),
                finished_at: turn.ended_at,
            });
        }
    }

    /// The copyable text of the finished turn starting at `first_item_key`.
    pub(crate) fn turn_copy_text(&self, first_item_key: Option<u64>) -> Option<SharedString> {
        let cache = self.row_caches.turns.get(&first_item_key?)?;
        Some(cache.copy_text.clone())
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

    fn apply_rows(&mut self, rows: Vec<Row>, live_keys: &HashSet<u64>, cx: &mut Context<Self>) {
        if let Some((replaced, inserted)) = row_splice(&self.rows, &rows) {
            let now = Instant::now();
            self.row_appeared_at.retain(|_, appeared_at| {
                now.duration_since(*appeared_at) < crate::stream::ENTRANCE * 2
            });
            for identity in arriving_rows(&self.rows, &rows) {
                self.row_appeared_at.insert(identity, now);
            }
            self.list_state.splice(replaced, inserted);
            self.rows = Rc::new(rows);
        }
        self.prune_markdown(live_keys);
        cx.notify();
    }

    /// Drops markdown, and the images only it showed, for items no longer in the timeline.
    fn prune_markdown(&mut self, live_keys: &HashSet<u64>) {
        self.markdown.retain(|(key, _), _| live_keys.contains(key));
        self.markdown_sources
            .retain(|(key, _), _| live_keys.contains(key));
        let images_before = self.markdown_images.len();
        self.markdown_images
            .retain(|(key, _), _| live_keys.contains(key));
        if self.markdown_images.len() == images_before {
            return;
        }
        let linked = self
            .markdown_images
            .values()
            .flatten()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        self.images
            .borrow_mut()
            .retain(|destination, _| linked.contains(destination.as_str()));
        self.requested_images
            .retain(|destination| linked.contains(destination.as_str()));
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

    /// The markdown for `text`, a field of `source`, synced to the text only when `source`
    /// changed since the last call.
    pub(crate) fn markdown_for(
        &mut self,
        source: &Rc<StreamItem>,
        role: u8,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        let key = (source.key, role);
        if let Some(markdown) = self.markdown.get(&key)
            && self
                .markdown_sources
                .get(&key)
                .is_some_and(|synced| Rc::ptr_eq(synced, source))
        {
            return markdown.clone();
        }
        self.markdown_sources.insert(key, source.clone());
        let text = separate_images(text);
        let text = text.as_ref();
        if let Some(markdown) = self.markdown.get(&key) {
            let current = markdown.read(cx).source();
            if current.as_ref() != text {
                let delta = text
                    .strip_prefix(current.as_ref())
                    .filter(|_| !current.is_empty());
                markdown.update(cx, |markdown, cx| match delta {
                    Some(delta) => markdown.append(delta, cx),
                    None => markdown.reset(text.to_owned().into(), cx),
                });
            }
            return markdown.clone();
        }
        // The editor's language registry highlights code blocks the way the editor does.
        let languages = self
            .workspace
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .map(|workspace| workspace.read(cx).app_state().languages.clone());
        let markdown = cx.new(|cx| Markdown::new(text.to_owned().into(), languages, None, cx));
        self.markdown.insert(key, markdown.clone());
        self.unparsed_markdown.insert(key);
        self.load_images(key, text, cx);
        markdown
    }

    /// A markdown element that shows the images this view fetched for its links.
    pub(crate) fn markdown_element(
        &self,
        markdown: Entity<Markdown>,
        style: MarkdownStyle,
        cx: &Context<Self>,
    ) -> MarkdownElement {
        let images = self.images.clone();
        let links = ThreadLinks {
            view: cx.weak_entity(),
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

    fn load_images(&mut self, markdown_key: (u64, u8), text: &str, cx: &mut Context<Self>) {
        let directory = self.directory(cx);
        let destinations = image_destinations(text);
        if !destinations.is_empty() {
            self.markdown_images.insert(
                markdown_key,
                destinations
                    .iter()
                    .map(|destination| (*destination).to_owned())
                    .collect(),
            );
        }
        for destination in destinations {
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
        workspace.update(cx, |workspace, cx| {
            workspace_tools::open_agent_rename(workspace, agent_id, window, cx);
        });
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Whether the composer holds text the user hasn't sent.
    pub(crate) fn has_unsent_text(&self, cx: &App) -> bool {
        !self.composer.read(cx).text(cx).trim().is_empty()
    }

    pub(crate) fn toggle_answer(
        &mut self,
        request_id: &str,
        question: usize,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self
            .store
            .read(cx)
            .state
            .permissions
            .get(request_id)
            .cloned();
        let multi_select = request
            .as_ref()
            .and_then(|request| {
                questions(request)
                    .get(question)
                    .map(|question| question.multi_select)
            })
            .unwrap_or(false);
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
        let picked = !answers.is_empty();
        // Like Paseo and Claude Code, a single-choice pick and an Other answer replace each
        // other, and a pick moves on to the next question.
        if !multi_select && picked {
            let input = self
                .question_inputs
                .borrow()
                .get(&(request_id.to_owned(), question))
                .map(|(editor, _)| editor.clone());
            if let Some(editor) = input {
                editor.update(cx, |editor, cx| editor.set_text("", window, cx));
            }
            if let Some(request) = request.filter(Self::uses_question_steps) {
                let count = questions(&request).len();
                if self.current_question(request_id, count) == question && question + 1 < count {
                    self.advance_question(&request, window, cx);
                }
            }
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
        let response = self.permission_response(request, allow, action_id, cx);
        let request_id = request.request_id.clone();
        self.store.update(cx, |store, cx| {
            store.respond_permission(request_id, response, cx)
        });
    }

    fn permission_response(
        &self,
        request: &PermissionRequest,
        allow: bool,
        action_id: Option<String>,
        cx: &App,
    ) -> PermissionResponse {
        let is_question = request.extra.get("kind").and_then(Value::as_str) == Some("question");
        let questions = questions(request);
        // Like Paseo, dismissing a form of optional free-text questions submits it empty, with
        // no action, since the dismiss action would deny it.
        let (allow, action_id) = if !allow && is_question && submit_empty_on_dismiss(&questions) {
            (true, None)
        } else {
            (allow, action_id)
        };
        if allow {
            let updated_input = is_question.then(|| {
                let mut input = request
                    .extra
                    .get("input")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::Object(Default::default()));
                let answers = build_answers(
                    &questions,
                    |index| self.question_selected(&request.request_id, index),
                    |index| self.question_text(&request.request_id, index, cx),
                );
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
        }
    }

    fn primary_permission(&self, cx: &App) -> Option<PermissionRequest> {
        let agent_id = self.agent_id.as_deref()?;
        self.store
            .read(cx)
            .permissions_for(agent_id)
            .into_iter()
            .next()
    }

    /// Answers stay until the daemon resolves the request, so a failed submit keeps what was
    /// typed.
    fn forget_resolved_questions(&mut self, cx: &App) {
        let store = self.store.read(cx);
        let pending = |request_id: &str| store.state.permissions.contains_key(request_id);
        self.question_answers
            .retain(|(request_id, _), _| pending(request_id));
        self.question_index
            .retain(|request_id, _| pending(request_id));
        self.question_inputs
            .borrow_mut()
            .retain(|(request_id, _), _| pending(request_id));
    }

    fn question_selected(&self, request_id: &str, index: usize) -> Vec<String> {
        self.question_answers
            .get(&(request_id.to_owned(), index))
            .cloned()
            .unwrap_or_default()
    }

    fn question_text(&self, request_id: &str, index: usize, cx: &App) -> String {
        self.question_inputs
            .borrow()
            .get(&(request_id.to_owned(), index))
            .map(|(editor, _)| editor.read(cx).text(cx))
            .unwrap_or_default()
    }

    fn current_question(&self, request_id: &str, count: usize) -> usize {
        self.question_index
            .get(request_id)
            .copied()
            .unwrap_or(0)
            .min(count.saturating_sub(1))
    }

    fn question_is_answered(
        &self,
        request_id: &str,
        index: usize,
        question: &Question,
        cx: &App,
    ) -> bool {
        is_answered(
            question,
            &self.question_selected(request_id, index),
            &self.question_text(request_id, index, cx),
        )
    }

    /// Whether the card walks through its questions with Next, which it does unless the daemon
    /// supplied its own actions for the request.
    fn uses_question_steps(request: &PermissionRequest) -> bool {
        request.extra.get("kind").and_then(Value::as_str) == Some("question")
            && request
                .extra
                .get("actions")
                .and_then(Value::as_array)
                .is_none_or(|actions| actions.is_empty())
    }

    /// Next on a question that isn't the last, Submit on the last once every question is
    /// answered.
    fn advance_question(
        &mut self,
        request: &PermissionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let questions = questions(request);
        let request_id = request.request_id.clone();
        let index = self.current_question(&request_id, questions.len());
        let Some(question) = questions.get(index) else {
            self.respond(request, true, Some("accept".into()), cx);
            return;
        };
        if index + 1 < questions.len() {
            if self.question_is_answered(&request_id, index, question, cx) {
                self.question_index.insert(request_id.clone(), index + 1);
                if let Some(next) = questions
                    .get(index + 1)
                    .filter(|next| shows_text_input(next))
                {
                    self.question_input(
                        &request_id,
                        index + 1,
                        &question_placeholder(next),
                        window,
                        cx,
                    );
                }
                self.focus_question_input(&request_id, index + 1, window, cx);
                cx.notify();
            }
            return;
        }
        let all_answered = questions
            .iter()
            .enumerate()
            .all(|(index, question)| self.question_is_answered(&request_id, index, question, cx));
        if all_answered {
            self.respond(request, true, Some("accept".into()), cx);
        }
    }

    fn focus_question_input(
        &self,
        request_id: &str,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self
            .question_inputs
            .borrow()
            .get(&(request_id.to_owned(), index))
            .map(|(editor, _)| editor.clone());
        match editor {
            Some(editor) => window.focus(&editor.focus_handle(cx), cx),
            None => window.focus(&self.focus_handle, cx),
        }
    }

    fn question_input(
        &self,
        request_id: &str,
        index: usize,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let key = (request_id.to_owned(), index);
        if let Some((editor, _)) = self.question_inputs.borrow().get(&key) {
            return editor.clone();
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor
        });
        // Next and Submit enable as the answer is typed, and typed text replaces a single-choice
        // pick.
        let subscription = cx.subscribe(&editor, {
            let key = key.clone();
            move |view, editor, event: &editor::EditorEvent, cx| {
                if !matches!(event, editor::EditorEvent::BufferEdited) {
                    return;
                }
                let multi_select = view
                    .store
                    .read(cx)
                    .state
                    .permissions
                    .get(&key.0)
                    .and_then(|request| {
                        questions(request)
                            .get(key.1)
                            .map(|question| question.multi_select)
                    })
                    .unwrap_or(false);
                if !multi_select && !editor.read(cx).text(cx).trim().is_empty() {
                    view.question_answers.remove(&key);
                }
                cx.notify();
            }
        });
        self.question_inputs
            .borrow_mut()
            .insert(key, (editor.clone(), subscription));
        editor
    }

    fn allow_first(
        &mut self,
        _: &crate::AllowPermission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = self.primary_permission(cx) {
            if Self::uses_question_steps(&request) {
                self.advance_question(&request, window, cx);
                return;
            }
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
            &store.archived_subagents.items(),
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
                    // The provider's own summary (for Claude: type, model, effort and tokens),
                    // shown as given; Paseo's schema asks clients not to parse it.
                    let detail = subagent.subtitle.clone();
                    let timeline_id =
                        paseo_client::subagent_timeline_id(&parent_agent_id, &subagent.id);
                    let open = (parent_agent_id.clone(), subagent.id.clone());
                    let run_time = subagent_run_time(subagent, now);
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
                        .children(run_time.map(|run_time| {
                            Label::new(run_time)
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
            // A tab on the composer's top edge, narrower than it, so the two read as one piece.
            v_flex()
                .id("paseo-subagent-track")
                .mx_3()
                .rounded_t(rems_from_px(crate::stream::CARD_RADIUS))
                .border_1()
                .border_b_0()
                .border_color(colors.border)
                .bg(colors.elevated_surface_background)
                .overflow_hidden()
                .child(
                    h_flex()
                        .id("paseo-subagent-track-header")
                        .px_3()
                        .py_1()
                        .gap_1p5()
                        .cursor_pointer()
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.subagents_expanded = !view.subagents_expanded;
                            view.mark_opened(OpenedSection::Subagents);
                            cx.notify();
                        }))
                        .child(
                            Icon::new(IconName::ListTree)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(summary)
                                .size(crate::stream::STEP_LABEL_SIZE)
                                .color(Color::Muted),
                        )
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
                        .child(crate::stream::rotating_chevron(
                            "paseo-subagent-chevron",
                            IconName::ChevronUp,
                            IconSize::Small,
                            self.subagents_expanded,
                            0.5,
                            cx,
                        )),
                )
                .when_some(rows, |this, rows| {
                    this.child(crate::stream::fade_in_since(
                        v_flex()
                            .id("paseo-subagent-rows")
                            .max_h(rems_from_px(240_f32))
                            .overflow_y_scroll()
                            .track_scroll(&self.subagent_scroll)
                            .children(rows)
                            .vertical_scrollbar_for(&self.subagent_scroll, window, cx),
                        "paseo-subagent-rows-fade",
                        self.opened_at.get(&OpenedSection::Subagents).copied(),
                        px(0.),
                    ))
                })
                .into_any_element(),
        )
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
        let bucket = self.bucket.unwrap_or(AgentBucket::Done);
        let notice = if crate::store::agent_provider_unavailable(agent) {
            Some((
                Color::Warning,
                format!(
                    "The {} provider isn't available on this host, so this agent can't take messages.",
                    agent_provider(agent)
                ),
            ))
        } else if bucket == AgentBucket::Failed {
            crate::store::agent_last_error(agent)
                .map(|error| (Color::Error, format!("Failed: {error}")))
        } else {
            None
        };
        // The tab, the title bar and the checkout footer show what this header used to.
        let (color, message) = notice?;
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .flex_none()
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_start()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .bg(color.color(cx).opacity(0.08))
                        .child(
                            div().pt_0p5().child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(color),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(3)
                                .text_ellipsis()
                                .child(Label::new(message).size(LabelSize::Small).color(color)),
                        ),
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
        let request_id = request.request_id.clone();
        let mut card = Self::permission_card(primary, kind, &request, cx);
        if kind == "question" {
            card = card.child(self.render_questions(&request, window, cx));
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
        card.child(
            Label::new(if kind == "question" {
                "Answer to continue"
            } else {
                "How would you like to proceed?"
            })
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(self.render_permission_buttons(primary, &request, cx))
        .into_any_element()
    }

    /// A permission card's frame and header: the request's kind, title and description.
    fn permission_card(
        primary: bool,
        kind: &str,
        request: &PermissionRequest,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
        // A question's full text is in the card body, so its header only names the kind.
        let title = match kind {
            "plan" => "Plan".to_owned(),
            "question" => "Question".to_owned(),
            _ => request.title.clone(),
        };
        v_flex()
            .id(SharedString::from(format!(
                "paseo-permission-{}",
                request.request_id
            )))
            .w_full()
            .p_3()
            .gap_2()
            .rounded(rems_from_px(crate::stream::CARD_RADIUS))
            .map(|card| crate::stream::raised_card(card, cx))
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
            })
    }

    /// A permission's actions; a stepped question's allow action moves to the next question
    /// until the last one.
    fn render_permission_buttons(
        &self,
        primary: bool,
        request: &PermissionRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let request_id = &request.request_id;
        let actions = permission_actions(request);
        let question_steps = Self::uses_question_steps(request);
        let step = question_steps.then(|| {
            let questions = questions(request);
            let index = self.current_question(request_id, questions.len());
            let is_last = index + 1 >= questions.len();
            let ready = if is_last {
                questions.iter().enumerate().all(|(index, question)| {
                    self.question_is_answered(request_id, index, question, cx)
                })
            } else {
                questions.get(index).is_some_and(|question| {
                    self.question_is_answered(request_id, index, question, cx)
                })
            };
            (is_last, ready)
        });
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
            let steps_here = step.filter(|_| allow);
            let label = match steps_here {
                Some((false, _)) => "Next".to_owned(),
                _ => action.label,
            };
            buttons = buttons.child(
                ui::Button::new(
                    SharedString::from(format!("paseo-permission-{}-{}", request_id, action.id)),
                    label,
                )
                .disabled(steps_here.is_some_and(|(_, ready)| !ready))
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
                .on_click(cx.listener(move |view, _, window, cx| {
                    if steps_here.is_some() {
                        view.advance_question(&request, window, cx);
                    } else {
                        view.respond(&request, allow, Some(action_id.clone()), cx);
                    }
                })),
            );
        }
        buttons
    }

    /// One question at a time, like Paseo: header tabs, the question's options, and a free-text
    /// field when the question offers one.
    fn render_questions(
        &self,
        request: &PermissionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let request_id = request.request_id.clone();
        let questions = questions(request);
        let index = self.current_question(&request_id, questions.len());
        let input = questions
            .get(index)
            .filter(|question| shows_text_input(question))
            .map(|question| {
                self.question_input(
                    &request_id,
                    index,
                    &question_placeholder(question),
                    window,
                    cx,
                )
            });
        let colors = cx.theme().colors();
        let tabs = (questions.len() > 1).then(|| {
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(questions.iter().enumerate().map(|(tab_index, question)| {
                    let answered = self.question_is_answered(&request_id, tab_index, question, cx);
                    let is_current = tab_index == index;
                    let label = if question.header.is_empty() {
                        format!("Question {}", tab_index + 1)
                    } else {
                        question.header.clone()
                    };
                    let request_id = request_id.clone();
                    h_flex()
                        .id(SharedString::from(format!(
                            "paseo-question-tab-{request_id}-{tab_index}"
                        )))
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .border_1()
                        .border_color(if is_current {
                            colors.border_focused
                        } else {
                            colors.border_variant
                        })
                        .when(is_current, |this| this.bg(colors.element_selected))
                        .hover(|style| style.bg(colors.element_hover))
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.question_index.insert(request_id.clone(), tab_index);
                            view.focus_question_input(&request_id, tab_index, window, cx);
                            cx.notify();
                        }))
                        .when(answered, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                        })
                        .child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .color(if is_current {
                                    Color::Default
                                } else {
                                    Color::Muted
                                }),
                        )
                }))
        });
        let Some(question) = questions.into_iter().nth(index) else {
            return v_flex().into_any_element();
        };
        let selected = self.question_selected(&request_id, index);
        let multi_select = question.multi_select;
        // A lone question has no tabs, so its header sits above it instead.
        let show_header = tabs.is_none() && !question.header.is_empty();
        v_flex()
            .gap_1()
            .children(tabs)
            .when(show_header, |this| {
                this.child(
                    Label::new(question.header.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(Label::new(question.question.clone()).size(LabelSize::Default))
            .children(
                question
                    .options
                    .into_iter()
                    .enumerate()
                    .map(|(option_index, option)| {
                        let is_selected = selected.contains(&option.0);
                        let request_id = request_id.clone();
                        let label = option.0.clone();
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
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.toggle_answer(&request_id, index, label.clone(), window, cx)
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
                                .color(if is_selected {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                }),
                            )
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(Label::new(option.0).size(LabelSize::Default))
                                    .when_some(option.1, |this, description| {
                                        this.child(
                                            Label::new(description)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                    }),
            )
            .when_some(input, |this, input| {
                let request = request.clone();
                this.child(
                    div()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.editor_background)
                        .on_action(cx.listener(move |view, _: &menu::Confirm, window, cx| {
                            if Self::uses_question_steps(&request) {
                                view.advance_question(&request, window, cx);
                            }
                        }))
                        .child(input),
                )
            })
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
            .children(self.render_draft_host_picker(cx))
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

    /// An archived agent is one the store knows only as archived, not among the host's agents.
    fn is_archived(&self, cx: &App) -> bool {
        let Some(agent_id) = self.agent_id.as_deref() else {
            return false;
        };
        let store = self.store.read(cx);
        store.state.agent(agent_id).is_none()
            && store
                .archived
                .iter()
                .flatten()
                .any(|agent| agent.id == agent_id)
    }

    fn render_archived_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.is_archived(cx) {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .px_4()
                .py_3()
                .gap_2()
                .rounded(rems_from_px(crate::stream::CARD_RADIUS))
                .map(|notice| crate::stream::raised_card(notice, cx))
                .child(
                    Icon::new(IconName::Archive)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Label::new("This agent is archived").color(Color::Muted)),
                )
                .child(
                    ui::Button::new("paseo-unarchive-agent", "Unarchive")
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|view, _, _, cx| {
                            if let Some(agent_id) = view.agent_id.clone() {
                                view.store
                                    .update(cx, |store, cx| store.unarchive(&agent_id, cx));
                            }
                        })),
                )
                .into_any_element(),
        )
    }

    /// A tab under the composer naming where the agent works and its branch. Drafts pick there
    /// between the project's checkout and a new worktree, and the new worktree's base.
    fn render_checkout_footer(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let (location, branch, diff_stat) = match self.agent_id.as_deref() {
            Some(agent_id) => {
                let agent = store.agent(agent_id)?;
                let diff_stat = crate::store::agent_workspace_id(agent)
                    .and_then(|workspace_id| store.state.workspaces.get(workspace_id))
                    .and_then(|workspace| workspace.diff_stat);
                let location = match agent_worktree_name(agent) {
                    Some(name) => checkout_label(IconName::GitWorktree, name),
                    None => checkout_label(IconName::Folder, "Local checkout".into()),
                };
                let branch =
                    agent_branch(agent).map(|branch| checkout_label(IconName::GitBranch, branch));
                (location, branch, diff_stat)
            }
            None => {
                let composer = self.composer.read(cx);
                let directory = composer.draft_directory.as_deref()?;
                let workspace = store
                    .state
                    .workspaces
                    .values()
                    .find(|workspace| workspace.directory == directory);
                let diff_stat = workspace.and_then(|workspace| workspace.diff_stat);
                let current_branch = workspace
                    .and_then(|workspace| workspace.current_branch.clone())
                    .map(|branch| checkout_label(IconName::GitBranch, branch));
                if !composer.can_create_worktree(cx) {
                    (
                        checkout_label(IconName::Folder, "Local checkout".into()),
                        current_branch,
                        diff_stat,
                    )
                } else if composer.uses_new_worktree(cx) {
                    let base = composer.worktree_base();
                    let label = base
                        .map(|base| base.label.clone())
                        .unwrap_or_else(|| "Default branch".into());
                    let tooltip = base
                        .map(|base| format!("Branch off {}", base.ref_name))
                        .unwrap_or_else(|| "Branch off the repository's default branch".into());
                    let base =
                        checkout_picker("paseo-worktree-base", IconName::GitBranch, label.into())
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.choose_worktree_base(window, cx);
                            }))
                            .into_any_element();
                    // A new worktree starts clean, so the checkout's changes aren't its own.
                    (self.render_isolation_menu(true), Some(base), None)
                } else {
                    (self.render_isolation_menu(false), current_branch, diff_stat)
                }
            }
        };
        let colors = cx.theme().colors();
        Some(
            h_flex()
                .id("paseo-checkout-footer")
                .mx_3()
                .px_2()
                .py_0p5()
                .gap_2()
                .justify_between()
                .rounded_b(rems_from_px(crate::stream::CARD_RADIUS))
                .border_1()
                .border_t_0()
                .border_color(colors.border)
                .bg(colors.elevated_surface_background)
                .child(location)
                .child(
                    h_flex().min_w_0().gap_2().children(branch).children(
                        diff_stat
                            .filter(|stat| stat.additions + stat.deletions > 0)
                            .map(|stat| {
                                h_flex()
                                    .flex_none()
                                    .gap_1()
                                    .child(
                                        Label::new(format!("+{}", stat.additions))
                                            .size(LabelSize::Small)
                                            .color(Color::Created),
                                    )
                                    .child(
                                        Label::new(format!("−{}", stat.deletions))
                                            .size(LabelSize::Small)
                                            .color(Color::Deleted),
                                    )
                            }),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_isolation_menu(&self, new_worktree: bool) -> AnyElement {
        let composer = self.composer.downgrade();
        let (icon, label) = if new_worktree {
            (IconName::GitWorktree, "New worktree")
        } else {
            (IconName::Folder, "Local checkout")
        };
        PopoverMenu::new("paseo-isolation-menu")
            .trigger_with_tooltip(
                checkout_picker("paseo-isolation", icon, label.into()),
                Tooltip::text("Where the new agent works"),
            )
            .anchor(gpui::Anchor::BottomLeft)
            .menu(move |window, cx| {
                let composer = composer.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let choose = |new_worktree: bool| {
                        let composer = composer.clone();
                        move |_: &mut Window, cx: &mut App| {
                            if let Err(error) = composer.update(cx, |composer, cx| {
                                composer.set_new_worktree(new_worktree, cx)
                            }) {
                                log::debug!("Paseo composer released: {error}");
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

    /// Which host the draft's agent will run on, shown only when there is more than one.
    fn render_draft_host_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let view = cx.weak_entity();
        render_host_picker(
            "paseo-draft-host",
            "paseo-draft-host-button",
            "The host the new agent runs on",
            self.store.clone(),
            move |store, window, cx| {
                if let Err(error) =
                    view.update(cx, |view, cx| view.switch_draft_host(store, window, cx))
                {
                    log::debug!("Paseo draft closed: {error}");
                }
            },
            cx,
        )
    }

    /// Replaces this draft with one on `store`'s host. A folder belongs to its host, so the new
    /// draft starts without the old one's.
    fn switch_draft_host(
        &mut self,
        store: Entity<PaseoStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if store == self.store || self.agent_id.is_some() {
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let this = cx.entity();
        crate::defer_workspace_update(workspace, window, cx, move |workspace, window, cx| {
            let old_tab = workspace
                .items_of_type::<AgentTab>(cx)
                .find(|tab| tab.read(cx).view() == &this);
            crate::open_draft_on(workspace, store, None, window, cx);
            if let Some(old_tab) = old_tab {
                crate::workspace_tabs::detach_tab(workspace, &old_tab, window, cx);
            }
        });
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
                questions(request)
                    .into_iter()
                    .find_map(|question| question.dismiss_label)
                    .unwrap_or_else(|| "Dismiss".into())
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
    /// Offers a free-text answer. The daemon sets `allowOther` on Claude's questions, since
    /// Claude expects the host to provide "Other"; Codex questions use `isOther`.
    pub allow_other: bool,
    pub allow_empty: bool,
    pub placeholder: Option<String>,
    pub dismiss_label: Option<String>,
}

/// Paseo's rules for the question form, so answers reach the agent in the same shape as from
/// the Paseo app.
fn question_placeholder(question: &Question) -> String {
    question.placeholder.clone().unwrap_or_else(|| {
        if question.options.is_empty() {
            "Type your answer…".to_owned()
        } else {
            "Other…".to_owned()
        }
    })
}

pub(crate) fn shows_text_input(question: &Question) -> bool {
    question.options.is_empty() || question.allow_other
}

pub(crate) fn is_answered(question: &Question, selected: &[String], text: &str) -> bool {
    if !selected.is_empty() {
        return true;
    }
    shows_text_input(question) && (!text.trim().is_empty() || question.allow_empty)
}

/// Answers keyed by header. Typed text replaces a single choice and joins multiple choices.
pub(crate) fn build_answers(
    questions: &[Question],
    selected: impl Fn(usize) -> Vec<String>,
    text: impl Fn(usize) -> String,
) -> serde_json::Map<String, Value> {
    let mut answers = serde_json::Map::new();
    for (index, question) in questions.iter().enumerate() {
        let mut chosen = selected(index);
        if shows_text_input(question) {
            let typed = text(index).trim().to_owned();
            if !typed.is_empty() {
                let answer = if question.multi_select {
                    chosen.push(typed);
                    chosen.join(", ")
                } else {
                    typed
                };
                answers.insert(question.header.clone(), Value::String(answer));
                continue;
            }
            if question.allow_empty && question.options.is_empty() {
                answers.insert(question.header.clone(), Value::String(String::new()));
                continue;
            }
        }
        if !chosen.is_empty() {
            answers.insert(question.header.clone(), Value::String(chosen.join(", ")));
        }
    }
    answers
}

/// Dismissing a form of only optional free-text questions submits it empty instead of denying.
pub(crate) fn submit_empty_on_dismiss(questions: &[Question]) -> bool {
    !questions.is_empty()
        && questions
            .iter()
            .all(|question| question.allow_empty && question.options.is_empty())
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
            allow_other: ["allowOther", "isOther"]
                .iter()
                .any(|key| question.get(*key).and_then(Value::as_bool) == Some(true)),
            allow_empty: question.get("allowEmpty").and_then(Value::as_bool) == Some(true),
            placeholder: question
                .get("placeholder")
                .and_then(Value::as_str)
                .map(str::to_owned),
            dismiss_label: question
                .get("dismissLabel")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

pub(crate) fn bucket_indicator(bucket: AgentBucket) -> AnyElement {
    match bucket {
        AgentBucket::Running => Icon::new(IconName::LoadCircle)
            .size(IconSize::XSmall)
            .color(Color::Accent)
            .with_rotate_animation(2)
            .into_any_element(),
        AgentBucket::NeedsInput => Indicator::dot().color(Color::Warning).into_any_element(),
        AgentBucket::Failed => Indicator::dot().color(Color::Error).into_any_element(),
        AgentBucket::Attention => Indicator::dot().color(Color::Accent).into_any_element(),
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
        let empty = self.projection.items().is_empty();
        self.markdown_style = Some(Self::build_markdown_style(window, cx));
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
                .key_context("PaseoAgentView PaseoView")
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
                            .max_w(content_max_width(cx))
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
                                match self.render_archived_notice(cx) {
                                    // Paseo swaps the composer for this notice until the agent is
                                    // unarchived.
                                    Some(notice) => this.child(notice),
                                    None => {
                                        this.children(self.render_permissions(window, cx)).child(
                                            v_flex()
                                                .w_full()
                                                .children(self.render_subagent_track(window, cx))
                                                .child(self.composer.clone())
                                                .children(self.render_checkout_footer(cx)),
                                        )
                                    }
                                }
                            }),
                    ),
                ),
        )
    }
}

/// Drives a question card the way clicks and typing do, for tests.
#[cfg(any(test, feature = "test-support"))]
impl AgentTab {
    pub fn test_select_answer(
        tab: &Entity<Self>,
        request_id: &str,
        question: usize,
        label: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = tab.read(cx).view.clone();
        view.update(cx, |view, cx| {
            view.toggle_answer(request_id, question, label.to_owned(), window, cx)
        });
    }

    /// The picked options and the typed text of one question.
    pub fn test_answer(
        tab: &Entity<Self>,
        request_id: &str,
        question: usize,
        cx: &App,
    ) -> (Vec<String>, String) {
        let view = tab.read(cx).view.read(cx);
        (
            view.question_selected(request_id, question),
            view.question_text(request_id, question, cx),
        )
    }

    pub fn test_current_question(tab: &Entity<Self>, cx: &App) -> Option<usize> {
        let view = tab.read(cx).view.read(cx);
        let request = view.primary_permission(cx)?;
        Some(view.current_question(&request.request_id, questions(&request).len()))
    }

    pub fn test_type_answer(
        tab: &Entity<Self>,
        request_id: &str,
        question: usize,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = tab.read(cx).view.clone();
        view.update(cx, |view, cx| {
            let editor = view.question_input(request_id, question, "", window, cx);
            editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
        });
    }

    /// Presses Next or Submit; returns the question shown afterwards.
    pub fn test_advance_question(
        tab: &Entity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        let view = tab.read(cx).view.clone();
        view.update(cx, |view, cx| {
            let request = view.primary_permission(cx)?;
            view.advance_question(&request, window, cx);
            view.question_index.get(&request.request_id).copied()
        })
    }

    pub fn test_view_id(tab: &Entity<Self>, cx: &App) -> gpui::EntityId {
        tab.read(cx).view.entity_id()
    }

    /// The profile name of the host the tab's chat talks to.
    pub fn test_host_name(tab: &Entity<Self>, cx: &App) -> Option<String> {
        crate::hosts::host_name(&tab.read(cx).view.read(cx).store, cx)
    }

    /// What Submit would send for the agent's first pending request.
    pub fn test_question_response(tab: &Entity<Self>, cx: &App) -> Option<PermissionResponse> {
        let view = tab.read(cx).view.read(cx);
        let request = view.primary_permission(cx)?;
        Some(view.permission_response(&request, true, Some("accept".into()), cx))
    }
}

pub struct AgentTab {
    view: Entity<AgentView>,
    /// Set on a restored tab until its workspace has checked that the agent belongs there.
    pub(crate) owner_check_pending: bool,
    /// Set when the tab is removed to move or hide its chat, which isn't the user closing it.
    pub(crate) leaving: bool,
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
        Self::wrapping(view, cx)
    }

    pub(crate) fn on_host(
        store: Entity<PaseoStore>,
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view =
            cx.new(|cx| AgentView::on_host(store, agent_id, directory, workspace, window, cx));
        Self::wrapping(view, cx)
    }

    /// A tab rebuilt from a saved workspace, on the host it was saved with (tabs saved before
    /// hosts were recorded use the default host). Its workspace checks the agent once it is
    /// known, because a saved tab may belong to another workspace's agent.
    pub(crate) fn restored(
        agent_id: String,
        host: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = host.map_or_else(
            || hosts::default_store(cx),
            |host| hosts::store_named(&host, cx),
        );
        let view = cx
            .new(|cx| AgentView::on_host(store, Some(agent_id), None, Some(workspace), window, cx));
        Self {
            owner_check_pending: true,
            ..Self::wrapping(view, cx)
        }
    }

    fn wrapping(view: Entity<AgentView>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&view, |_, _, event: &AgentViewEvent, cx| match event {
            AgentViewEvent::TabChanged => cx.emit(ItemEvent::UpdateTab),
        });
        Self {
            view,
            owner_check_pending: false,
            leaving: false,
            _subscription: subscription,
        }
    }

    /// A new tab showing `view`, for moving a chat into `workspace`. A moved tab keeps a pending
    /// ownership check, so its new workspace checks it once the agent is known.
    pub(crate) fn for_view(
        view: Entity<AgentView>,
        workspace: WeakEntity<Workspace>,
        owner_check_pending: bool,
        cx: &mut App,
    ) -> Entity<Self> {
        view.update(cx, |view, cx| {
            view.workspace = Some(workspace);
            cx.notify();
        });
        cx.new(|cx| Self {
            owner_check_pending,
            ..Self::wrapping(view, cx)
        })
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

    /// Where the agent works, when its tab is in an editor window for other folders.
    fn away_suffix(&self, cx: &App) -> Option<String> {
        let view = self.view.read(cx);
        let agent = view.agent(cx)?;
        let directory = agent.directory.as_ref()?;
        let workspace = view.workspace.as_ref()?.upgrade()?;
        let at_home = workspace
            .read(cx)
            .root_paths(cx)
            .iter()
            .any(|root| directory.starts_with(root));
        (!at_home).then(|| {
            crate::store::agent_worktree_name(agent).unwrap_or_else(|| agent_project_name(agent))
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn composer_text(&self, cx: &App) -> String {
        self.view.read(cx).composer.read(cx).text(cx)
    }

    /// Marks a draft as sent, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_begin_creating(tab: &Entity<Self>, cx: &mut App) {
        let composer = tab.read(cx).view.read(cx).composer.clone();
        composer.update(cx, |composer, _| composer.begin_creating_for_test());
    }

    /// Finishes a sent draft as if the daemon created `agent_id`, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_finish_creating(tab: &Entity<Self>, agent_id: &str, cx: &mut App) {
        let composer = tab.read(cx).view.read(cx).composer.clone();
        composer.update(cx, |composer, cx| {
            composer.finish_creating_for_test(agent_id.to_owned(), cx)
        });
    }

    /// Sends the composer's text, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_submit(tab: &Entity<Self>, cx: &mut App) {
        let composer = tab.read(cx).view.read(cx).composer.clone();
        composer.update(cx, |composer, cx| composer.submit(false, cx));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_type_in_composer(
        tab: &Entity<Self>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let composer = tab.read(cx).view.read(cx).composer.clone();
        composer.update(cx, |composer, cx| composer.set_text(text, window, cx));
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

    fn on_removed(&self, cx: &mut Context<Self>) {
        if self.leaving {
            return;
        }
        let view = self.view.read(cx);
        let workspace_id = view
            .workspace
            .as_ref()
            .map(|workspace| workspace.entity_id());
        if let (Some(workspace_id), Some(agent_id)) = (workspace_id, view.agent_id.clone()) {
            crate::workspace_tabs::tab_closed(workspace_id, agent_id, cx);
        }
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let view = self.view.read(cx);
        let alert = view.bucket.and_then(AgentAlert::for_bucket);
        let underline = div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(-3.))
            .h(px(2.))
            .rounded_sm();
        let content = h_flex()
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
            .when_some(self.away_suffix(cx), |this, suffix| {
                this.child(Label::new(suffix).single_line().color(Color::Muted))
            });
        div()
            .relative()
            .child(content)
            .child(pulse_in_alert_color(
                underline,
                alert,
                ("paseo-tab-alert", self.view.entity_id()),
                |underline, color| underline.bg(color),
                cx,
            ))
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
                view.store.read(cx).display_title(agent),
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
            let (agent_id, host) = db
                .get_agent_tab(item_id, workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("No Paseo agent tab to restore"))?;
            cx.update(|window, cx| {
                cx.new(|cx| AgentTab::restored(agent_id, host, workspace, window, cx))
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
        let host = hosts::host_name(&self.view.read(cx).store, cx);
        let db = persistence::AgentTabDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_agent_tab(item_id, workspace_id, agent_id, host)
                .await
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

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE paseo_agent_tabs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    agent_id TEXT NOT NULL,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            // The Paseo profile name of the tab's host.
            sql!(ALTER TABLE paseo_agent_tabs ADD COLUMN host TEXT),
        ];
    }

    db::static_connection!(AgentTabDb, [WorkspaceDb]);

    impl AgentTabDb {
        query! {
            pub async fn save_agent_tab(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                agent_id: String,
                host: Option<String>
            ) -> Result<()> {
                INSERT OR REPLACE INTO paseo_agent_tabs(item_id, workspace_id, agent_id, host)
                VALUES (?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_agent_tab(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(String, Option<String>)>> {
                SELECT agent_id, host
                FROM paseo_agent_tabs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

/// Ends the paragraph after each markdown image that text follows. The markdown renderer lays out
/// a paragraph's first line after an image against the wrong width, so it runs past the column.
/// Growing text keeps its earlier output as a prefix, so streamed chunks still append.
fn separate_images(text: &str) -> std::borrow::Cow<'_, str> {
    let images = markdown_images(text);
    if images.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut separated = String::with_capacity(text.len() + 8);
    let mut copied = 0;
    for image in images {
        separated.push_str(&text[copied..image.end]);
        copied = image.end;
        let rest = &text[image.end..];
        let newlines = rest.bytes().take_while(|byte| *byte == b'\n').count();
        if newlines < 2 && rest.len() > newlines {
            separated.push_str(&"\n".repeat(2 - newlines));
        }
    }
    separated.push_str(&text[copied..]);
    std::borrow::Cow::Owned(separated)
}

/// The link targets of markdown images (`![alt](target)`) in `text`.
fn image_destinations(text: &str) -> Vec<&str> {
    markdown_images(text)
        .into_iter()
        .map(|image| &text[image.destination])
        .filter(|destination| !destination.is_empty())
        .collect()
}

/// A markdown image outside code: where its destination is and where the image ends.
struct MarkdownImage {
    destination: std::ops::Range<usize>,
    end: usize,
}

/// The images in `text`, skipping code blocks and code spans. A destination may be wrapped in
/// `<…>` or hold balanced parentheses, as in `/tmp/Screenshot (1).png`.
fn markdown_images(text: &str) -> Vec<MarkdownImage> {
    let code = code_ranges(text);
    let mut images = Vec::new();
    let mut search_from = 0;
    while let Some(found) = text.get(search_from..).and_then(|rest| rest.find("![")) {
        let start = search_from + found;
        if let Some(range) = code.iter().find(|range| range.contains(&start)) {
            search_from = range.end;
            continue;
        }
        let Some(image) = image_at(text, start) else {
            search_from = start + 2;
            continue;
        };
        search_from = image.end;
        images.push(image);
    }
    images
}

fn image_at(text: &str, start: usize) -> Option<MarkdownImage> {
    let open = start + text.get(start..)?.find("](")? + 2;
    let after = text.get(open..)?;
    let leading = after.len() - after.trim_start().len();
    let (destination, close) = if let Some(bracketed) = after[leading..].strip_prefix('<') {
        let end = open + leading + 1 + bracketed.find('>')?;
        let close = end + text.get(end..)?.find(')')?;
        (open + leading + 1..end, close)
    } else {
        let mut depth = 0usize;
        let close = open
            + after
                .char_indices()
                .find_map(|(offset, character)| match character {
                    '(' => {
                        depth += 1;
                        None
                    }
                    ')' if depth == 0 => Some(offset),
                    ')' => {
                        depth -= 1;
                        None
                    }
                    _ => None,
                })?;
        let inside = &text[open..close];
        let target_start = open + leading;
        let target_length = inside
            .trim_start()
            .split_whitespace()
            .next()
            .map_or(0, str::len);
        (target_start..target_start + target_length, close)
    };
    Some(MarkdownImage {
        destination,
        end: close + 1,
    })
}

/// Byte ranges of fenced code blocks and code spans. An unclosed fence runs to the end, so text
/// streaming into it stays code.
fn code_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut fence_start = None;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        match (is_fence, fence_start) {
            (true, None) => fence_start = Some(line_start),
            (true, Some(start)) => {
                ranges.push(start..line_start + line.len());
                fence_start = None;
            }
            (false, None) => {
                let mut offset = 0;
                while let Some(open) = line[offset..].find('`') {
                    let open = offset + open;
                    let ticks = line[open..]
                        .bytes()
                        .take_while(|byte| *byte == b'`')
                        .count();
                    let marker = &line[open..open + ticks];
                    match line[open + ticks..].find(marker) {
                        Some(close) => {
                            let close = open + ticks + close + ticks;
                            ranges.push(line_start + open..line_start + close);
                            offset = close;
                        }
                        None => break,
                    }
                }
            }
            (false, Some(_)) => {}
        }
        line_start += line.len();
    }
    if let Some(start) = fence_start {
        ranges.push(start..text.len());
    }
    ranges
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

pub(crate) fn checkout_label(icon: IconName, text: String) -> AnyElement {
    h_flex()
        .min_w_0()
        .gap_1()
        .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .child(
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .into_any_element()
}

/// A menu button choosing among the configured hosts, or `None` with fewer than two.
pub(crate) fn render_host_picker(
    menu_id: &'static str,
    button_id: &'static str,
    tooltip: &'static str,
    own_store: Entity<PaseoStore>,
    on_pick: impl Fn(Entity<PaseoStore>, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    let hosts = hosts::configured_hosts(cx);
    if hosts.len() < 2 {
        return None;
    }
    let current = hosts
        .iter()
        .find(|(_, store)| *store == own_store)
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    let on_pick = Rc::new(on_pick);
    Some(
        PopoverMenu::new(menu_id)
            .trigger_with_tooltip(
                checkout_picker(button_id, IconName::Server, current.into()),
                Tooltip::text(tooltip),
            )
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let (hosts, own_store, on_pick) =
                    (hosts.clone(), own_store.clone(), on_pick.clone());
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (name, store) in hosts.iter().cloned() {
                        let on_pick = on_pick.clone();
                        menu = menu.toggleable_entry(
                            name,
                            store == own_store,
                            ui::IconPosition::End,
                            None,
                            move |window, cx| on_pick(store.clone(), window, cx),
                        );
                    }
                    menu
                }))
            })
            .into_any_element(),
    )
}

pub(crate) fn checkout_picker(id: &'static str, icon: IconName, label: SharedString) -> ui::Button {
    ui::Button::new(id, label)
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .start_icon(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .end_icon(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
}

/// The agent's ⋯ menu for the right of the tab bar, when `item` is an agent's tab.
pub fn agent_tab_menu(item: &dyn workspace::ItemHandle, cx: &App) -> Option<AnyElement> {
    let tab = item.to_any_view().downcast::<AgentTab>().ok()?;
    let view = tab.read(cx).view().clone();
    let agent_view = view.read(cx);
    if agent_view.is_subagent() || agent_view.agent_id.is_none() {
        return None;
    }
    let focus = agent_view.focus_handle.clone();
    let this = view.downgrade();
    Some(
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
                                    view.store
                                        .update(cx, |store, cx| store.load_tail(agent_id, cx));
                                }
                            }) {
                                log::debug!("Paseo agent view released: {error}");
                            }
                        })
                        .separator()
                        .action("Archive Agent", ArchiveAgent.boxed_clone())
                }))
            })
            .into_any_element(),
    )
}

/// How long a subagent has run: until now while it runs, until its last update once it stopped.
fn subagent_run_time(
    subagent: &paseo_client::ProviderSubagent,
    now: chrono::DateTime<Utc>,
) -> Option<String> {
    let started = parse_timestamp(&subagent.created_at)?;
    let ended = if subagent.status == "running" {
        now
    } else {
        parse_timestamp(&subagent.updated_at)?
    };
    Some(crate::timeline::format_duration(
        (ended - started).num_seconds(),
    ))
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

/// Opens the files that thread links and `path:line` code spans point at.
#[derive(Clone)]
struct ThreadLinks {
    view: WeakEntity<AgentView>,
    store: Entity<PaseoStore>,
    agent_id: Option<String>,
    workspace: Option<WeakEntity<Workspace>>,
    code_spans: Rc<RefCell<CodeSpanCache>>,
}

/// Enough resolved code spans for a long thread; past this the oldest are forgotten.
const CODE_SPAN_CACHE_LIMIT: usize = 4096;

/// Resolved code spans, because resolving runs on every render of every span. A span that needs
/// the disk or a project scan reads as no link until its lookup finishes.
#[derive(Default)]
struct CodeSpanCache {
    links: HashMap<String, Option<SharedString>>,
    /// Span texts oldest first, to forget the oldest past the limit.
    order: std::collections::VecDeque<String>,
}

impl CodeSpanCache {
    fn get(&self, text: &str) -> Option<Option<SharedString>> {
        self.links.get(text).cloned()
    }

    fn insert(&mut self, text: &str, link: Option<SharedString>) {
        if let Some(existing) = self.links.get_mut(text) {
            *existing = link;
            return;
        }
        while self.links.len() >= CODE_SPAN_CACHE_LIMIT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.links.remove(&oldest);
        }
        self.links.insert(text.to_owned(), link);
        self.order.push_back(text.to_owned());
    }
}

/// How far a code span resolved without waiting.
enum CodeSpanResolution {
    Link(SharedString),
    NotALink,
    /// The span names no project entry, so it is looked up on disk and by file name off the
    /// render path.
    Lookup {
        target: LinkTarget,
        check_disk: bool,
        relative: Option<String>,
    },
}

impl ThreadLinks {
    fn directory(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .timeline_directory(self.agent_id.as_deref()?)
    }

    fn workspace(&self) -> Option<Entity<Workspace>> {
        self.workspace.as_ref().and_then(WeakEntity::upgrade)
    }

    /// Code spans become links when they name an existing file or project folder, as a `file://`
    /// URL with the line, so a click opens the file the span resolved to, or when they name a
    /// code symbol, resolved only on click. This runs on every render of every span.
    fn code_span_link(&self, text: &str, cx: &App) -> Option<SharedString> {
        let text = text.trim();
        if !is_path_like(text) {
            // Looked up only when clicked, since resolving asks the language servers.
            return is_symbol_like(text).then(|| format!("{SYMBOL_LINK_SCHEME}{text}").into());
        }
        if let Some(cached) = self.code_spans.borrow().get(text) {
            return cached;
        }
        let link = match self.resolve_code_span(text, cx) {
            CodeSpanResolution::Link(link) => Some(link),
            CodeSpanResolution::NotALink => None,
            CodeSpanResolution::Lookup {
                target,
                check_disk,
                relative,
            } => {
                self.look_up_code_span(text, target, check_disk, relative, cx);
                None
            }
        };
        self.code_spans.borrow_mut().insert(text, link.clone());
        link
    }

    fn resolve_code_span(&self, text: &str, cx: &App) -> CodeSpanResolution {
        let Some(target) = link_target(text, self.directory(cx).as_deref()) else {
            return CodeSpanResolution::NotALink;
        };
        let Some(workspace) = self.workspace() else {
            return CodeSpanResolution::NotALink;
        };
        let project = workspace.read(cx).project().read(cx);
        let entry = project
            .find_project_path(&target.path, cx)
            .and_then(|project_path| project.entry_for_path(&project_path, cx));
        if entry.is_some() {
            return file_link(&target.path, target.row)
                .map_or(CodeSpanResolution::NotALink, CodeSpanResolution::Link);
        }
        // Agents often name a file without its folders, like `producer.rs:83`, or relative to a
        // folder inside the project, like `phantom-bin/src/engine/book.rs`.
        let relative = util::paths::PathWithPosition::parse_str(text).path;
        let relative = (!relative.is_absolute())
            .then(|| relative.to_str().map(str::to_owned))
            .flatten();
        CodeSpanResolution::Lookup {
            target,
            // Ignored folders such as `.notes` have no project entries until expanded.
            check_disk: self.store.read(cx).is_local_host(),
            relative,
        }
    }

    /// Resolves a span the project has no entry for, then redraws the chat if it became a link.
    fn look_up_code_span(
        &self,
        text: &str,
        target: LinkTarget,
        check_disk: bool,
        relative: Option<String>,
        cx: &App,
    ) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let text = text.to_owned();
        let code_spans = self.code_spans.clone();
        let view = self.view.clone();
        cx.spawn(async move |cx| {
            let on_disk = check_disk && {
                let path = target.path.clone();
                cx.background_spawn(async move {
                    std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file())
                })
                .await
            };
            let path = if on_disk {
                Some(target.path.clone())
            } else if let Some(relative) = relative {
                cx.update(|cx| unique_project_file(project.read(cx), &relative, cx))
                    .await
            } else {
                None
            };
            let Some(link) = path.and_then(|path| file_link(&path, target.row)) else {
                return;
            };
            code_spans.borrow_mut().insert(&text, Some(link));
            if let Err(error) = view.update(cx, |_, cx| cx.notify()) {
                log::debug!("Paseo agent view released before a code span resolved: {error}");
            }
        })
        .detach();
    }

    fn find_text(&self, name: &str, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let query = symbol_segments(name)
            .last()
            .copied()
            .unwrap_or(name)
            .to_owned();
        workspace.update(cx, |workspace, cx| {
            search::text_finder::TextFinder::open_with_query(workspace, query, window, cx).detach();
        });
    }

    /// Opens the definition of the symbol a code span names, asking the project's language
    /// servers the way Zed's Go to Symbol in Project does: one match opens, several open that
    /// picker with the name typed, none searches the project for the name.
    fn open_symbol(&self, name: &str, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let query = symbol_segments(name)
            .last()
            .copied()
            .unwrap_or(name)
            .to_owned();
        let task = workspace.update(cx, |workspace, cx| {
            workspace
                .project()
                .update(cx, |project, cx| project.symbols(&query, cx))
        });
        let name = name.to_owned();
        let notify = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let symbols = task.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let keys = symbols
                        .iter()
                        .map(|symbol| (symbol.name.as_str(), symbol.container_name.as_deref()))
                        .collect::<Vec<_>>();
                    match pick_symbol(&name, &keys) {
                        SymbolPick::One(index) => {
                            if let Some(symbol) = symbols.get(index) {
                                open_symbol_definition(workspace, symbol.clone(), window, cx)
                            }
                        }
                        SymbolPick::Several => {
                            window
                                .dispatch_action(workspace::ToggleProjectSymbols.boxed_clone(), cx);
                            let workspace = cx.weak_entity();
                            window.defer(cx, move |window, cx| {
                                let picker = workspace.upgrade().and_then(|workspace| {
                                    workspace.read(cx).active_modal::<picker::Picker<
                                        project_symbols::ProjectSymbolsDelegate,
                                    >>(cx)
                                });
                                if let Some(picker) = picker {
                                    picker.update(cx, |picker, cx| {
                                        picker.set_query(&query, window, cx)
                                    });
                                }
                            });
                        }
                        SymbolPick::None => window.dispatch_action(
                            workspace::pane::DeploySearch {
                                query: Some(query.clone()),
                                case_sensitive: Some(true),
                                whole_word: Some(true),
                                ..Default::default()
                            }
                            .boxed_clone(),
                            cx,
                        ),
                    }
                })
            })
            .detach_and_notify_err(notify, window, cx);
    }

    fn open(&self, url: &str, window: &mut Window, cx: &mut App) {
        if let Some(name) = url.strip_prefix(SYMBOL_LINK_SCHEME) {
            // Ctrl-click (Cmd on macOS) finds the name's uses in Zed's Text Finder.
            if window.modifiers().secondary() {
                self.find_text(name, window, cx);
            } else {
                self.open_symbol(name, window, cx);
            }
            return;
        }
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

/// A `file://` URL for `path`, at `row` when known.
fn file_link(path: &std::path::Path, row: Option<u32>) -> Option<SharedString> {
    let mut url = url::Url::from_file_path(path).ok()?;
    if let Some(row) = row {
        url.set_fragment(Some(&format!("L{row}")));
    }
    Some(url.to_string().into())
}

/// The project's only file with this name, or `None` when there is none or more than one. The
/// worktrees are walked in the background, since a project can hold many files.
fn unique_project_file(
    project: &project::Project,
    relative: &str,
    cx: &App,
) -> Task<Option<PathBuf>> {
    let relative = relative.trim_start_matches("./").replace('\\', "/");
    let Some(file_name) = relative.rsplit('/').next().map(str::to_owned) else {
        return Task::ready(None);
    };
    let snapshots = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).snapshot())
        .collect::<Vec<_>>();
    cx.background_spawn(async move {
        // Whole folder names only: `c/mod_a/lib.rs` must not match `src/mod_a/lib.rs`.
        let ends_with_relative = |path: &str| {
            path == relative
                || path
                    .strip_suffix(relative.as_str())
                    .is_some_and(|parent| parent.ends_with('/'))
        };
        let mut matches = snapshots.iter().flat_map(|snapshot| {
            snapshot
                .files(false, 0)
                .filter(|entry| entry.path.file_name() == Some(file_name.as_str()))
                .filter(|entry| ends_with_relative(entry.path.as_unix_str()))
                .take(2)
                .map(|entry| snapshot.absolutize(&entry.path))
                .collect::<Vec<_>>()
        });
        let only = matches.next()?;
        matches.next().is_none().then_some(only)
    })
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

/// The link a symbol code span carries until it is clicked.
pub(crate) const SYMBOL_LINK_SCHEME: &str = "zaseo-symbol:";

/// A name's path segments, `store::bucket()` as `["store", "bucket"]`.
fn symbol_segments(name: &str) -> Vec<&str> {
    name.strip_suffix("()")
        .unwrap_or(name)
        .split("::")
        .collect()
}

/// Whether a code span reads as a code name: a path like `store::bucket`, a `snake_case` or
/// `SCREAMING_CASE` name, or a `CamelCase` one. Single words stay plain, since `main` or `true`
/// are more often prose than a symbol to jump to.
fn is_symbol_like(text: &str) -> bool {
    let segments = symbol_segments(text);
    let valid_segment = |segment: &&str| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            && !segment.starts_with(|character: char| character.is_ascii_digit())
    };
    if text.len() < 3 || !segments.iter().all(valid_segment) {
        return false;
    }
    let Some(last) = segments.last() else {
        return false;
    };
    let inner_underscore = last.trim_matches('_').contains('_');
    let camel = last
        .chars()
        .zip(last.chars().skip(1))
        .any(|(first, second)| first.is_ascii_lowercase() && second.is_ascii_uppercase());
    segments.len() > 1 || inner_underscore || camel
}

#[derive(Debug, PartialEq)]
enum SymbolPick {
    One(usize),
    Several,
    None,
}

/// Which of the symbols (name, container) a clicked name means: those with its last segment as
/// their name, narrowed by the segment before it when that matches a container.
fn pick_symbol(name: &str, symbols: &[(&str, Option<&str>)]) -> SymbolPick {
    let segments = symbol_segments(name);
    let Some(last) = segments.last() else {
        return SymbolPick::None;
    };
    let named = (0..symbols.len())
        .filter(|index| symbols[*index].0 == *last)
        .collect::<Vec<_>>();
    let qualified = segments
        .len()
        .checked_sub(2)
        .and_then(|index| segments.get(index))
        .map(|qualifier| {
            named
                .iter()
                .copied()
                .filter(|index| {
                    symbols[*index]
                        .1
                        .is_some_and(|container| container.contains(qualifier))
                })
                .collect::<Vec<_>>()
        })
        .filter(|qualified| !qualified.is_empty());
    match qualified.as_deref().unwrap_or(&named) {
        [] => SymbolPick::None,
        [only] => SymbolPick::One(*only),
        _ => SymbolPick::Several,
    }
}

/// Opens a symbol's file at its start, as Zed's Go to Symbol in Project does.
fn open_symbol_definition(
    workspace: &mut Workspace,
    symbol: project::Symbol,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let buffer = workspace.project().update(cx, |project, cx| {
        project.open_buffer_for_symbol(&symbol, cx)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let buffer = buffer.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let position = buffer
                .read(cx)
                .clip_point_utf16(symbol.range.start, text::Bias::Left);
            let editor = workspace
                .open_project_item::<Editor>(None, buffer, true, true, true, true, window, cx);
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let Some(buffer_snapshot) = snapshot.as_singleton() else {
                    return;
                };
                let Some(anchor) =
                    snapshot.anchor_in_buffer(buffer_snapshot.anchor_before(position))
                else {
                    return;
                };
                editor.change_selections(
                    editor::SelectionEffects::scroll(editor::scroll::Autoscroll::center()),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
            });
        })
    })
    .detach_and_notify_err(cx.weak_entity(), window, cx);
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

    #[test]
    fn only_rows_arriving_during_the_conversation_ease_in() {
        let group = |key| Row::ToolGroup {
            key,
            label: String::new(),
            running: false,
            failed: false,
            expanded: false,
        };
        let footer = |turn: u64| Row::TurnFooter {
            first_item_key: Some(1000 + turn),
            has_text: false,
            duration_seconds: None,
            finished_at: None,
        };
        let working = Row::Working {
            since: None,
            spinner: true,
        };

        let opened = vec![group(1), footer(0)];
        assert!(arriving_rows(&[], &opened).is_empty());

        let running = vec![group(1), footer(0), group(2), working];
        assert_eq!(
            arriving_rows(&opened, &running),
            vec![RowIdentity::ToolGroup(2), RowIdentity::Working]
        );

        let ticking = vec![
            group(1),
            footer(0),
            group(2),
            Row::Working {
                since: Some(Utc::now()),
                spinner: false,
            },
        ];
        assert!(arriving_rows(&running, &ticking).is_empty());

        let older_loaded = vec![group(7), group(8), group(1), footer(0)];
        assert!(arriving_rows(&opened, &older_loaded).is_empty());

        // An older turn loading above shifts every turn's index, not its first item.
        let older_turn = Row::TurnFooter {
            first_item_key: Some(1),
            has_text: false,
            duration_seconds: None,
            finished_at: None,
        };
        let shifted = Row::TurnFooter {
            first_item_key: Some(1000),
            has_text: false,
            duration_seconds: None,
            finished_at: None,
        };
        assert!(arriving_rows(&opened, &[group(6), older_turn, group(1), shifted]).is_empty());

        let unfolded = vec![group(1), group(3), group(4), footer(0)];
        assert_eq!(
            arriving_rows(&opened, &unfolded),
            vec![RowIdentity::ToolGroup(3), RowIdentity::ToolGroup(4)]
        );

        let bulk = std::iter::once(group(1))
            .chain((10..10 + MAX_ANIMATED_ARRIVALS as u64 + 1).map(group))
            .collect::<Vec<_>>();
        assert!(arriving_rows(&opened, &bulk).is_empty());
    }

    fn item_row(key: u64, content: StreamContent) -> Row {
        Row::Item {
            item: Rc::new(StreamItem {
                key,
                timestamp: None,
                last_timestamp: None,
                content,
            }),
            tool: None,
            expanded: false,
            streaming: false,
        }
    }

    fn text(key: u64) -> Row {
        item_row(
            key,
            StreamContent::Assistant {
                text: format!("text {key}"),
            },
        )
    }

    #[test]
    fn a_change_splices_only_the_rows_between_the_unchanged_ends() {
        let rows = [text(1), text(2), text(3), Row::Spacer];
        assert_eq!(row_splice(&rows, &rows), None);
        assert_eq!(
            row_splice(&rows, &[text(1), text(5), text(6), text(3), Row::Spacer]),
            Some((1..2, 2)),
            "a change in the middle keeps the rows after it"
        );
        assert_eq!(
            row_splice(&rows, &[text(1), text(2), text(3), text(4), Row::Spacer]),
            Some((3..3, 1)),
            "a row added before the spacer"
        );
        assert_eq!(row_splice(&rows, &[text(1), Row::Spacer]), Some((1..3, 0)));
        assert_eq!(
            row_splice(&[Row::Spacer, Row::Spacer], &[Row::Spacer]),
            Some((1..2, 0)),
            "the ends never overlap"
        );
    }

    #[test]
    fn images_end_their_paragraph() {
        assert_eq!(separate_images("no images"), "no images");
        assert_eq!(
            separate_images("![shot](/tmp/a.png)That grab"),
            "![shot](/tmp/a.png)\n\nThat grab"
        );
        assert_eq!(
            separate_images("![shot](/tmp/a.png)\nThat grab"),
            "![shot](/tmp/a.png)\n\nThat grab"
        );
        assert_eq!(
            separate_images("![shot](/tmp/a.png)\n\nThat"),
            "![shot](/tmp/a.png)\n\nThat"
        );
        assert_eq!(
            separate_images("![shot](/tmp/a.png)"),
            "![shot](/tmp/a.png)"
        );
        let partial = separate_images("![shot](/tmp/a.png)\n").into_owned();
        assert!(separate_images("![shot](/tmp/a.png)\nmore").starts_with(&partial));
    }

    #[test]
    fn image_links_keep_parentheses_and_code_intact() {
        assert_eq!(
            separate_images("![a](/tmp/a(1).png)Then"),
            "![a](/tmp/a(1).png)\n\nThen"
        );
        assert_eq!(
            separate_images("![s](</tmp/Screenshot (1).png>)Then"),
            "![s](</tmp/Screenshot (1).png>)\n\nThen"
        );
        let fenced = "```md\n![x](y)text\n```\n";
        assert_eq!(separate_images(fenced), fenced);
        let inline = "use `![x](y)z` here";
        assert_eq!(separate_images(inline), inline);
        assert_eq!(
            image_destinations("![a](/tmp/a(1).png) ![s](</tmp/S (1).png>) `![c](code.png)`"),
            vec!["/tmp/a(1).png", "/tmp/S (1).png"]
        );
    }

    #[test]
    fn a_finished_turn_folds_its_steps_before_the_answer() {
        let user = |key| {
            item_row(
                key,
                StreamContent::User {
                    text: "go".into(),
                    message_id: None,
                },
            )
        };
        let reasoning = |key| item_row(key, StreamContent::Reasoning { text: "hmm".into() });
        let items = [
            user(0),
            text(1),
            reasoning(2),
            text(3),
            user(4),
            reasoning(5),
        ]
        .into_iter()
        .filter_map(|row| match row {
            Row::Item { item, .. } => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
        assert_eq!(
            folded_work(&items, 0..4),
            Some(1..3),
            "narration and thinking fold"
        );
        assert_eq!(
            folded_work(&items, 4..6),
            Some(5..6),
            "no answer: everything after the message"
        );
        assert_eq!(
            folded_work(&items, 0..2),
            None,
            "only an answer: nothing to fold"
        );
    }

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
    fn subagent_run_time_counts_to_now_while_running_and_to_the_end_after() {
        let now = "2026-09-28T10:05:30Z"
            .parse::<chrono::DateTime<Utc>>()
            .expect("time");
        let mut running = subagent("a", "running");
        running.updated_at = "2026-09-28T10:01:00Z".into();
        assert_eq!(subagent_run_time(&running, now).as_deref(), Some("5m 30s"));

        let mut finished = subagent("b", "completed");
        finished.updated_at = "2026-09-28T10:02:00Z".into();
        assert_eq!(subagent_run_time(&finished, now).as_deref(), Some("2m"));

        let mut unparsed = subagent("c", "running");
        unparsed.created_at = "soon".into();
        assert_eq!(subagent_run_time(&unparsed, now), None);
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
        let find = |relative: &str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| unique_project_file(project.read(cx), relative, cx))
        };
        assert_eq!(
            find("producer.rs", cx).await,
            Some(PathBuf::from("/repo/src/producer.rs"))
        );
        assert_eq!(find("lib.rs", cx).await, None);
        assert_eq!(find("missing.rs", cx).await, None);
        // A path relative to some folder inside the project, like
        // `phantom-bin/src/engine/book.rs` for `axon-rs/crates/phantom/phantom-bin/…`.
        assert_eq!(
            find("mod_a/lib.rs", cx).await,
            Some(PathBuf::from("/repo/src/mod_a/lib.rs"))
        );
        assert_eq!(
            find("./src/mod_b/lib.rs", cx).await,
            Some(PathBuf::from("/repo/src/mod_b/lib.rs"))
        );
        // Folder names match whole, so `c/mod_a` is not the end of `src/mod_a`.
        assert_eq!(find("c/mod_a/lib.rs", cx).await, None);
    }

    #[test]
    fn symbol_like_spans_are_code_names_not_words() {
        for name in [
            "CommandPaletteSources",
            "open_agent",
            "store::bucket",
            "PaseoStore::bucket()",
            "MAX_PLACE_RESULTS",
        ] {
            assert!(is_symbol_like(name), "{name}");
        }
        for text in [
            "main",
            "Rakka",
            "true",
            "a_",
            "ctrl-k",
            "Vec<String>",
            "src/main.rs",
            "x y",
            "12_000",
        ] {
            assert!(!is_symbol_like(text), "{text}");
        }
    }

    #[test]
    fn a_clicked_name_picks_the_symbol_it_names() {
        let symbols = [
            ("bucket", Some("PaseoStore")),
            ("bucket", Some("Sidebar")),
            ("open_agent", None),
            ("open_agent_here", None),
        ];
        assert_eq!(pick_symbol("open_agent", &symbols), SymbolPick::One(2));
        assert_eq!(pick_symbol("bucket", &symbols), SymbolPick::Several);
        assert_eq!(
            pick_symbol("PaseoStore::bucket()", &symbols),
            SymbolPick::One(0)
        );
        assert_eq!(pick_symbol("missing", &symbols), SymbolPick::None);
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

    #[test]
    fn question_answers_follow_paseo_rules() {
        let parsed = questions(&request(json!({"kind":"question","input":{"questions":[
            {"header":"Color","question":"Pick one","options":[{"label":"Red"},{"label":"Blue"}],"allowOther":true},
            {"header":"Tags","question":"Pick some","options":[{"label":"a"},{"label":"b"}],"multiSelect":true,"isOther":true},
            {"header":"Note","question":"Anything else?","options":[],"allowEmpty":true,"dismissLabel":"Skip"},
            {"header":"Size","question":"Pick a size","options":[{"label":"S"}]}
        ]}})));
        assert!(parsed[0].allow_other && parsed[1].allow_other && !parsed[3].allow_other);
        assert!(shows_text_input(&parsed[0]));
        assert!(shows_text_input(&parsed[2]));
        assert!(!shows_text_input(&parsed[3]));
        assert_eq!(parsed[2].dismiss_label.as_deref(), Some("Skip"));

        assert!(!is_answered(&parsed[0], &[], "  "));
        assert!(is_answered(&parsed[0], &[], "Green"));
        assert!(is_answered(&parsed[2], &[], ""));
        assert!(!is_answered(&parsed[3], &[], "typed but hidden"));
        assert!(is_answered(&parsed[3], &["S".into()], ""));

        let selected = |index: usize| match index {
            0 => vec!["Red".to_owned()],
            1 => vec!["a".to_owned()],
            _ => Vec::new(),
        };
        let text = |index: usize| match index {
            0 => " Green ".to_owned(),
            1 => "c".to_owned(),
            3 => "ignored".to_owned(),
            _ => String::new(),
        };
        let answers = build_answers(&parsed, selected, text);
        assert_eq!(answers.get("Color"), Some(&json!("Green")));
        assert_eq!(answers.get("Tags"), Some(&json!("a, c")));
        assert_eq!(answers.get("Note"), Some(&json!("")));
        assert_eq!(answers.get("Size"), None);

        assert!(!submit_empty_on_dismiss(&parsed));
        assert!(submit_empty_on_dismiss(&parsed[2..3]));
        assert!(!submit_empty_on_dismiss(&[]));
    }

    fn chunk(sequence: u64, text: &str) -> paseo_client::TimelineEntry {
        paseo_client::TimelineEntry {
            agent_id: "agent".into(),
            epoch: "epoch".into(),
            sequence,
            timestamp: "2026-10-02T00:00:00Z".into(),
            payload: paseo_client::TimelinePayload::Message(
                serde_json::json!({"type": "assistant_message", "text": text}),
            ),
            extra: serde_json::json!({}),
        }
    }

    #[gpui::test]
    fn streamed_chunks_reach_the_rows_once_the_store_update_ends(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::PaseoSettings::register(cx);
            editor::init(cx);
        });
        let store = cx.new(|_| PaseoStore::default());
        let (view, cx) = cx.add_window_view(|window, cx| {
            AgentView::on_host(store.clone(), Some("agent".into()), None, None, window, cx)
        });
        let send = |chunks: Vec<paseo_client::TimelineEntry>, cx: &mut gpui::VisualTestContext| {
            store.update(cx, |store, cx| {
                for chunk in chunks {
                    store.state.insert_entry(chunk);
                    cx.emit(StoreEvent::TimelineChanged("agent".into()));
                }
            });
        };
        let item_texts = |view: &AgentView| {
            view.rows
                .iter()
                .filter_map(|row| match row {
                    Row::Item { item, .. } => match &item.content {
                        StreamContent::Assistant { text } => Some(text.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        send(vec![chunk(1, "Hello"), chunk(2, ", wor")], cx);
        let first = view.read_with(cx, |view, _| {
            assert!(!view.rebuild_pending);
            assert_eq!(item_texts(view), ["Hello, wor"]);
            view.projection.items().first().cloned()
        });

        send(vec![chunk(3, "ld")], cx);
        view.read_with(cx, |view, _| {
            assert_eq!(item_texts(view), ["Hello, world"]);
            let streamed = view.projection.items().first();
            assert!(
                first
                    .zip(streamed)
                    .is_some_and(|(before, after)| !Rc::ptr_eq(&before, after)),
                "the streamed message is a new item, so its row re-renders"
            );
        });
    }

    #[gpui::test]
    fn chat_rows_draw_and_a_streamed_chunk_grows_the_last(cx: &mut gpui::TestAppContext) {
        cx.update(crate::test_init);
        let store = cx.new(|_| PaseoStore::default());
        let (view, cx) = cx.add_window_view(|window, cx| {
            AgentView::on_host(store.clone(), Some("agent".into()), None, None, window, cx)
        });
        let send = |chunk: paseo_client::TimelineEntry, cx: &mut gpui::VisualTestContext| {
            store.update(cx, |store, cx| {
                store.state.insert_entry(chunk);
                cx.emit(StoreEvent::TimelineChanged("agent".into()));
            });
            cx.run_until_parked();
        };
        send(chunk(1, "Hello"), cx);
        let message_row = |cx: &mut gpui::VisualTestContext| {
            let rows = view.read_with(cx, |view, _| view.rows.len());
            (0..rows)
                .filter_map(|index| cx.debug_bounds(format!("paseo-chat-row-{index}").leak()))
                .next()
                .expect("the message row is drawn")
        };
        let width = cx.update(|window, _| window.viewport_size().width);
        let before = message_row(cx);
        assert!(before.size.height > px(0.));
        assert!(
            before.left() >= px(0.) && before.right() <= width,
            "the row {before:?} fits the chat's {width:?}"
        );
        send(chunk(2, &" and more".repeat(400)), cx);
        let after = message_row(cx);
        assert!(
            after.size.height > before.size.height,
            "the streamed text wraps onto more lines: {before:?} then {after:?}"
        );
    }

    #[gpui::test]
    fn a_subagent_chat_follows_its_parents_directory(cx: &mut gpui::TestAppContext) {
        cx.update(crate::test_init);
        let store = cx.new(|_| PaseoStore::default());
        let parent = |directory: &str| {
            let mut agent = crate::test_agent("parent", "Parent", "running");
            agent.directory = Some(PathBuf::from(directory));
            agent
        };
        store.update(cx, |store, _| {
            store.state.upsert_agent(parent("/work/project"));
            store.state.subagents.insert(
                "parent".into(),
                vec![paseo_client::ProviderSubagent {
                    id: "task".into(),
                    parent_agent_id: "parent".into(),
                    parent_subagent_id: None,
                    provider: "claude".into(),
                    title: Some("reviewer".into()),
                    description: None,
                    status: "running".into(),
                    created_at: "2026-10-03T10:00:00Z".into(),
                    updated_at: "2026-10-03T10:00:00Z".into(),
                    tool_call_id: None,
                    cwd: None,
                    subtitle: None,
                }],
            );
        });
        let timeline_id = paseo_client::subagent_timeline_id("parent", "task");
        let (view, cx) = cx.add_window_view(|window, cx| {
            AgentView::on_host(store.clone(), Some(timeline_id), None, None, window, cx)
        });
        cx.run_until_parked();
        let tools_directory = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |view, _| view.row_caches.tools_directory.clone())
        };
        assert_eq!(tools_directory(cx), Some(PathBuf::from("/work/project")));

        store.update(cx, |store, cx| {
            store.state.upsert_agent(parent("/work/other"));
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            tools_directory(cx),
            Some(PathBuf::from("/work/other")),
            "tool paths follow the directory the subagent works in"
        );
    }

    #[gpui::test]
    fn buffer_zoom_zooms_the_chat(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            let base = crate::chat_font_size(cx);
            theme_settings::increase_buffer_font_size(cx);
            theme_settings::increase_buffer_font_size(cx);
            assert_eq!(crate::chat_font_size(cx), base + px(2.));
            theme_settings::reset_buffer_font_size(cx);
            assert_eq!(crate::chat_font_size(cx), base);
        });
    }
}
