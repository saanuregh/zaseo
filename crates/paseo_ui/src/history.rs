use chrono::{DateTime, Local, NaiveDate, Utc};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, SharedString, Subscription, Task, WeakEntity, Window, prelude::*, px,
};
use paseo_client::{AgentHistoryPage, AgentSummary, RecoveryState};
use serde_json::{Value, json};
use std::time::Duration;
use ui::{
    Chip, CommonAnimationExt as _, ContextMenu, HighlightedLabel, Tooltip, prelude::*,
    right_click_menu,
};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::sidebar::{provider_icon, title_match_positions};
use crate::store::{PaseoStore, agent_provider, agent_updated_at, agent_workspace_id};
use crate::timeline::format_relative;

/// How long typing pauses before History searches, as in Paseo.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// The date sections History groups agents into, by their last activity against local days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HistorySection {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Older,
}

impl HistorySection {
    fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::ThisWeek => "This week",
            Self::ThisMonth => "This month",
            Self::Older => "Older",
        }
    }
}

/// Paseo's buckets: the same calendar day, the one before, then within a week or a month of
/// today's start. An agent without a time is Older.
pub(crate) fn history_section(activity: Option<NaiveDate>, today: NaiveDate) -> HistorySection {
    let Some(activity) = activity else {
        return HistorySection::Older;
    };
    match (today - activity).num_days() {
        days if days <= 0 => HistorySection::Today,
        1 => HistorySection::Yesterday,
        days if days <= 7 => HistorySection::ThisWeek,
        days if days <= 30 => HistorySection::ThisMonth,
        _ => HistorySection::Older,
    }
}

enum HistoryStatus {
    Loading,
    LoadingMore,
    Idle,
    Failed(String),
}

/// What a History row shows, read from the agent and its project placement.
#[derive(Clone)]
struct HistoryRow {
    agent_id: String,
    workspace_id: Option<String>,
    place: String,
    title: String,
    provider: String,
    archived: bool,
    pending: usize,
    project: Option<String>,
    branch: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

impl HistoryRow {
    fn new(agent: &AgentSummary) -> Self {
        let placement = agent.project.as_ref();
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let project = text(placement.map(|placement| &placement["projectName"]));
        let place = text(placement.map(|placement| &placement["workspaceName"]))
            .or_else(|| project.clone())
            .unwrap_or_default();
        Self {
            agent_id: agent.id.clone(),
            workspace_id: agent_workspace_id(agent).map(str::to_owned),
            place,
            title: agent
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "New session".to_owned()),
            provider: agent_provider(agent).to_owned(),
            archived: agent.extra["archivedAt"].is_string(),
            pending: agent.extra["pendingPermissions"]
                .as_array()
                .map_or(0, Vec::len),
            project,
            branch: text(placement.map(|placement| &placement["checkout"]["currentBranch"])),
            updated_at: agent_updated_at(agent),
        }
    }
}

