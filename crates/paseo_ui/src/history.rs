use chrono::{DateTime, Local, NaiveDate, Utc};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, IntoElement, SharedString, Subscription, Task, WeakEntity, Window, prelude::*, px,
};
use paseo_client::{AgentHistoryPage, AgentSummary, RecoveryState};
use serde_json::{Value, json};
use std::time::Duration;
use ui::{Chip, ContextMenu, HighlightedLabel, Tooltip, prelude::*, right_click_menu};
use workspace::{Item, Workspace, item::ItemEvent};

use crate::sidebar::title_match_positions;
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
/// What every History row reads, worked out once per render.
struct RowStyle<'a> {
    lower_query: &'a str,
    hover: gpui::Hsla,
    active: gpui::Hsla,
    now: DateTime<Utc>,
}

struct HistoryRow {
    agent_id: String,
    workspace_id: Option<String>,
    place: String,
    title: String,
    provider: String,
    /// The daemon's icon for a provider Zaseo has no icon of its own for.
    provider_icon_svg: Option<String>,
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
                .unwrap_or_else(|| "New agent".to_owned()),
            provider: agent_provider(agent).to_owned(),
            provider_icon_svg: None,
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

/// One host's History pages.
struct HostHistory {
    store: Entity<PaseoStore>,
    agents: Vec<AgentSummary>,
    next_cursor: Option<String>,
    search_truncated: bool,
    connection_generation: u64,
    error: Option<String>,
    /// The page request in flight; `Some` while this host is loading.
    load: Option<Task<()>>,
}

/// Paseo's History: every agent each host keeps, active and archived, newest first in date
/// sections, searched by the hosts, a page per host at a time.
pub struct PaseoHistoryView {
    hosts: Vec<HostHistory>,
    workspace: WeakEntity<Workspace>,
    search: Entity<Editor>,
    query: String,
    /// The agents' rows from every host, newest first, rebuilt only when the agents change.
    rows: Vec<HistoryRow>,
    status: HistoryStatus,
    search_debounce: Option<Task<()>>,
    focus_handle: FocusHandle,
    _timestamp_ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PaseoHistoryView {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search history", window, cx);
            editor
        });
        let subscriptions = vec![
            // History isn't live, so only a host added, removed or reconnected, which changes
            // whose history is listed, needs it to load again.
            cx.observe(&crate::hosts::registry(cx), |view: &mut Self, _, cx| {
                if view.hosts_changed(cx) {
                    view.reload(cx);
                }
            }),
            cx.subscribe(&search, |view: &mut Self, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    view.search_changed(cx);
                }
            }),
        ];
        let mut view = Self {
            hosts: Vec::new(),
            workspace,
            search,
            query: String::new(),
            rows: Vec::new(),
            status: HistoryStatus::Loading,
            search_debounce: None,
            focus_handle: cx.focus_handle(),
            _timestamp_ticker: crate::sidebar::redraw_every_minute(cx),
            _subscriptions: subscriptions,
        };
        view.reload(cx);
        view
    }

    fn hosts_changed(&self, cx: &App) -> bool {
        let stores = crate::hosts::stores(cx);
        stores.len() != self.hosts.len()
            || stores.iter().zip(&self.hosts).any(|(store, host)| {
                *store != host.store
                    || store.read(cx).connection_generation != host.connection_generation
            })
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

    /// Loads every host's first page again, keeping the rows on screen until they arrive.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let previous = std::mem::take(&mut self.hosts);
        self.hosts = crate::hosts::stores(cx)
            .into_iter()
            .map(|store| {
                // A host still listed keeps its rows on screen while its first page reloads.
                let agents = previous
                    .iter()
                    .find(|host| host.store == store)
                    .map(|host| host.agents.clone())
                    .unwrap_or_default();
                HostHistory {
                    connection_generation: store.read(cx).connection_generation,
                    store,
                    agents,
                    next_cursor: None,
                    search_truncated: false,
                    error: None,
                    load: None,
                }
            })
            .collect();
        for index in 0..self.hosts.len() {
            self.fetch(index, None, cx);
        }
        self.update_status(false);
        self.agents_changed(cx);
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        // A first page still loading replaces the rows and their cursor.
        if matches!(self.status, HistoryStatus::Loading) {
            return;
        }
        let pages = self
            .hosts
            .iter()
            .enumerate()
            .filter_map(|(index, host)| Some((index, host.next_cursor.clone()?)))
            .collect::<Vec<_>>();
        if pages.is_empty() {
            return;
        }
        for (index, cursor) in pages {
            self.fetch(index, Some(cursor), cx);
        }
        self.update_status(true);
        cx.notify();
    }

    fn fetch(&mut self, index: usize, cursor: Option<String>, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index) else {
            return;
        };
        let searching = !self.query.is_empty();
        let host_searches = Self::host_searches(&host.store, cx);
        // A host that can't search would list everything under a query, so it sits out.
        if searching && !host_searches {
            if let Some(host) = self.hosts.get_mut(index) {
                host.agents.clear();
                host.next_cursor = None;
            }
            return;
        }
        let appending = cursor.is_some();
        let search = self.query.clone();
        let store = host.store.clone();
        let generation = store.read(cx).connection_generation;
        let task = store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.agent_history(&search, cursor).await
            })
        });
        let load = cx.spawn(async move |view, cx| {
            let result = task.await;
            if let Err(error) = view.update(cx, |view, cx| {
                let Some(host) = view.hosts.get_mut(index) else {
                    return;
                };
                if host.store != store || !store.read(cx).is_current_connection(generation) {
                    return;
                }
                host.load = None;
                match result {
                    Ok(page) => {
                        Self::apply_page(host, page, appending, cx);
                        host.error = None;
                    }
                    Err(error) => host.error = Some(format!("{error:#}")),
                }
                view.update_status(false);
                view.agents_changed(cx);
            }) {
                log::debug!("Paseo History closed before its page arrived: {error}");
            }
        });
        if let Some(host) = self.hosts.get_mut(index) {
            host.connection_generation = generation;
            host.load = Some(load);
        }
        cx.notify();
    }

    fn apply_page(
        host: &mut HostHistory,
        page: AgentHistoryPage,
        appending: bool,
        cx: &mut Context<Self>,
    ) {
        host.store.update(cx, |store, cx| {
            store.remember_archived(
                page.agents
                    .iter()
                    .filter(|agent| agent.extra["archivedAt"].is_string())
                    .cloned(),
                cx,
            )
        });
        if appending {
            host.agents.extend(page.agents);
        } else {
            host.agents = page.agents;
        }
        host.next_cursor = page.next_cursor;
        host.search_truncated = page.search_truncated;
    }

    /// Loading while any first page is out, failed only when every host failed and nothing
    /// is listed.
    fn update_status(&mut self, loading_more: bool) {
        let loading = self.hosts.iter().any(|host| host.load.is_some());
        let was_loading_more = matches!(self.status, HistoryStatus::LoadingMore);
        self.status = if loading && (loading_more || was_loading_more) {
            HistoryStatus::LoadingMore
        } else if loading {
            HistoryStatus::Loading
        } else if let Some(error) = self
            .hosts
            .iter()
            .find_map(|host| host.error.clone())
            .filter(|_| self.hosts.iter().all(|host| host.error.is_some()))
        {
            HistoryStatus::Failed(error)
        } else {
            HistoryStatus::Idle
        };
    }

    fn agents_changed(&mut self, cx: &mut Context<Self>) {
        self.rows = self
            .hosts
            .iter()
            .flat_map(|host| {
                let store = host.store.read(cx);
                host.agents.iter().map(move |agent| {
                    let mut row = HistoryRow::new(agent);
                    row.provider_icon_svg =
                        crate::sidebar::provider_icon_svg(store, &row.provider).map(str::to_owned);
                    row
                })
            })
            .collect();
        self.rows
            .sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        cx.notify();
    }

    fn host_searches(store: &Entity<PaseoStore>, cx: &App) -> bool {
        store.read(cx).server_info.has_feature("agentHistorySearch")
    }

    /// Paseo shows search only when a host can search its history.
    fn searchable(&self, cx: &App) -> bool {
        self.hosts
            .iter()
            .any(|host| Self::host_searches(&host.store, cx))
    }

    fn has_more(&self) -> bool {
        self.hosts.iter().any(|host| host.next_cursor.is_some())
    }

    fn search_truncated(&self) -> bool {
        self.hosts.iter().any(|host| host.search_truncated)
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

    /// The host listing an agent's row.
    fn host_of_agent(&mut self, agent_id: &str) -> Option<&mut HostHistory> {
        self.hosts
            .iter_mut()
            .find(|host| host.agents.iter().any(|agent| agent.id == agent_id))
    }

    /// Marks an agent archived or active in the rows, since History doesn't follow live changes.
    fn set_archived(&mut self, agent_id: &str, archived: bool, cx: &mut Context<Self>) {
        if let Some(agent) = self
            .host_of_agent(agent_id)
            .and_then(|host| host.agents.iter_mut().find(|agent| agent.id == agent_id))
        {
            agent.extra["archivedAt"] = if archived {
                json!(Utc::now().to_rfc3339())
            } else {
                Value::Null
            };
        }
        self.agents_changed(cx);
    }

    fn archive_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if let Some(store) = self.host_of_agent(agent_id).map(|host| host.store.clone()) {
            store.update(cx, |store, cx| store.archive(agent_id, cx));
        }
        self.set_archived(agent_id, true, cx);
    }

    fn unarchive_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if let Some(store) = self.host_of_agent(agent_id).map(|host| host.store.clone()) {
            store.update(cx, |store, cx| store.unarchive(agent_id, cx));
        }
        self.set_archived(agent_id, false, cx);
    }

    fn delete_row(&mut self, agent_id: &str, cx: &mut Context<Self>) {
        if let Some(host) = self.host_of_agent(agent_id) {
            let store = host.store.clone();
            host.agents.retain(|agent| agent.id != agent_id);
            store.update(cx, |store, cx| store.delete(agent_id, cx));
        }
        self.agents_changed(cx);
    }

    /// Restoring a workspace brings back its archived agents.
    fn restore_workspace_row(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter_mut().find(|host| {
            host.agents
                .iter()
                .any(|agent| agent_workspace_id(agent) == Some(workspace_id))
        }) else {
            return;
        };
        host.store.update(cx, |store, cx| {
            store.restore_workspace(workspace_id.to_owned(), cx)
        });
        for agent in &mut host.agents {
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
        lower_query: &str,
        color: Color,
        size: LabelSize,
        weight: Option<FontWeight>,
    ) -> AnyElement {
        let positions = (!lower_query.is_empty())
            .then(|| title_match_positions(&text, lower_query))
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

    fn render_row(
        &self,
        index: usize,
        row: &HistoryRow,
        style: &RowStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = style.lower_query;
        let (hover, active) = (style.hover, style.active);
        let column = |text: Option<String>, width: f32| {
            div()
                .flex_none()
                .w(px(width))
                .min_w_0()
                .child(Self::render_text(
                    text.unwrap_or_default(),
                    query,
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
                .hover(|style| style.bg(hover))
                .active(|style| style.bg(active))
                .on_click(cx.listener(move |view, _, window, cx| view.open(&agent_id, window, cx)))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .child(div().flex_none().max_w(px(320.)).min_w_0().child(
                            Self::render_text(
                                row.place.clone(),
                                query,
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
                        .child(crate::sidebar::source_icon(
                            &row.provider,
                            row.provider_icon_svg.as_deref(),
                            IconSize::Small,
                            cx,
                        ))
                        .child(div().min_w_0().flex_shrink_1().child(Self::render_text(
                            row.title.clone(),
                            query,
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
                                .map(|updated_at| format_relative(updated_at, style.now))
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
        row: &HistoryRow,
        item: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.weak_entity();
        let agent_id = row.agent_id.clone();
        let workspace_id = row.workspace_id.clone();
        let archived = row.archived;
        right_click_menu(("paseo-history-menu", index))
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| {
                // Asked when the menu opens, since recovery arrives after the rows.
                let restorable_workspace = workspace_id.clone().filter(|workspace_id| {
                    archived
                        && crate::hosts::stores(cx).iter().any(|store| {
                            matches!(
                                store.read(cx).recovery.get(workspace_id),
                                Some(RecoveryState::Recoverable { .. })
                            )
                        })
                });
                let (view, agent_id) = (view.clone(), agent_id.clone());
                ContextMenu::build(window, cx, move |menu, _, _| {
                    if !archived {
                        return menu.entry(
                            "Archive",
                            Some(Box::new(crate::ArchiveAgent)),
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
                    menu.separator()
                        .entry("Delete Permanently…", None, move |window, cx| {
                            let (view, agent_id) = (view.clone(), agent_id.clone());
                            crate::workspace_tools::confirm_then(
                                "Delete this agent permanently?",
                                "Its conversation is removed from the host and can't be recovered.",
                                "Delete Permanently",
                                move |cx| {
                                    if let Err(error) =
                                        view.update(cx, |view, cx| view.delete_row(&agent_id, cx))
                                    {
                                        log::debug!("Paseo History closed: {error}");
                                    }
                                },
                                window,
                                cx,
                            );
                        })
                })
            })
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let today = Local::now().date_naive();
        let lower_query = self.query.to_lowercase();
        let colors = cx.theme().colors();
        let style = RowStyle {
            lower_query: &lower_query,
            hover: colors.ghost_element_hover,
            active: colors.ghost_element_active,
            now: Utc::now(),
        };
        let mut list = v_flex().gap_0p5();
        let mut current = None;
        for (index, row) in self.rows.iter().enumerate() {
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
            list = list.child(self.render_row(index, row, &style, cx));
        }
        list.into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.search_truncated() {
            return Some(
                Label::new("Too many matches — narrow your search")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        self.has_more().then_some(())?;
        let loading = matches!(
            self.status,
            HistoryStatus::Loading | HistoryStatus::LoadingMore
        );
        Some(
            Button::new(
                "paseo-history-load-more",
                if loading { "Loading…" } else { "Load more" },
            )
            .style(ButtonStyle::Subtle)
            .disabled(loading)
            .on_click(cx.listener(|view, _, _, cx| view.load_more(cx)))
            .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let searching = !self.query.is_empty();
        let message = crate::render_message(
            if searching {
                "No agents match"
            } else {
                "No agents yet"
            },
            None,
            searching.then(|| {
                Button::new("paseo-history-clear-search", "Clear search")
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|view, _, window, cx| view.clear_search(window, cx)))
                    .into_any_element()
            }),
        );
        v_flex()
            .py_8()
            .items_center()
            .child(message)
            .into_any_element()
    }
}

impl Render for PaseoHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let (border_variant, editor_background) = (colors.border_variant, colors.editor_background);
        let body = match &self.status {
            HistoryStatus::Loading if self.rows.is_empty() => crate::render_loading("Loading…"),
            HistoryStatus::Failed(error) if self.rows.is_empty() => crate::render_error(
                "Unable to load agents",
                error.clone(),
                Some(
                    h_flex()
                        .child(
                            Button::new("paseo-history-retry", "Try again")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|view, _, _, cx| view.reload(cx))),
                        )
                        .into_any_element(),
                ),
                cx,
            ),
            _ if self.rows.is_empty() => self.render_empty(cx),
            _ => self.render_list(cx),
        };
        let footer = (!self.rows.is_empty())
            .then(|| self.render_footer(cx))
            .flatten();
        let failure = match &self.status {
            HistoryStatus::Failed(error) if !self.rows.is_empty() => Some(
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
                            Label::new(format!("Unable to load agents: {error}"))
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
                .border_color(border_variant)
                .bg(editor_background)
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
            .key_context("PaseoHistory PaseoView")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(editor_background)
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

    #[gpui::test]
    fn an_empty_history_says_so_inside_the_view(cx: &mut gpui::TestAppContext) {
        cx.update(crate::test_init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            PaseoHistoryView::new(WeakEntity::new_invalid(), window, cx)
        });
        cx.run_until_parked();
        // What a daemon with no agents answers, through the path a loaded page takes.
        view.update(cx, |view, cx| {
            for host in &mut view.hosts {
                host.load = None;
                host.error = None;
                let empty = AgentHistoryPage {
                    agents: Vec::new(),
                    next_cursor: None,
                    search_truncated: false,
                };
                PaseoHistoryView::apply_page(host, empty, false, cx);
            }
            view.update_status(false);
            view.agents_changed(cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("paseo-error").is_none(),
            "a loaded page shows no error"
        );
        let message = cx.debug_bounds("paseo-message").expect("the empty state");
        let width = cx.update(|window, _| window.viewport_size().width);
        assert!(message.size.height > gpui::px(0.));
        assert!(
            message.left() >= gpui::px(0.) && message.right() <= width,
            "{message:?} fits {width:?}"
        );
    }

    #[gpui::test]
    fn a_failed_load_shows_its_error_inside_the_view(cx: &mut gpui::TestAppContext) {
        cx.update(crate::test_init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            PaseoHistoryView::new(WeakEntity::new_invalid(), window, cx)
        });
        cx.run_until_parked();
        let error = cx
            .debug_bounds("paseo-error")
            .expect("a host that isn't connected fails to load");
        let width = cx.update(|window, _| window.viewport_size().width);
        assert!(error.size.height > gpui::px(0.));
        assert!(
            error.left() >= gpui::px(0.) && error.right() <= width,
            "{error:?} fits {width:?}"
        );
    }

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
        assert_eq!(row.title, "New agent");
        assert_eq!(row.project.as_deref(), Some("axon"));
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert!(row.archived);
        assert_eq!(row.pending, 2);

        agent.project =
            Some(json!({"projectKey": "p", "projectName": "axon", "workspaceName": null}));
        assert_eq!(HistoryRow::new(&agent).place, "axon");
    }
}