/// Paseo's History: every agent the host keeps, active and archived, newest first in date
/// sections, searched by the host, a page at a time.
pub struct PaseoHistoryView {
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    search: Entity<Editor>,
    query: String,
    agents: Vec<AgentSummary>,
    /// The agents' rows, newest first, rebuilt only when the agents change.
    rows: Vec<HistoryRow>,
    next_cursor: Option<String>,
    search_truncated: bool,
    status: HistoryStatus,
    connection_generation: u64,
    load: Option<Task<()>>,
    search_debounce: Option<Task<()>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl PaseoHistoryView {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = crate::store(cx);
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search history", window, cx);
            editor
        });
        let subscriptions = vec![
            // History isn't live, so only a host switch or reconnect, which is another host's
            // history, needs it to load again.
            cx.observe(&store, |view: &mut Self, store, cx| {
                if store.read(cx).connection_generation != view.connection_generation {
                    view.reload(cx);
                }
            }),
            cx.subscribe(&search, |view: &mut Self, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    view.search_changed(cx);
                }
            }),
        ];
        let connection_generation = store.read(cx).connection_generation;
        let mut view = Self {
            store,
            workspace,
            search,
            query: String::new(),
            agents: Vec::new(),
            rows: Vec::new(),
            next_cursor: None,
            search_truncated: false,
            status: HistoryStatus::Loading,
            connection_generation,
            load: None,
            search_debounce: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        view.reload(cx);
        view
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text(cx).trim().to_owned();
        if query == self.query {
            // Typed back to the query on screen: the search still waiting is stale.
            self.search_debounce = None;
            return;
        }
        self.search_debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            if let Err(error) = view.update(cx, |view, cx| {
                view.query = query;
                view.reload(cx);
            }) {
                log::debug!("Paseo History closed before its search ran: {error}");
            }
        }));
    }

    /// Loads the first page again, keeping the rows on screen until it arrives.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        self.status = HistoryStatus::Loading;
        self.fetch(None, cx);
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        // A first page still loading replaces the rows and their cursor.
        if matches!(self.status, HistoryStatus::Loading) {
            return;
        }
        let Some(cursor) = self.next_cursor.clone() else {
            return;
        };
        self.status = HistoryStatus::LoadingMore;
        self.fetch(Some(cursor), cx);
    }

    fn fetch(&mut self, cursor: Option<String>, cx: &mut Context<Self>) {
        let appending = cursor.is_some();
        let search = if self.searchable(cx) {
            self.query.clone()
        } else {
            String::new()
        };
        let generation = self.store.read(cx).connection_generation;
        self.connection_generation = generation;
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.agent_history(&search, cursor).await
            })
        });
        cx.notify();
        self.load = Some(cx.spawn(async move |view, cx| {
            let result = task.await;
            if let Err(error) = view.update(cx, |view, cx| {
                if !view.store.read(cx).is_current_connection(generation) {
                    return;
                }
                match result {
                    Ok(page) => view.apply_page(page, appending, cx),
                    Err(error) => view.status = HistoryStatus::Failed(format!("{error:#}")),
                }
                cx.notify();
            }) {
                log::debug!("Paseo History closed before its page arrived: {error}");
            }
        }));
    }

    fn apply_page(&mut self, page: AgentHistoryPage, appending: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.remember_archived(
                page.agents
                    .iter()
                    .filter(|agent| agent.extra["archivedAt"].is_string())
                    .cloned(),
                cx,
            )
        });
        if appending {
            self.agents.extend(page.agents);
        } else {
            self.agents = page.agents;
        }
        self.next_cursor = page.next_cursor;
        self.search_truncated = page.search_truncated;
        self.status = HistoryStatus::Idle;
        self.agents_changed(cx);
    }

    fn agents_changed(&mut self, cx: &mut Context<Self>) {
        self.rows = self.agents.iter().map(HistoryRow::new).collect();
        self.rows
            .sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        cx.notify();
    }

    /// Paseo shows search only when the host can search its history.
    fn searchable(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .server_info
            .has_feature("agentHistorySearch")
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |editor, cx| editor.set_text("", window, cx));
    }

    fn open(&mut self, agent_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            crate::open_agent(workspace, agent_id, true, window, cx);
        });
    }

    /// Marks an agent archived or active in the rows, since History doesn't follow live changes.
    fn set_archived(&mut self, agent_id: &str, archived: bool, cx: &mut Context<Self>) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
            agent.extra["archivedAt"] = if archived {
                json!(Utc::now().to_rfc3339())
            } else {
                Value::Null
            };
        }
        self.agents_changed(cx);
    }

    fn archive_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.archive(agent_id, cx));
        self.set_archived(agent_id, true, cx);
    }

    fn unarchive_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.unarchive(agent_id, cx));
        self.set_archived(agent_id, false, cx);
    }

    fn delete_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.delete(agent_id, cx));
        self.agents.retain(|agent| agent.id != agent_id);
        self.agents_changed(cx);
    }

    /// Restoring a workspace brings back its archived agents.
    fn restore_workspace_row(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.restore_workspace(workspace_id.to_owned(), cx)
        });
        for agent in &mut self.agents {
            if agent_workspace_id(agent) == Some(workspace_id) {
                agent.extra["archivedAt"] = Value::Null;
            }
        }
        self.agents_changed(cx);
    }

    fn render_badge(icon: Option<IconName>, text: String, color: Color) -> AnyElement {
        Chip::new(text)
            .label_size(LabelSize::XSmall)
            .label_color(color)
            .when_some(icon, |chip, icon| chip.icon(icon).icon_color(color))
            .into_any_element()
    }

    fn render_text(
        text: String,
        query: &str,
        color: Color,
        size: LabelSize,
        weight: Option<FontWeight>,
    ) -> AnyElement {
        let positions = (!query.is_empty())
            .then(|| title_match_positions(&text, &query.to_lowercase()))
            .flatten();
        match positions {
            Some(positions) => HighlightedLabel::new(text, positions)
                .size(size)
                .color(color)
                .truncate()
                .into_any_element(),
            None => Label::new(text)
                .size(size)
                .color(color)
                .when_some(weight, |label, weight| label.weight(weight))
                .truncate()
                .into_any_element(),
        }
    }

    fn render_row(&self, index: usize, row: HistoryRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let query = self.query.clone();
        let now = Utc::now();
        let column = |text: Option<String>, width: f32| {
            div()
                .flex_none()
                .w(px(width))
                .min_w_0()
                .child(Self::render_text(
                    text.unwrap_or_default(),
                    &query,
                    Color::Muted,
                    LabelSize::Small,
                    None,
                ))
        };
        let agent_id = row.agent_id.clone();
        let item =
            h_flex()
                .id(("paseo-history-row", index))
                .w_full()
                .min_w_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(colors.ghost_element_hover))
                .active(|style| style.bg(colors.ghost_element_active))
                .on_click(cx.listener(move |view, _, window, cx| view.open(&agent_id, window, cx)))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .child(div().flex_none().max_w(px(320.)).min_w_0().child(
                            Self::render_text(
                                row.place.clone(),
                                &query,
                                Color::Default,
                                LabelSize::Default,
                                None,
                            ),
                        ))
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Icon::new(provider_icon(&row.provider))
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().min_w_0().flex_shrink_1().child(Self::render_text(
                            row.title.clone(),
                            &query,
                            Color::Muted,
                            LabelSize::Default,
                            None,
                        )))
                        .when(row.archived, |this| {
                            this.child(Self::render_badge(
                                Some(IconName::Archive),
                                "Archived".to_owned(),
                                Color::Muted,
                            ))
                        })
                        .when(row.pending > 0, |this| {
                            this.child(Self::render_badge(
                                None,
                                format!("{} pending", row.pending),
                                Color::Warning,
                            ))
                        }),
                )
                .child(column(row.project.clone(), 132.))
                .child(column(row.branch.clone(), 132.))
                .child(
                    div().flex_none().w(px(72.)).flex().justify_end().child(
                        Label::new(
                            row.updated_at
                                .map(|updated_at| format_relative(updated_at, now))
                                .unwrap_or_default(),
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                );
        self.row_menu(index, row, item, cx)
    }

    /// Paseo's rows have no menu; Zaseo keeps its archived-agent actions here: Unarchive, Restore
    /// Workspace and Delete Permanently, and Archive for an active agent (Paseo's long press).
    fn row_menu(
        &self,
        index: usize,
        row: HistoryRow,
        item: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.weak_entity();
        let HistoryRow {
            agent_id,
            workspace_id,
            archived,
            ..
        } = row;
        right_click_menu(("paseo-history-menu", index))
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| {
                // Asked when the menu opens, since recovery arrives after the rows.
                let restorable_workspace = workspace_id.clone().filter(|workspace_id| {
                    archived
                        && matches!(
                            crate::store(cx).read(cx).recovery.get(workspace_id),
                            Some(RecoveryState::Recoverable { .. })
                        )
                });
                let (view, agent_id) = (view.clone(), agent_id.clone());
                ContextMenu::build(window, cx, move |menu, _, _| {
                    if !archived {
                        return menu.entry(
                            "Archive",
                            None,
                            row_action(&view, &agent_id, Self::archive_row),
                        );
                    }
                    let menu = menu.entry(
                        "Unarchive",
                        None,
                        row_action(&view, &agent_id, Self::unarchive_row),
                    );
                    let menu = match &restorable_workspace {
                        Some(workspace_id) => menu.entry(
                            "Restore Workspace",
                            None,
                            row_action(&view, workspace_id, Self::restore_workspace_row),
                        ),
                        None => menu,
                    };
                    menu.separator().entry(
                        "Delete Permanently",
                        None,
                        row_action(&view, &agent_id, Self::delete_row),
                    )
                })
            })
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let today = Local::now().date_naive();
        let mut list = v_flex().gap_0p5();
        let mut current = None;
        for (index, row) in self.rows.clone().into_iter().enumerate() {
            let section = history_section(
                row.updated_at
                    .map(|updated_at| updated_at.with_timezone(&Local).date_naive()),
                today,
            );
            if current != Some(section) {
                current = Some(section);
                list = list.child(
                    div()
                        .px_3()
                        .pt_4()
                        .pb_1()
                        .child(Label::new(section.label()).color(Color::Muted)),
                );
            }
            list = list.child(self.render_row(index, row, cx));
        }
        list.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.search_truncated {
            return Some(
                Label::new("Too many matches — narrow your search")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        self.next_cursor.as_ref()?;
        let loading = matches!(
            self.status,
            HistoryStatus::Loading | HistoryStatus::LoadingMore
        );
        Some(
            Button::new(
                "paseo-history-load-more",
                if loading { "Loading..." } else { "Load more" },
            )
            .style(ButtonStyle::Subtle)
            .disabled(loading)
            .on_click(cx.listener(|view, _, _, cx| view.load_more(cx)))
            .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let searching = !self.query.is_empty();
        v_flex()
            .py_12()
            .gap_3()
            .items_center()
            .child(
                Label::new(if searching {
                    "No sessions match"
                } else {
                    "No sessions yet"
                })
                .color(Color::Muted),
            )
            .when(searching, |this| {
                this.child(
                    Button::new("paseo-history-clear-search", "Clear search")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|view, _, window, cx| view.clear_search(window, cx))),
                )
            })
            .into_any_element()
    }
}

impl Render for PaseoHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let body = match &self.status {
            HistoryStatus::Loading if self.agents.is_empty() => v_flex()
                .py_12()
                .items_center()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Medium)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .into_any_element(),
            HistoryStatus::Failed(error) if self.agents.is_empty() => v_flex()
                .py_12()
                .gap_3()
                .items_center()
                .child(Label::new("Unable to load sessions").weight(FontWeight::SEMIBOLD))
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new("paseo-history-retry", "Try again")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|view, _, _, cx| view.reload(cx))),
                )
                .into_any_element(),
            _ if self.agents.is_empty() => self.render_empty(cx),
            _ => self.render_list(cx),
        };
        let footer = (!self.agents.is_empty())
            .then(|| self.render_footer(cx))
            .flatten();
        let failure = match &self.status {
            HistoryStatus::Failed(error) if !self.agents.is_empty() => Some(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(cx.theme().status().error_background)
                    .child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(format!("Unable to load sessions: {error}"))
                                .size(LabelSize::Small),
                        ),
                    )
                    .child(
                        Button::new("paseo-history-retry-banner", "Try again")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|view, _, _, cx| view.reload(cx))),
                    ),
            ),
            _ => None,
        };
        let search = self.searchable(cx).then(|| {
            h_flex()
                .max_w(px(480.))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.editor_background)
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(div().flex_1().child(self.search.clone()))
                .when(!self.query.is_empty(), |this| {
                    this.child(
                        IconButton::new("paseo-history-clear", IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Clear search"))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.clear_search(window, cx)),
                            ),
                    )
                })
        });
        div()
            .id("paseo-history")
            .key_context("PaseoHistory")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(colors.editor_background)
            .child(
                v_flex()
                    .w_full()
                    .px_6()
                    .py_4()
                    .gap_2()
                    .children(search)
                    .children(failure)
                    .child(body)
                    .children(footer.map(|footer| h_flex().py_3().justify_center().child(footer))),
            )
    }
}

impl Focusable for PaseoHistoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for PaseoHistoryView {}

impl Item for PaseoHistoryView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "History".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::HistoryRerun))
    }
}

/// A menu entry's handler running `action` on the History view for `id`.
fn row_action(
    view: &WeakEntity<PaseoHistoryView>,
    id: &str,
    action: fn(&mut PaseoHistoryView, &str, &mut Context<PaseoHistoryView>),
) -> impl Fn(&mut Window, &mut App) + 'static {
    let (view, id) = (view.clone(), id.to_owned());
    move |_, cx| {
        if let Err(error) = view.update(cx, |view, cx| action(view, &id, cx)) {
            log::debug!("Paseo History closed: {error}");
        }
    }
}

/// Opens the History tab, reusing and refreshing an open one, since History doesn't follow live
/// changes (Paseo refetches it each visit).
pub(crate) fn open_history(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<PaseoHistoryView>(cx).next();
    if let Some(existing) = existing {
        existing.update(cx, |view, cx| view.reload(cx));
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak_workspace = cx.weak_entity();
    let view = cx.new(|cx| PaseoHistoryView::new(weak_workspace, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_sections_follow_local_days() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 1).expect("date");
        let days_ago = |days: i64| Some(today - chrono::Duration::days(days));
        assert_eq!(history_section(days_ago(0), today), HistorySection::Today);
        assert_eq!(history_section(days_ago(-1), today), HistorySection::Today);
        assert_eq!(
            history_section(days_ago(1), today),
            HistorySection::Yesterday
        );
        assert_eq!(
            history_section(days_ago(2), today),
            HistorySection::ThisWeek
        );
        assert_eq!(
            history_section(days_ago(7), today),
            HistorySection::ThisWeek
        );
        assert_eq!(
            history_section(days_ago(8), today),
            HistorySection::ThisMonth
        );
        assert_eq!(
            history_section(days_ago(30), today),
            HistorySection::ThisMonth
        );
        assert_eq!(history_section(days_ago(31), today), HistorySection::Older);
        assert_eq!(history_section(None, today), HistorySection::Older);
    }

    #[test]
    fn history_rows_read_the_placement_like_paseo() {
        let mut agent = crate::store::test_agent("a", "idle", json!({}));
        agent.title = None;
        agent.project = Some(json!({
            "projectKey": "p",
            "projectName": "axon",
            "workspaceName": "Design Cortex",
            "checkout": {"currentBranch": "main"}
        }));
        agent.extra["archivedAt"] = json!("2026-09-30T10:00:00Z");
        agent.extra["pendingPermissions"] = json!([{}, {}]);
        let row = HistoryRow::new(&agent);
        assert_eq!(row.place, "Design Cortex");
        assert_eq!(row.title, "New session");
        assert_eq!(row.project.as_deref(), Some("axon"));
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert!(row.archived);
        assert_eq!(row.pending, 2);

        agent.project =
            Some(json!({"projectKey": "p", "projectName": "axon", "workspaceName": null}));
        assert_eq!(HistoryRow::new(&agent).place, "axon");
    }
}
