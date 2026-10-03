use anyhow::Result;
use chrono::{DateTime, Utc};
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    Action as _, Animation, AnimationExt as _, AnyElement, App, AppContext as _,
    AsyncWindowContext, ClipboardItem, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Global, IntoElement, ListAlignment, ListOffset, ListState, Pixels, Subscription,
    Task, WeakEntity, Window, ease_in_out, list, prelude::*, pulsating_between, px, relative,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use paseo_client::{AgentSummary, ProjectDescriptor, WorkspaceDescriptor, WorkspaceLabel};
use settings::Settings as _;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ui::{
    ContextMenu, HighlightedLabel, IconButton, Indicator, KeyBinding, PopoverMenu, Tooltip,
    prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::store::{
    AgentBucket, ConnectionStatus, PaseoStore, agent_branch, agent_bucket, agent_last_message_at,
    agent_project_directory, agent_project_key, agent_project_name, agent_provider,
    agent_requires_attention, agent_title, agent_updated_at, agent_workspace_id,
    agent_worktree_name,
};
use crate::stream::{ENTRANCE, fade_in_since, rotating_chevron};
use crate::timeline::{format_relative, parse_timestamp};
use crate::workspace_tools;
use crate::{
    ArchiveAgent, CopyAgentId, FocusSidebarFilter, ManageHosts, NewAgentWorkspace, OpenWorkspace,
    PaseoSettings, Reconnect, RenameAgent, ToggleGroupByStatus, TogglePanel, connection_picker,
    hosts::{self, HostsEvent},
    open_agent,
};

const GROUPING_KEY: &str = "paseo_sidebar_group_by_status";
/// The hosts the sidebar lists, as a JSON list of profile names; empty lists every host.
const HOST_FILTER_KEY: &str = "paseo_sidebar_host_filter";

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SidebarEntry {
    Header {
        key: String,
        label: String,
        count: usize,
        collapsed: bool,
        /// Where "+" starts a new agent.
        directory: Option<PathBuf>,
        project_id: Option<String>,
        /// A label group's color.
        color: Option<String>,
    },
    Workspace {
        workspace_id: String,
        collapsed: bool,
        agent_count: usize,
        highlight_positions: Vec<usize>,
        /// The workspace's only agent, shown in the workspace's row instead of a row of its own.
        single_agent: Option<String>,
    },
    Agent {
        agent_id: String,
        /// Shown under its workspace's row, which already names its branch and worktree.
        nested: bool,
        highlight_positions: Vec<usize>,
    },
}

pub(crate) fn provider_icon(provider: &str) -> IconName {
    match provider {
        "claude" | "claude-code" => IconName::AiClaude,
        "codex" | "openai" => IconName::AiOpenAi,
        "opencode" => IconName::AiOpenCode,
        "gemini" => IconName::AiGemini,
        "copilot" => IconName::Copilot,
        _ => IconName::Sparkle,
    }
}

/// How many agents each workspace has, so an agent's display title, its workspace's name when it
/// is alone there, takes one lookup rather than a scan of every agent.
pub(crate) struct WorkspaceAgentCounts<'a>(HashMap<&'a str, usize>);

impl<'a> WorkspaceAgentCounts<'a> {
    pub(crate) fn new(agents: impl IntoIterator<Item = &'a AgentSummary>) -> Self {
        let mut counts = HashMap::new();
        for agent in agents {
            if let Some(workspace_id) = agent_workspace_id(agent) {
                *counts.entry(workspace_id).or_insert(0) += 1;
            }
        }
        Self(counts)
    }

    /// [`crate::store::agent_display_title`] for one of the counted agents.
    pub(crate) fn display_title<Id, Descriptor>(
        &self,
        workspaces: &BTreeMap<Id, Descriptor>,
        agent: &AgentSummary,
    ) -> String
    where
        Id: std::borrow::Borrow<str> + Ord,
        Descriptor: std::borrow::Borrow<WorkspaceDescriptor>,
    {
        agent_workspace_id(agent)
            .filter(|workspace_id| self.0.get(workspace_id) == Some(&1))
            .and_then(|workspace_id| workspaces.get(workspace_id))
            .map(<Descriptor as std::borrow::Borrow<WorkspaceDescriptor>>::borrow)
            .filter(|workspace| !workspace.name.trim().is_empty())
            .map_or_else(|| agent_title(agent), |workspace| workspace.name.clone())
    }
}

/// What every agent's filter match shares, worked out once per rebuild.
struct FilterContext<'a> {
    filter: &'a str,
    lower_filter: String,
    titles: WorkspaceAgentCounts<'a>,
}

impl<'a> FilterContext<'a> {
    fn new(inputs: &SidebarInputs<'a>) -> Self {
        Self {
            filter: inputs.filter,
            lower_filter: inputs.filter.to_lowercase(),
            titles: WorkspaceAgentCounts::new(inputs.agents.iter().copied()),
        }
    }
}

fn matches_filter(
    agent: &AgentSummary,
    inputs: &SidebarInputs,
    context: &FilterContext,
) -> Option<Vec<usize>> {
    let filter = context.filter;
    if filter.is_empty() {
        return Some(Vec::new());
    }
    let lower_filter = &context.lower_filter;
    let title = context.titles.display_title(inputs.workspaces, agent);
    if let Some(positions) = title_match_positions(&title, lower_filter) {
        return Some(positions);
    }
    let project = agent_project_name(agent).to_lowercase();
    (project.contains(lower_filter.as_str())
        || agent_title(agent)
            .to_lowercase()
            .contains(lower_filter.as_str())
        || agent_provider(agent).contains(lower_filter.as_str())
        || agent.id.starts_with(filter))
    .then(Vec::new)
}

/// Byte offsets of the title characters matching a lowercase filter. Offsets come from the original
/// title so they stay on character boundaries even when lowercasing changes byte lengths.
pub(crate) fn title_match_positions(title: &str, lower_filter: &str) -> Option<Vec<usize>> {
    let filter_length = lower_filter.chars().count();
    title.char_indices().find_map(|(start, _)| {
        let rest = title.get(start..)?;
        lowercase_starts_with(rest, lower_filter).then(|| {
            rest.char_indices()
                .take(filter_length)
                .map(|(offset, _)| start + offset)
                .collect()
        })
    })
}

/// Whether `text` in lower case starts with `lower_prefix`, lowercasing only as far as the prefix
/// reaches instead of the whole text.
fn lowercase_starts_with(text: &str, lower_prefix: &str) -> bool {
    let mut lowered = text.chars().flat_map(char::to_lowercase);
    lower_prefix
        .chars()
        .all(|expected| lowered.next() == Some(expected))
}

/// How the sidebar groups its rows, like Paseo's sidebar grouping preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SidebarGrouping {
    #[default]
    Project,
    Status,
    Labels,
}

impl From<settings::PaseoSidebarGrouping> for SidebarGrouping {
    fn from(grouping: settings::PaseoSidebarGrouping) -> Self {
        match grouping {
            settings::PaseoSidebarGrouping::Project => Self::Project,
            settings::PaseoSidebarGrouping::Status => Self::Status,
            settings::PaseoSidebarGrouping::Labels => Self::Labels,
        }
    }
}

impl From<SidebarGrouping> for settings::PaseoSidebarGrouping {
    fn from(grouping: SidebarGrouping) -> Self {
        match grouping {
            SidebarGrouping::Project => Self::Project,
            SidebarGrouping::Status => Self::Status,
            SidebarGrouping::Labels => Self::Labels,
        }
    }
}

impl SidebarGrouping {
    fn stored(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Status => "status",
            Self::Labels => "labels",
        }
    }

    /// Reads the saved grouping, including `1` from when the only choice was status.
    fn from_stored(value: &str) -> Self {
        match value {
            "status" | "1" => Self::Status,
            "labels" => Self::Labels,
            _ => Self::Project,
        }
    }
}

pub(crate) struct SidebarInputs<'a> {
    pub agents: &'a [&'a AgentSummary],
    pub workspaces: &'a BTreeMap<&'a str, &'a WorkspaceDescriptor>,
    pub projects: &'a BTreeMap<&'a str, &'a ProjectDescriptor>,
    pub labels: &'a [&'a WorkspaceLabel],
    pub pending_permission_agents: &'a HashSet<&'a str>,
    pub grouping: SidebarGrouping,
    pub collapsed: &'a HashSet<String>,
    pub filter: &'a str,
}

/// A workspace and the agents shown under it, with when anything in it last happened.
struct WorkspaceGroup<'a> {
    workspace: &'a WorkspaceDescriptor,
    agents: Vec<(&'a AgentSummary, Vec<usize>)>,
    activity: Option<DateTime<Utc>>,
    highlight_positions: Vec<usize>,
}

fn workspace_collapse_key(workspace_id: &str) -> String {
    format!("workspace:{workspace_id}")
}

/// Paseo's label colors, named after Tailwind's, drawn from the theme's terminal palette so they
/// suit any theme. The bright shades keep every name distinct where Tailwind has two blues and two
/// cyans.
pub(crate) fn label_color(name: &str, cx: &App) -> gpui::Hsla {
    let colors = cx.theme().colors();
    match name {
        "red" => colors.terminal_ansi_red,
        "amber" => colors.terminal_ansi_yellow,
        "emerald" => colors.terminal_ansi_green,
        "sky" => colors.terminal_ansi_cyan,
        "teal" => colors.terminal_ansi_bright_cyan,
        "indigo" => colors.terminal_ansi_bright_blue,
        "violet" => colors.terminal_ansi_magenta,
        "pink" => colors.terminal_ansi_bright_magenta,
        "orange" => colors.terminal_ansi_bright_red,
        _ => colors.terminal_ansi_blue,
    }
}

pub(crate) fn project_label(project: &ProjectDescriptor) -> String {
    project
        .custom_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| project.display_name.clone())
}

/// The sidebar's visible order. Project grouping shows pinned workspaces first, then projects by
/// most recent activity, each with its workspaces and their agents; agents the daemon hasn't
/// placed in a workspace sit directly under their project. Status grouping lists agents in Paseo's
/// status buckets, and label grouping lists workspaces under each label.
pub(crate) fn build_entries(inputs: &SidebarInputs) -> Vec<SidebarEntry> {
    let mut sorted = inputs.agents.to_vec();
    let context = FilterContext::new(inputs);
    match inputs.grouping {
        SidebarGrouping::Status => {
            // By the last message the user sent, so agents working at the same time don't swap
            // places on every update they report.
            sorted.sort_by_cached_key(|agent| std::cmp::Reverse(agent_last_message_at(agent)));
            status_entries(&sorted, inputs, &context)
        }
        SidebarGrouping::Project | SidebarGrouping::Labels => {
            sorted.sort_by_cached_key(|agent| std::cmp::Reverse(agent_updated_at(agent)));
            workspace_entries(&sorted, inputs, &context)
        }
    }
}

/// One row per workspace, as Paseo lists them, under its most urgent agent's state, so opening
/// another agent in a workspace neither adds a row nor retitles the ones there. Agents outside
/// any workspace keep rows of their own.
fn status_entries(
    sorted: &[&AgentSummary],
    inputs: &SidebarInputs,
    context: &FilterContext,
) -> Vec<SidebarEntry> {
    struct StatusWorkspace<'a> {
        bucket: AgentBucket,
        agents: Vec<&'a AgentSummary>,
        matched: bool,
    }
    enum StatusRow<'a> {
        Workspace(&'a str),
        Agent(&'a AgentSummary, AgentBucket, Vec<usize>),
    }
    // Rows in the order of `sorted`; a workspace's slot is its first agent's.
    let mut order: Vec<StatusRow> = Vec::new();
    let mut workspaces: HashMap<&str, StatusWorkspace> = HashMap::new();
    for agent in sorted {
        let bucket = agent_bucket(
            agent,
            inputs.pending_permission_agents.contains(agent.id.as_str()),
        );
        let workspace_id = agent_workspace_id(agent)
            .filter(|workspace_id| inputs.workspaces.contains_key(*workspace_id));
        match workspace_id {
            Some(workspace_id) => {
                let workspace = workspaces.entry(workspace_id).or_insert_with(|| {
                    order.push(StatusRow::Workspace(workspace_id));
                    StatusWorkspace {
                        bucket,
                        agents: Vec::new(),
                        matched: false,
                    }
                });
                workspace.bucket = workspace.bucket.min(bucket);
                workspace.agents.push(agent);
                workspace.matched |= matches_filter(agent, inputs, context).is_some();
            }
            None => {
                if let Some(highlights) = matches_filter(agent, inputs, context) {
                    order.push(StatusRow::Agent(agent, bucket, highlights));
                }
            }
        }
    }
    let mut groups: BTreeMap<AgentBucket, Vec<SidebarEntry>> = BTreeMap::new();
    for row in order {
        match row {
            StatusRow::Workspace(workspace_id) => {
                let Some(workspace) = workspaces.get(workspace_id) else {
                    continue;
                };
                let name_positions = inputs.workspaces.get(workspace_id).and_then(|descriptor| {
                    if inputs.filter.is_empty() {
                        Some(Vec::new())
                    } else {
                        title_match_positions(&descriptor.name, &context.lower_filter)
                    }
                });
                if name_positions.is_none() && !workspace.matched {
                    continue;
                }
                let single_agent = match workspace.agents.as_slice() {
                    [agent] => Some(agent.id.clone()),
                    _ => None,
                };
                groups
                    .entry(workspace.bucket)
                    .or_default()
                    .push(SidebarEntry::Workspace {
                        workspace_id: workspace_id.to_owned(),
                        collapsed: false,
                        agent_count: workspace.agents.len(),
                        highlight_positions: name_positions.unwrap_or_default(),
                        single_agent,
                    });
            }
            StatusRow::Agent(agent, bucket, highlight_positions) => {
                groups.entry(bucket).or_default().push(SidebarEntry::Agent {
                    agent_id: agent.id.clone(),
                    nested: false,
                    highlight_positions,
                });
            }
        }
    }
    let mut entries = Vec::new();
    for (bucket, rows) in groups {
        let key = format!("status:{}", bucket as u8);
        let collapsed = inputs.collapsed.contains(&key) && inputs.filter.is_empty();
        entries.push(SidebarEntry::Header {
            key,
            label: bucket.label().to_owned(),
            count: rows.len(),
            collapsed,
            directory: None,
            project_id: None,
            color: None,
        });
        if !collapsed {
            entries.extend(rows);
        }
    }
    entries
}

fn workspace_entries(
    sorted: &[&AgentSummary],
    inputs: &SidebarInputs,
    context: &FilterContext,
) -> Vec<SidebarEntry> {
    let mut groups: BTreeMap<&str, WorkspaceGroup> = BTreeMap::new();
    let mut unplaced: Vec<(&AgentSummary, Vec<usize>)> = Vec::new();
    for workspace in inputs.workspaces.values().copied() {
        let highlight_positions = if inputs.filter.is_empty() {
            Some(Vec::new())
        } else {
            title_match_positions(&workspace.name, &context.lower_filter)
        };
        groups.insert(
            workspace.id.as_str(),
            WorkspaceGroup {
                workspace,
                agents: Vec::new(),
                activity: workspace.activity_at.as_deref().and_then(parse_timestamp),
                highlight_positions: highlight_positions.clone().unwrap_or_default(),
            },
        );
    }
    let name_matches =
        |group: &WorkspaceGroup| inputs.filter.is_empty() || !group.highlight_positions.is_empty();
    for agent in sorted {
        let workspace_id = agent_workspace_id(agent);
        match workspace_id.and_then(|workspace_id| groups.get_mut(workspace_id)) {
            Some(group) => {
                group.activity = group.activity.max(agent_updated_at(agent));
                let highlights = matches_filter(agent, inputs, context)
                    .or_else(|| name_matches(group).then(Vec::new));
                if let Some(highlights) = highlights {
                    group.agents.push((agent, highlights));
                }
            }
            None => {
                if let Some(highlights) = matches_filter(agent, inputs, context) {
                    unplaced.push((agent, highlights));
                }
            }
        }
    }
    let visible = groups
        .into_values()
        .filter(|group| name_matches(group) || !group.agents.is_empty())
        .collect::<Vec<_>>();
    match inputs.grouping {
        SidebarGrouping::Labels => label_sections(visible, unplaced, inputs),
        _ => project_sections(visible, unplaced, inputs),
    }
}

/// An agent state worth the user's eye, shown by a sidebar row's border and an agent tab's
/// underline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentAlert {
    NeedsInput,
    Failed,
    Running,
    Unread,
}

impl AgentAlert {
    pub(crate) fn for_bucket(bucket: AgentBucket) -> Option<Self> {
        match bucket {
            AgentBucket::NeedsInput => Some(Self::NeedsInput),
            AgentBucket::Failed => Some(Self::Failed),
            AgentBucket::Running => Some(Self::Running),
            AgentBucket::Attention => Some(Self::Unread),
            AgentBucket::Done => None,
        }
    }

    fn dot_color(self) -> Color {
        match self {
            Self::NeedsInput => Color::Warning,
            Self::Failed => Color::Error,
            Self::Running => Color::Muted,
            Self::Unread => Color::Accent,
        }
    }

    pub(crate) fn border_color(self) -> Color {
        match self {
            Self::NeedsInput => Color::Warning,
            Self::Failed => Color::Error,
            Self::Running | Self::Unread => Color::Accent,
        }
    }

    /// How long one pulse of a tab's underline, or one sweep of a row's activity line, takes.
    /// Waiting for input moves faster than running, so the rows that need the user stand out;
    /// finished and failed rows hold still.
    pub(crate) fn pulse(self) -> Option<Duration> {
        match self {
            Self::NeedsInput => Some(Duration::from_secs(1)),
            Self::Running => Some(Duration::from_secs(2)),
            Self::Failed | Self::Unread => None,
        }
    }
}

/// Where a sidebar row's agent works and when it last changed, shown under its title.
struct RowDetails {
    /// The host's profile name, shown only while the sidebar lists several hosts.
    host: Option<String>,
    project: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    timestamp: Option<String>,
    /// Shown in place of the timestamp, such as "No agents" for an empty workspace.
    note: Option<String>,
}

impl RowDetails {
    /// The details as one line of small parts after `icon`; when the sidebar is narrow, the
    /// project, host, worktree and branch names give way and end in an ellipsis, while the time
    /// and note keep their width.
    fn render(self, icon: AnyElement) -> AnyElement {
        let separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(Color::Placeholder)
                .into_any_element()
        };
        let muted = |text: String| {
            div()
                .flex_none()
                .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        };
        let shrinking = |text: String| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate()
                .into_any_element()
        };
        let checkout = (self.worktree.is_some() || self.branch.is_some()).then(|| {
            h_flex()
                .min_w_0()
                .max_w_full()
                .overflow_hidden()
                .gap_0p5()
                .child(
                    Icon::new(if self.worktree.is_some() {
                        IconName::GitWorktree
                    } else {
                        IconName::GitBranch
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .when_some(self.worktree.clone(), |this, worktree| {
                    this.child(div().min_w_0().child(shrinking(worktree)))
                })
                .when(self.worktree.is_some() && self.branch.is_some(), |this| {
                    this.child(
                        Label::new("/")
                            .size(LabelSize::Small)
                            .color(Color::Placeholder),
                    )
                })
                .when_some(self.branch.clone(), |this, branch| {
                    this.child(div().min_w_0().child(shrinking(branch)))
                })
                .into_any_element()
        });
        let joined = |icon: Option<AnyElement>, parts: Vec<AnyElement>| {
            let mut line = h_flex()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .gap_x_1()
                .items_center()
                .children(icon);
            for (index, part) in parts.into_iter().enumerate() {
                if index > 0 {
                    line = line.child(separator());
                }
                line = line.child(part);
            }
            line
        };
        let host = self.host.map(|host| {
            h_flex()
                .min_w_0()
                .gap_0p5()
                .child(
                    Icon::new(IconName::Server)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().min_w_0().child(shrinking(host)))
                .into_any_element()
        });
        let project = self
            .project
            .map(|project| div().min_w_0().child(shrinking(project)).into_any_element());
        let place = [project, host, checkout]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let change = [self.timestamp.map(muted), self.note.map(muted)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        joined(Some(icon), place.into_iter().chain(change).collect()).into_any_element()
    }
}

/// A workspace row's details, and whether the row is dimmed because the workspace has no agents.
fn workspace_row_details(
    workspace: &WorkspaceDescriptor,
    agent_count: usize,
    host: Option<String>,
    show_project: bool,
    timestamp: Option<String>,
) -> (RowDetails, bool) {
    let project = show_project.then(|| workspace.project_display_name.clone());
    if agent_count == 0 {
        let details = RowDetails {
            host,
            project,
            worktree: None,
            branch: None,
            timestamp: None,
            note: Some("No agents".into()),
        };
        return (details, true);
    }
    let is_worktree = workspace.is_worktree();
    let details = RowDetails {
        host,
        project,
        worktree: is_worktree
            .then(|| workspace.worktree_slug.clone())
            .flatten(),
        branch: workspace.current_branch.clone(),
        timestamp,
        note: None,
    };
    (details, false)
}

/// An agent's row is selected only while it is the focused agent and its chat is the active item,
/// so another screen in front, such as History, takes the highlight.
fn agent_row_selected(
    focused_agent: Option<&str>,
    active_tab_agent: Option<&str>,
    agent_id: &str,
) -> bool {
    focused_agent == Some(agent_id) && active_tab_agent == Some(agent_id)
}

/// One sidebar row, for an agent or a workspace: a title that wraps onto a second line, and under
/// it the row's icon leading its details, so the title gets the row's full width.
struct SidebarRow {
    id: gpui::ElementId,
    icon: AnyElement,
    title: SharedString,
    highlight_positions: Vec<usize>,
    title_generating: bool,
    details: RowDetails,
    trailing: Vec<AnyElement>,
    selected: bool,
    keyboard_selected: bool,
    muted: bool,
    /// The directory shown under the full title in the title's tooltip.
    path: SharedString,
}

impl SidebarRow {
    fn render(self, cx: &App) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
        let tooltip_title = self.title.clone();
        let path = self.path;
        let title_color = if self.muted {
            Color::Muted
        } else {
            Color::Default
        };
        let title = if self.title_generating {
            Label::new(self.title)
                .color(Color::Muted)
                .with_animation(
                    "paseo-row-title-generating",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else if self.highlight_positions.is_empty() {
            Label::new(self.title)
                .color(title_color)
                .weight(gpui::FontWeight::MEDIUM)
                .into_any_element()
        } else {
            HighlightedLabel::new(self.title, self.highlight_positions)
                .color(title_color)
                .weight(gpui::FontWeight::MEDIUM)
                .into_any_element()
        };
        let details = self.details.render(self.icon);
        h_flex()
            .id(self.id)
            .w_full()
            .min_h(px(28.))
            .py_1()
            .px_3()
            .gap_2()
            .items_start()
            .rounded_md()
            .cursor_pointer()
            .border_1()
            .when(self.selected, |this| crate::stream::raised_card(this, cx))
            .border_color(if self.keyboard_selected {
                colors.panel_focused_border
            } else if self.selected {
                colors.border
            } else {
                gpui::transparent_black()
            })
            .when(!self.selected && !self.keyboard_selected, |this| {
                this.hover(|style| {
                    style
                        .bg(colors.ghost_element_hover)
                        .border_color(colors.border_variant)
                })
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    // Two lines, then an ellipsis, so a long title reads without taking over
                    // the list.
                    // The tooltip sits on the title rather than the row, because the row is the
                    // right-click menu's trigger and a row tooltip would stay over the open menu.
                    .child(
                        div()
                            .id("paseo-row-title")
                            .tooltip(move |_window, cx| {
                                Tooltip::with_meta(tooltip_title.clone(), None, path.clone(), cx)
                            })
                            .line_clamp(crate::PaseoSettings::get_global(cx).sidebar.title_lines)
                            .text_ellipsis()
                            .child(title),
                    )
                    .child(details),
            )
            .children(self.trailing)
    }
}

/// Underlines a sidebar row with a thin line showing `alert`: a segment sweeping across it while
/// the agent runs or waits, a still line when it finished unread or failed. The line overlays the
/// row's bottom edge so rows don't shift when an agent's state changes.
fn alert_line(
    row: AnyElement,
    alert: Option<AgentAlert>,
    id: impl Into<gpui::ElementId>,
    cx: &App,
) -> AnyElement {
    let frame = div().relative().py_0p5().child(row);
    let Some(alert) = alert else {
        return frame.into_any_element();
    };
    let color = alert.border_color().color(cx);
    let track = div()
        .absolute()
        .bottom_0p5()
        .left_3()
        .right_3()
        .h(px(2.))
        .rounded_full()
        .overflow_hidden();
    let sweep = alert
        .pulse()
        .filter(|_| crate::PaseoSettings::get_global(cx).sidebar.animate_status);
    let line = match sweep {
        Some(sweep) => track
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(relative(SWEEP_WIDTH))
                    .rounded_full()
                    .bg(color)
                    .with_animation(
                        id,
                        Animation::new(sweep).repeat().with_easing(ease_in_out),
                        |segment, delta| {
                            segment.left(relative(delta * (1. + SWEEP_WIDTH) - SWEEP_WIDTH))
                        },
                    ),
            )
            .into_any_element(),
        None => track.bg(color.opacity(0.6)).into_any_element(),
    };
    frame.child(line).into_any_element()
}

/// The share of the activity line that the sweeping segment covers.
const SWEEP_WIDTH: f32 = 0.3;

/// Colors `element` with `paint` in `alert`'s color, pulsing for alerts that pulse, and paints it
/// clear when there is no alert.
pub(crate) fn pulse_in_alert_color(
    element: gpui::Div,
    alert: Option<AgentAlert>,
    id: impl Into<gpui::ElementId>,
    paint: impl Fn(gpui::Div, gpui::Hsla) -> gpui::Div + 'static,
    cx: &App,
) -> AnyElement {
    let Some(alert) = alert else {
        return paint(element, gpui::transparent_black()).into_any_element();
    };
    let color = alert.border_color().color(cx);
    let pulse = alert
        .pulse()
        .filter(|_| crate::PaseoSettings::get_global(cx).sidebar.animate_status);
    match pulse {
        Some(pulse) => element
            .with_animation(
                id,
                Animation::new(pulse)
                    .repeat()
                    .with_easing(pulsating_between(0.15, 1.0)),
                move |element, delta| paint(element, color.opacity(delta)),
            )
            .into_any_element(),
        None => paint(element, color).into_any_element(),
    }
}

fn push_workspace(entries: &mut Vec<SidebarEntry>, group: &WorkspaceGroup, inputs: &SidebarInputs) {
    let collapsed = inputs
        .collapsed
        .contains(&workspace_collapse_key(&group.workspace.id))
        && inputs.filter.is_empty();
    let single_agent = match group.agents.as_slice() {
        [(agent, _)] => Some(agent.id.clone()),
        _ => None,
    };
    let merged = single_agent.is_some();
    entries.push(SidebarEntry::Workspace {
        workspace_id: group.workspace.id.clone(),
        collapsed,
        agent_count: group.agents.len(),
        highlight_positions: group.highlight_positions.clone(),
        single_agent,
    });
    if !collapsed && !merged {
        entries.extend(group.agents.iter().map(|(agent, highlight_positions)| {
            SidebarEntry::Agent {
                agent_id: agent.id.clone(),
                nested: true,
                highlight_positions: highlight_positions.clone(),
            }
        }));
    }
}

fn opens_agent(entry: &SidebarEntry) -> bool {
    matches!(
        entry,
        SidebarEntry::Agent { .. }
            | SidebarEntry::Workspace {
                single_agent: Some(_),
                ..
            }
    )
}

fn push_unplaced_agents(entries: &mut Vec<SidebarEntry>, agents: &[(&AgentSummary, Vec<usize>)]) {
    entries.extend(
        agents
            .iter()
            .map(|(agent, highlight_positions)| SidebarEntry::Agent {
                agent_id: agent.id.clone(),
                nested: false,
                highlight_positions: highlight_positions.clone(),
            }),
    );
}

/// A project section: its workspaces and the agents not placed in any, most recent first.
struct ProjectSection<'a> {
    key: String,
    label: String,
    directory: Option<PathBuf>,
    project_id: Option<String>,
    workspaces: Vec<WorkspaceGroup<'a>>,
    agents: Vec<(&'a AgentSummary, Vec<usize>)>,
    activity: Option<DateTime<Utc>>,
}

/// Project sections in the order they were first named, found by key.
#[derive(Default)]
struct ProjectSections<'a> {
    sections: Vec<ProjectSection<'a>>,
    positions: HashMap<String, usize>,
}

impl<'a> ProjectSections<'a> {
    /// The section with `key`, added with the rest of the details when it is new.
    fn section(
        &mut self,
        key: String,
        label: String,
        directory: Option<PathBuf>,
        project_id: Option<String>,
    ) -> Option<&mut ProjectSection<'a>> {
        let position = match self.positions.get(&key) {
            Some(position) => *position,
            None => {
                let position = self.sections.len();
                self.positions.insert(key.clone(), position);
                self.sections.push(ProjectSection {
                    key,
                    label,
                    directory,
                    project_id,
                    workspaces: Vec::new(),
                    agents: Vec::new(),
                    activity: None,
                });
                position
            }
        };
        self.sections.get_mut(position)
    }
}

fn project_sections(
    mut groups: Vec<WorkspaceGroup>,
    unplaced: Vec<(&AgentSummary, Vec<usize>)>,
    inputs: &SidebarInputs,
) -> Vec<SidebarEntry> {
    groups.sort_by_key(|group| std::cmp::Reverse(group.activity));
    let (pinned, groups): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .partition(|group| group.workspace.pinned_at.is_some());
    let mut sections = ProjectSections::default();
    for group in groups {
        let project = inputs
            .projects
            .get(group.workspace.project_id.as_str())
            .copied();
        let section = sections.section(
            format!("project:{}", group.workspace.project_id),
            project
                .map(project_label)
                .unwrap_or_else(|| group.workspace.project_display_name.clone()),
            Some(group.workspace.project_root_path.clone()),
            Some(group.workspace.project_id.clone()),
        );
        if let Some(section) = section {
            section.activity = section.activity.max(group.activity);
            section.workspaces.push(group);
        }
    }
    // The first project by ID for each root, as a scan of the projects in order finds.
    let mut projects_by_root: HashMap<&std::path::Path, &ProjectDescriptor> = HashMap::new();
    for project in inputs.projects.values() {
        projects_by_root
            .entry(project.root_path.as_path())
            .or_insert(project);
    }
    for (agent, highlights) in unplaced {
        let directory = agent_project_directory(agent);
        let project = directory
            .as_deref()
            .and_then(|directory| projects_by_root.get(directory));
        let section = match project {
            Some(project) => sections.section(
                format!("project:{}", project.id),
                project_label(project),
                Some(project.root_path.clone()),
                Some(project.id.clone()),
            ),
            None => sections.section(
                format!("directory:{}", agent_project_key(agent)),
                agent_project_name(agent),
                agent.directory.clone(),
                None,
            ),
        };
        if let Some(section) = section {
            section.activity = section.activity.max(agent_updated_at(agent));
            section.agents.push((agent, highlights));
        }
    }
    if inputs.filter.is_empty() {
        let listed_projects = sections
            .sections
            .iter()
            .filter_map(|section| section.project_id.as_deref())
            .chain(
                pinned
                    .iter()
                    .map(|group| group.workspace.project_id.as_str()),
            )
            .collect::<HashSet<_>>();
        let mut empty = inputs
            .projects
            .values()
            .filter(|project| !listed_projects.contains(project.id.as_str()))
            .collect::<Vec<_>>();
        empty.sort_by_cached_key(|project| project_label(project).to_lowercase());
        for project in empty {
            sections.section(
                format!("project:{}", project.id),
                project_label(project),
                Some(project.root_path.clone()),
                Some(project.id.clone()),
            );
        }
    }
    let mut sections = sections.sections;
    // Sections without activity keep their order after the active ones.
    sections.sort_by_key(|section| std::cmp::Reverse(section.activity));

    let mut entries = Vec::new();
    if !pinned.is_empty() {
        let key = "pinned".to_owned();
        let collapsed = inputs.collapsed.contains(&key) && inputs.filter.is_empty();
        entries.push(SidebarEntry::Header {
            key,
            label: "Pinned".into(),
            count: pinned.len(),
            collapsed,
            directory: None,
            project_id: None,
            color: None,
        });
        if !collapsed {
            for group in &pinned {
                push_workspace(&mut entries, group, inputs);
            }
        }
    }
    for section in sections {
        let collapsed = inputs.collapsed.contains(&section.key) && inputs.filter.is_empty();
        entries.push(SidebarEntry::Header {
            count: section.workspaces.len() + section.agents.len(),
            key: section.key,
            label: section.label,
            collapsed,
            directory: section.directory,
            project_id: section.project_id,
            color: None,
        });
        if !collapsed {
            for group in &section.workspaces {
                push_workspace(&mut entries, group, inputs);
            }
            push_unplaced_agents(&mut entries, &section.agents);
        }
    }
    entries
}

fn label_sections(
    mut groups: Vec<WorkspaceGroup>,
    unplaced: Vec<(&AgentSummary, Vec<usize>)>,
    inputs: &SidebarInputs,
) -> Vec<SidebarEntry> {
    groups.sort_by_key(|group| std::cmp::Reverse(group.activity));
    let mut entries = Vec::new();
    let mut push_section = |key: String,
                            label: String,
                            color: Option<String>,
                            members: Vec<&WorkspaceGroup>,
                            agents: &[(&AgentSummary, Vec<usize>)]| {
        if members.is_empty() && agents.is_empty() {
            return;
        }
        let collapsed = inputs.collapsed.contains(&key) && inputs.filter.is_empty();
        entries.push(SidebarEntry::Header {
            key,
            label,
            count: members.len() + agents.len(),
            collapsed,
            directory: None,
            project_id: None,
            color,
        });
        if !collapsed {
            for group in members {
                push_workspace(&mut entries, group, inputs);
            }
            push_unplaced_agents(&mut entries, agents);
        }
    };
    for label in inputs.labels {
        let members = groups
            .iter()
            .filter(|group| group.workspace.labels.contains(&label.name))
            .collect::<Vec<_>>();
        push_section(
            format!("label:{}", label.name),
            label.name.clone(),
            Some(label.color.clone()),
            members,
            &[],
        );
    }
    let unlabeled = groups
        .iter()
        .filter(|group| {
            !group
                .workspace
                .labels
                .iter()
                .any(|name| inputs.labels.iter().any(|label| &label.name == name))
        })
        .collect::<Vec<_>>();
    push_section(
        "label:".into(),
        "No label".into(),
        None,
        unlabeled,
        &unplaced,
    );
    entries
}

/// What a number key opens: an agent, or a workspace with several agents.
#[derive(Debug, Clone, PartialEq)]
enum NumberKeyTarget {
    Agent(String),
    Workspace(String),
}

/// The rows Ctrl-1…9 open, top to bottom, as Paseo numbers its sidebar: each workspace row, and
/// each agent outside a workspace, but not the agents nested under a workspace row.
fn number_key_targets(entries: &[SidebarEntry]) -> Vec<NumberKeyTarget> {
    let mut targets = Vec::new();
    for entry in entries {
        let target = match entry {
            SidebarEntry::Workspace {
                single_agent: Some(agent_id),
                ..
            } => NumberKeyTarget::Agent(agent_id.clone()),
            SidebarEntry::Workspace { workspace_id, .. } => {
                NumberKeyTarget::Workspace(workspace_id.clone())
            }
            SidebarEntry::Agent {
                agent_id,
                nested: false,
                ..
            } => NumberKeyTarget::Agent(agent_id.clone()),
            _ => continue,
        };
        // Grouped by label, a workspace can be listed under several labels.
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

fn agent_order(entries: &[SidebarEntry]) -> Vec<String> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter_map(|entry| match entry {
            SidebarEntry::Agent { agent_id, .. }
            | SidebarEntry::Workspace {
                single_agent: Some(agent_id),
                ..
            } => seen.insert(agent_id.as_str()).then(|| agent_id.clone()),
            _ => None,
        })
        .collect()
}

/// The rows that differ between `old` and `new`, as the range of `old` they replace and how many
/// rows of `new` take its place, or `None` when nothing changed. Only these are measured again,
/// as Zed's own sidebar does.
fn changed_rows(old: &[SidebarEntry], new: &[SidebarEntry]) -> Option<(Range<usize>, usize)> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

fn same_entry(first: &SidebarEntry, second: &SidebarEntry) -> bool {
    match (first, second) {
        (SidebarEntry::Header { key: first, .. }, SidebarEntry::Header { key: second, .. }) => {
            first == second
        }
        (
            SidebarEntry::Agent {
                agent_id: first, ..
            },
            SidebarEntry::Agent {
                agent_id: second, ..
            },
        ) => first == second,
        (
            SidebarEntry::Workspace {
                workspace_id: first,
                ..
            },
            SidebarEntry::Workspace {
                workspace_id: second,
                ..
            },
        ) => first == second,
        _ => false,
    }
}

/// How the sidebar is set up, shared by every project's sidebar. Each project in a window has
/// its own panel, so without this a project switch shows a sidebar still set the old way.
#[derive(Clone, Default)]
struct SharedSidebarView {
    grouping: SidebarGrouping,
    collapsed: HashSet<String>,
    /// Hosts the sidebar lists by profile name, like Paseo's host filter; empty lists all.
    hosts: BTreeSet<String>,
}

impl Global for SharedSidebarView {}

pub struct PaseoPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    filter: Entity<Editor>,
    position: DockPosition,
    grouping: SidebarGrouping,
    collapsed: HashSet<String>,
    host_filter: BTreeSet<String>,
    selected: Option<usize>,
    entries: Vec<SidebarEntry>,
    /// The list's items: [`LEADING_ROWS`] for host problems and the empty state, then `entries`.
    /// Only rows in view are built.
    list_state: ListState,
    /// What every row reads this frame, set by render before the list builds its rows.
    row_context: Option<RowContext>,
    pointer_inside: bool,
    refresh_pending: bool,
    /// A host change isn't applied to the rows yet. Rows are rebuilt when the panel is next
    /// drawn, so a panel that isn't shown does no work.
    stale: bool,
    /// One of those changes came while the panel was on screen, so its rows flash; changes made
    /// while it was hidden show without one.
    changed_while_shown: bool,
    motion: RowMotion,
    row_hosts: RowHosts,
    _subscriptions: Vec<Subscription>,
}

/// What rows show that would take a scan of every agent per row: each row's host, and each
/// agent's title and bucket. Rows re-render every frame while a status pulses, so this is
/// gathered on every store change instead, including while the pointer holds back a reorder, so
/// rows stay live.
#[derive(Default)]
struct RowHosts {
    agents: HashMap<String, AgentRow>,
    workspaces: HashMap<String, WorkspaceRow>,
    projects: HashMap<String, Entity<PaseoStore>>,
    /// Each host's name, only while more than one host is configured; with one, every row would
    /// repeat the same name.
    names: HashMap<EntityId, String>,
}

struct AgentRow {
    store: Entity<PaseoStore>,
    title: SharedString,
    bucket: AgentBucket,
    path: SharedString,
}

struct WorkspaceRow {
    store: Entity<PaseoStore>,
    /// Whether any of the workspace's agents finished and isn't read yet.
    unread: bool,
    motion_key: String,
    path: SharedString,
}

impl RowHosts {
    /// Gathers the rows of `stores`. `previous` keeps the rows of agents and workspaces that
    /// left, for rows that stay listed until the pointer leaves.
    fn new(
        stores: &[Entity<PaseoStore>],
        configured: Vec<(String, Entity<PaseoStore>)>,
        previous: Option<RowHosts>,
        cx: &App,
    ) -> Self {
        let mut row_hosts = Self::default();
        for store in stores {
            let store_state = store.read(cx);
            let state = &store_state.state;
            let pending = crate::attention::pending_permission_agents(store_state);
            let titles = WorkspaceAgentCounts::new(state.agents());
            let mut unread_workspaces = HashSet::new();
            for agent in state.agents() {
                if agent_requires_attention(agent)
                    && let Some(workspace_id) = agent_workspace_id(agent)
                {
                    unread_workspaces.insert(workspace_id);
                }
                if row_hosts.agents.contains_key(&agent.id) {
                    continue;
                }
                row_hosts.agents.insert(
                    agent.id.clone(),
                    AgentRow {
                        store: store.clone(),
                        title: titles.display_title(&state.workspaces, agent).into(),
                        bucket: agent_bucket(agent, pending.contains(agent.id.as_str())),
                        path: agent
                            .directory
                            .as_ref()
                            .map(|directory| directory.display().to_string())
                            .unwrap_or_default()
                            .into(),
                    },
                );
            }
            for (workspace_id, workspace) in &state.workspaces {
                row_hosts
                    .workspaces
                    .entry(workspace_id.clone())
                    .or_insert_with(|| WorkspaceRow {
                        store: store.clone(),
                        unread: unread_workspaces.contains(workspace_id.as_str()),
                        motion_key: workspace_motion_key(workspace_id),
                        path: workspace.directory.display().to_string().into(),
                    });
            }
            for project_id in state.projects.keys() {
                row_hosts
                    .projects
                    .entry(project_id.clone())
                    .or_insert_with(|| store.clone());
            }
        }
        if let Some(previous) = previous {
            for (agent_id, row) in previous.agents {
                row_hosts.agents.entry(agent_id).or_insert(row);
            }
            for (workspace_id, row) in previous.workspaces {
                row_hosts.workspaces.entry(workspace_id).or_insert(row);
            }
            for (project_id, store) in previous.projects {
                row_hosts.projects.entry(project_id).or_insert(store);
            }
        }
        if configured.len() > 1 {
            row_hosts.names = configured
                .into_iter()
                .map(|(name, store)| (store.entity_id(), name))
                .collect();
        }
        row_hosts
    }

    fn name(&self, store: &Entity<PaseoStore>) -> Option<String> {
        self.names.get(&store.entity_id()).cloned()
    }

    /// An agent's row and summary. An agent that left its store's list, kept while the pointer
    /// holds back a reorder, is found among the store's archived agents if they have loaded.
    fn agent<'a>(&self, agent_id: &str, cx: &'a App) -> Option<(&AgentRow, &'a AgentSummary)> {
        let row = self.agents.get(agent_id)?;
        let agent = row.store.read(cx).agent(agent_id)?;
        Some((row, agent))
    }
}

/// A workspace entry's fields, as its row reads them.
struct WorkspaceEntry<'a> {
    workspace_id: &'a str,
    collapsed: bool,
    agent_count: usize,
    highlight_positions: &'a [usize],
    single_agent: Option<&'a str>,
}

/// What every row reads, worked out once per frame rather than once per row.
struct RowContext {
    focused_agent: Option<String>,
    active_tab_agent: Option<String>,
    panel_focused: bool,
    animate: bool,
    now: DateTime<Utc>,
}

/// More rows than this appearing at once is a load, so they appear without easing in.
const MAX_ANIMATED_APPEARANCES: usize = 3;

/// How long a row's flash takes to fade after its agent changes state.
const STATE_FLASH: Duration = Duration::from_millis(900);
/// The list item before the rows, holding host problems and the empty state.
const LEADING_ROWS: usize = 1;
/// How far past the visible rows the list measures rows on every layout.
const LIST_OVERDRAW: Pixels = px(1000.);

/// When agents appeared and changed state while the sidebar was open, so their rows ease in and
/// flash once.
#[derive(Default)]
struct RowMotion {
    /// Each agent's last seen bucket; `None` until the first look, which only records.
    buckets: Option<HashMap<String, AgentBucket>>,
    appeared_at: HashMap<String, Instant>,
    changed_at: HashMap<String, (Instant, AgentAlert)>,
}

impl RowMotion {
    fn observe(&mut self, agents: impl IntoIterator<Item = (String, AgentBucket)>, now: Instant) {
        let current = agents.into_iter().collect::<HashMap<_, _>>();
        // An empty or unseen list is the host still loading, and a burst of new rows is a load
        // too (a reconnect or host switch), not agents appearing one by one.
        let previous = self
            .buckets
            .as_ref()
            .filter(|previous| !previous.is_empty());
        let appearing = previous.map_or(0, |previous| {
            current
                .keys()
                .filter(|key| !previous.contains_key(*key))
                .count()
        });
        if let Some(previous) = previous {
            for (agent_id, bucket) in &current {
                match previous.get(agent_id) {
                    None if appearing > MAX_ANIMATED_APPEARANCES => {}
                    None => {
                        self.appeared_at.insert(agent_id.clone(), now);
                    }
                    Some(previous) if previous != bucket => match AgentAlert::for_bucket(*bucket) {
                        Some(alert) => {
                            self.changed_at.insert(agent_id.clone(), (now, alert));
                        }
                        None => {
                            self.changed_at.remove(agent_id);
                        }
                    },
                    Some(_) => {}
                }
            }
        }
        self.appeared_at
            .retain(|_, appeared_at| now.duration_since(*appeared_at) < ENTRANCE * 2);
        self.changed_at
            .retain(|_, (changed_at, _)| now.duration_since(*changed_at) < STATE_FLASH * 2);
        self.buckets = Some(current);
    }

    /// Takes the agents' buckets as seen, without easing in or flashing any row.
    fn record(&mut self, agents: impl IntoIterator<Item = (String, AgentBucket)>) {
        self.buckets = Some(agents.into_iter().collect());
    }
}

/// A workspace row's key in [`RowMotion`], apart from agent ids; it moves with its most urgent
/// agent.
fn workspace_motion_key(workspace_id: &str) -> String {
    format!("workspace:{workspace_id}")
}

/// Eases a sidebar row in when its agent just appeared, and washes it in its new state's colour,
/// fading out, when the agent just changed state.
fn row_motion(
    row: AnyElement,
    key: &str,
    motion: &RowMotion,
    animate: bool,
    cx: &App,
) -> AnyElement {
    if !animate {
        return row;
    }
    let flash = motion
        .changed_at
        .get(key)
        .filter(|(changed_at, _)| changed_at.elapsed() < STATE_FLASH * 2)
        .map(|(_, alert)| alert.border_color().color(cx));
    let frame = div()
        .relative()
        .child(row)
        .when_some(flash, |frame, color| {
            frame.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_md()
                    .bg(color)
                    .with_animation(
                        SharedString::from(format!("paseo-row-flash-{key}")),
                        Animation::new(STATE_FLASH).with_easing(ease_in_out),
                        |wash, delta| wash.opacity(0.22 * (1. - delta)),
                    ),
            )
        });
    match motion.appeared_at.get(key).copied() {
        Some(appeared_at) => fade_in_since(
            frame,
            SharedString::from(format!("paseo-row-appear-{key}")),
            Some(appeared_at),
            px(0.),
        ),
        None => frame.into_any_element(),
    }
}

impl PaseoPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        Task::ready(workspace.clone().update_in(&mut cx, |_, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        }))
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let registry = hosts::registry(cx);
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter agents", window, cx);
            editor
        });
        if !cx.has_global::<SharedSidebarView>() {
            // The setting wins; without one, the last choice saved before it existed.
            let grouping = crate::PaseoSettings::get_global(cx)
                .sidebar
                .grouping
                .map(SidebarGrouping::from)
                .or_else(|| {
                    KeyValueStore::global(cx)
                        .read_kvp(GROUPING_KEY)
                        .log_err()
                        .flatten()
                        .map(|value| SidebarGrouping::from_stored(&value))
                })
                .unwrap_or_default();
            let hosts = KeyValueStore::global(cx)
                .read_kvp(HOST_FILTER_KEY)
                .log_err()
                .flatten()
                .and_then(|value| serde_json::from_str::<BTreeSet<String>>(&value).log_err())
                .unwrap_or_default();
            cx.set_global(SharedSidebarView {
                grouping,
                hosts,
                ..SharedSidebarView::default()
            });
        }
        let shared = cx.global::<SharedSidebarView>().clone();
        let mut subscriptions = vec![
            cx.observe_global::<SharedSidebarView>(|panel: &mut Self, cx| {
                panel.shared_view_changed(cx)
            }),
            // The settings UI can change the grouping too.
            cx.observe_global::<settings::SettingsStore>(|panel: &mut Self, cx| {
                let grouping = crate::PaseoSettings::get_global(cx)
                    .sidebar
                    .grouping
                    .map(SidebarGrouping::from);
                if let Some(grouping) = grouping.filter(|grouping| *grouping != panel.grouping) {
                    cx.update_global::<SharedSidebarView, _>(|shared, _| {
                        shared.grouping = grouping;
                        shared.collapsed.clear();
                    });
                }
                cx.notify();
            }),
            cx.observe(&registry, |panel: &mut Self, _, cx| panel.store_changed(cx)),
            cx.subscribe(&registry, |_: &mut Self, _, event: &HostsEvent, cx| {
                if matches!(event, HostsEvent::FocusChanged) {
                    cx.notify();
                }
            }),
            cx.subscribe(
                &filter,
                |panel: &mut Self, _, event: &editor::EditorEvent, cx| {
                    if matches!(event, editor::EditorEvent::BufferEdited) {
                        panel.selected = None;
                        panel.refresh_entries(cx);
                    }
                },
            ),
        ];
        // The History row is highlighted while History is the active tab.
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(
                cx.subscribe(&workspace, |_, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        cx.notify();
                    }
                }),
            );
        }
        let mut panel = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            filter,
            position: DockPosition::Left,
            grouping: shared.grouping,
            collapsed: shared.collapsed,
            host_filter: shared.hosts,
            selected: None,
            entries: Vec::new(),
            list_state: ListState::new(LEADING_ROWS, ListAlignment::Top, LIST_OVERDRAW),
            row_context: None,
            pointer_inside: false,
            refresh_pending: false,
            stale: false,
            changed_while_shown: false,
            motion: RowMotion::default(),
            row_hosts: RowHosts::default(),
            _subscriptions: subscriptions,
        };
        panel.observe_motion(false, cx);
        panel.refresh_entries(cx);
        panel
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.stale = true;
        self.changed_while_shown |= cx.in_last_frame(cx.entity_id());
        cx.notify();
    }

    /// Brings the rows up to date with host changes, without a notify, so render can call it.
    /// Rows moving under the pointer cause misclicks, so reorders wait until the pointer leaves;
    /// what the rows show is gathered now, so their content stays live.
    fn apply_store_changes(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.stale) {
            return;
        }
        let shown = std::mem::take(&mut self.changed_while_shown);
        self.observe_motion(!shown, cx);
        if self.pointer_inside {
            self.refresh_pending = true;
            let stores = self.shown_stores(cx);
            let previous = std::mem::take(&mut self.row_hosts);
            self.row_hosts =
                RowHosts::new(&stores, hosts::configured_hosts(cx), Some(previous), cx);
        } else {
            self.rebuild_entries(cx);
        }
    }

    /// The rows, first brought up to date, for actions that read them while the panel may be
    /// hidden.
    pub(crate) fn current_entries(&mut self, cx: &mut Context<Self>) -> &[SidebarEntry] {
        self.apply_store_changes(cx);
        &self.entries
    }

    /// The hosts the filter lists, in settings order. A filter naming no listed host lists every
    /// host, so the sidebar is never empty because of a host that was removed.
    fn shown_stores(&self, cx: &App) -> Vec<Entity<PaseoStore>> {
        let registry = hosts::registry(cx);
        let registry = registry.read(cx);
        let listed_names = registry
            .listed()
            .map(|host| host.name().to_owned())
            .collect::<BTreeSet<_>>();
        let shown = effective_host_filter(&self.host_filter, &listed_names);
        registry
            .listed()
            .filter(|host| shown.contains(host.name()))
            .map(|host| host.store.clone())
            .collect()
    }

    /// `quietly` takes changes made while the panel wasn't shown as seen, so they don't all
    /// flash at once when it is.
    fn observe_motion(&mut self, quietly: bool, cx: &mut Context<Self>) {
        let mut rows = HashMap::new();
        for store in self.shown_stores(cx) {
            let store = store.read(cx);
            for agent in store.state.agents() {
                let bucket = store.bucket(agent);
                rows.insert(agent.id.clone(), bucket);
                if let Some(workspace_id) = agent_workspace_id(agent) {
                    rows.entry(workspace_motion_key(workspace_id))
                        .and_modify(|workspace_bucket: &mut AgentBucket| {
                            *workspace_bucket = (*workspace_bucket).min(bucket)
                        })
                        .or_insert(bucket);
                }
            }
        }
        if quietly {
            self.motion.record(rows);
        } else {
            self.motion.observe(rows, Instant::now());
        }
    }

    fn shared_view_changed(&mut self, cx: &mut Context<Self>) {
        let shared = cx.global::<SharedSidebarView>().clone();
        self.grouping = shared.grouping;
        self.collapsed = shared.collapsed;
        self.host_filter = shared.hosts;
        self.stale = false;
        self.changed_while_shown = false;
        self.observe_motion(false, cx);
        self.refresh_entries(cx);
    }

    fn toggle_collapsed(&mut self, key: String, cx: &mut Context<Self>) {
        cx.update_global::<SharedSidebarView, _>(|shared, _| {
            if !shared.collapsed.remove(&key) {
                shared.collapsed.insert(key);
            }
        });
    }

    pub(crate) fn set_pointer_inside(&mut self, inside: bool, cx: &mut Context<Self>) {
        self.pointer_inside = inside;
        if !inside && self.refresh_pending {
            self.refresh_entries(cx);
        }
    }

    fn refresh_entries(&mut self, cx: &mut Context<Self>) {
        self.rebuild_entries(cx);
        cx.notify();
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        self.refresh_pending = false;
        let previously_selected = self
            .selected
            .and_then(|selected| self.entries.get(selected))
            .cloned();
        let filter = self.filter.read(cx).text(cx);
        let stores = self.shown_stores(cx);
        self.row_hosts = RowHosts::new(&stores, hosts::configured_hosts(cx), None, cx);
        let merged = merged_state(&stores, cx);
        let entries = build_entries(&SidebarInputs {
            agents: &merged.agents,
            workspaces: &merged.workspaces,
            projects: &merged.projects,
            labels: &merged.labels,
            pending_permission_agents: &merged.pending,
            grouping: self.grouping,
            collapsed: &self.collapsed,
            filter: filter.trim(),
        });
        if let Some((replaced, count)) = changed_rows(&self.entries, &entries) {
            self.list_state.splice(
                replaced.start + LEADING_ROWS..replaced.end + LEADING_ROWS,
                count,
            );
        }
        self.entries = entries;
        // Entries re-sort as agents update, so keep the selection on the same entry, not index.
        if let Some(previous) = previously_selected {
            self.selected = self
                .entries
                .iter()
                .position(|entry| same_entry(entry, &previous))
                .or_else(|| {
                    self.selected
                        .map(|selected| selected.min(self.entries.len().saturating_sub(1)))
                })
                .filter(|_| !self.entries.is_empty());
        }
    }

    fn toggle_group_by_status(
        &mut self,
        _: &ToggleGroupByStatus,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let grouping = if self.grouping == SidebarGrouping::Status {
            SidebarGrouping::Project
        } else {
            SidebarGrouping::Status
        };
        self.set_grouping(grouping, cx);
    }

    fn set_grouping(&mut self, grouping: SidebarGrouping, cx: &mut Context<Self>) {
        cx.update_global::<SharedSidebarView, _>(|shared, _| {
            shared.grouping = grouping;
            shared.collapsed.clear();
        });
        let value = grouping.stored().to_owned();
        let kvp = KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            kvp.write_kvp(GROUPING_KEY.to_owned(), value).await
        });
        if let Some(workspace) = self.workspace.upgrade() {
            let fs = workspace.read(cx).app_state().fs.clone();
            settings::update_settings_file(fs, cx, move |settings, _| {
                settings
                    .paseo
                    .get_or_insert_default()
                    .sidebar
                    .get_or_insert_default()
                    .grouping = Some(grouping.into());
            });
        }
    }

    fn focus_filter(
        &mut self,
        _: &FocusSidebarFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = self.filter.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            Some(selected) => (selected + 1) % self.entries.len(),
            None => self.entries.iter().position(opens_agent).unwrap_or(0),
        });
        self.reveal_selected(cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            Some(0) | None => self.entries.len() - 1,
            Some(selected) => selected - 1,
        });
        self.reveal_selected(cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.entries.is_empty() {
            self.selected = Some(0);
            self.reveal_selected(cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.entries.len().checked_sub(1) {
            self.selected = Some(last);
            self.reveal_selected(cx);
        }
    }

    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected {
            let index = selected + LEADING_ROWS;
            // The list measures only the rows around the visible ones, and revealing a row
            // scrolls by the heights of the rows before it. A row beyond those is a jump, made by
            // its index instead, which needs no heights.
            let viewport = self.list_state.viewport_bounds();
            let near = self.list_state.bounds_for_item(index).is_some_and(|row| {
                row.top() < viewport.bottom() + LIST_OVERDRAW
                    && row.bottom() > viewport.top() - LIST_OVERDRAW
            });
            if near {
                self.list_state.scroll_to_reveal_item(index);
            } else {
                self.list_state.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
            }
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .selected
            .or_else(|| self.entries.iter().position(opens_agent))
            .and_then(|selected| self.entries.get(selected).cloned());
        if let Some(entry) = entry {
            self.activate(&entry, true, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if !self.filter.read(cx).text(cx).is_empty() {
            self.filter
                .update(cx, |editor, cx| editor.set_text("", window, cx));
            return;
        }
        self.selected = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn activate(
        &mut self,
        entry: &SidebarEntry,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match entry {
            SidebarEntry::Header { key, .. } => self.toggle_collapsed(key.clone(), cx),
            SidebarEntry::Workspace {
                single_agent: Some(agent_id),
                ..
            } => {
                let agent_id = agent_id.clone();
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        open_agent(workspace, &agent_id, focus, window, cx);
                    });
                }
            }
            SidebarEntry::Workspace { workspace_id, .. } => {
                let workspace_id = workspace_id.clone();
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        crate::open_paseo_workspace_tabs(workspace, &workspace_id, window, cx);
                    });
                }
            }
            SidebarEntry::Agent { agent_id, .. } => {
                let agent_id = agent_id.clone();
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        open_agent(workspace, &agent_id, focus, window, cx);
                    });
                }
            }
        }
    }

    fn selected_agent(&self) -> Option<String> {
        match self
            .selected
            .and_then(|selected| self.entries.get(selected))
        {
            Some(SidebarEntry::Agent { agent_id, .. })
            | Some(SidebarEntry::Workspace {
                single_agent: Some(agent_id),
                ..
            }) => Some(agent_id.clone()),
            _ => None,
        }
    }

    fn archive_selected(&mut self, _: &ArchiveAgent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_agent()
            && let Some(store) = hosts::store_for_agent(&agent_id, cx)
        {
            store.update(cx, |store, cx| store.archive(&agent_id, cx));
        }
    }

    fn rename_selected(&mut self, _: &RenameAgent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_agent() {
            self.rename(agent_id, window, cx);
        }
    }

    fn rename(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace_tools::open_agent_rename(workspace, agent_id, window, cx);
            });
        }
    }

    fn copy_selected_id(&mut self, _: &CopyAgentId, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_agent() {
            cx.write_to_clipboard(ClipboardItem::new_string(agent_id));
        }
    }

    /// Starts a draft in `directory` on `host`, the default host when `None`.
    fn new_agent_in(
        &mut self,
        directory: Option<PathBuf>,
        host: Option<Entity<PaseoStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            let host = host.unwrap_or_else(|| hosts::default_store(cx));
            workspace.update(cx, |workspace, cx| match directory {
                Some(directory) => {
                    crate::open_draft_in_on(workspace, host, directory, window, cx)
                        .detach_and_log_err(cx);
                }
                None => {
                    crate::open_draft_on(workspace, host, None, window, cx);
                }
            });
        }
    }

    /// The hosts button: every host's connection at a glance, worst first, and a menu to
    /// reconnect one, pick the default host for new agents, or manage hosts.
    fn render_host_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let registry = hosts::registry(cx);
        let registry = registry.read(cx);
        let statuses = registry
            .hosts()
            .iter()
            .filter(|host| host.profile.is_some())
            .map(|host| (host.name().to_owned(), host.store.read(cx).status))
            .collect::<Vec<_>>();
        let label = match statuses.as_slice() {
            [] => "No host".to_owned(),
            [(name, _)] => name.clone(),
            hosts => format!("{} hosts", hosts.len()),
        };
        let worst = statuses
            .iter()
            .map(|(_, status)| *status)
            .max_by_key(|status| match status {
                ConnectionStatus::Connected => 0,
                ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => 1,
                ConnectionStatus::Disconnected => 2,
            })
            .unwrap_or(ConnectionStatus::Disconnected);
        let (dot, tooltip) = match worst {
            ConnectionStatus::Connected => (Color::Success, "Every host is connected"),
            ConnectionStatus::Connecting => (Color::Warning, "Connecting…"),
            ConnectionStatus::Reconnecting => (Color::Warning, "Reconnecting…"),
            ConnectionStatus::Disconnected => (Color::Error, "A host is disconnected"),
        };
        PopoverMenu::new("paseo-host-menu")
            .trigger_with_tooltip(
                ui::Button::new("paseo-host-button", label)
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .start_icon(
                        Icon::new(IconName::Circle)
                            .size(IconSize::XSmall)
                            .color(dot),
                    )
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
                Tooltip::text(tooltip),
            )
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let statuses = statuses.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    menu = menu.header("Hosts");
                    for (index, (name, status)) in statuses.iter().enumerate() {
                        let (state, color) = match status {
                            ConnectionStatus::Connected => ("connected", Color::Success),
                            ConnectionStatus::Connecting => ("connecting…", Color::Warning),
                            ConnectionStatus::Reconnecting => ("reconnecting…", Color::Warning),
                            ConnectionStatus::Disconnected => ("disconnected", Color::Error),
                        };
                        let host = name.clone();
                        let label = name.clone();
                        menu = menu.custom_entry(
                            move |_, _| {
                                h_flex()
                                    .id(("paseo-host-menu-entry", index))
                                    .w_full()
                                    .gap_2()
                                    .justify_between()
                                    .tooltip(Tooltip::text("Reconnect"))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(Indicator::dot().color(color))
                                            .child(Label::new(label.clone())),
                                    )
                                    .child(
                                        Label::new(state)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .into_any_element()
                            },
                            move |_, cx| hosts::reconnect_host(&host, cx),
                        );
                    }
                    let default_host = PaseoSettings::get_global(cx)
                        .active()
                        .map(|profile| profile.name.clone());
                    menu = menu.separator().header("Default for new agents");
                    for (name, _) in &statuses {
                        let host = name.clone();
                        menu = menu.toggleable_entry(
                            name.clone(),
                            default_host.as_deref() == Some(name.as_str()),
                            ui::IconPosition::End,
                            None,
                            move |_, cx| connection_picker::set_default_host(host.clone(), cx),
                        );
                    }
                    menu.separator()
                        .action("Manage Hosts…", ManageHosts.boxed_clone())
                        .action("Provider Usage", crate::OpenProviderUsage.boxed_clone())
                        .action("Daemon Status", crate::OpenDaemonStatus.boxed_clone())
                        .action("Reconnect All", Reconnect.boxed_clone())
                }))
            })
    }

    /// A row per host that needs the user: offline, refusing the password, or reaching a daemon
    /// another host already lists. Shown only with several hosts; one host's state is the
    /// sidebar's empty state.
    fn render_host_problems(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let registry = hosts::registry(cx);
        let registry = registry.read(cx);
        let configured = registry
            .hosts()
            .iter()
            .filter(|host| host.profile.is_some())
            .count();
        if configured < 2 {
            return Vec::new();
        }
        registry
            .hosts()
            .iter()
            .enumerate()
            .filter(|(_, host)| host.profile.is_some())
            .filter_map(|(index, host)| {
                let store = host.store.read(cx);
                let name = host.name().to_owned();
                let original = registry.duplicate_of(index);
                let (problem, detail) = if let Some(original) = original {
                    (
                        format!("same daemon as {}", original.name()),
                        "Its agents are listed once, under the first host.".to_owned(),
                    )
                } else if store.status == ConnectionStatus::Disconnected {
                    let detail = store
                        .state
                        .error
                        .clone()
                        .unwrap_or_else(|| "Not connected".to_owned());
                    let problem = if store.needs_password {
                        "needs a password"
                    } else {
                        "offline"
                    };
                    (problem.to_owned(), detail)
                } else {
                    return None;
                };
                let needs_password = store.needs_password;
                let duplicate = original.is_some();
                let action = if duplicate {
                    None
                } else if needs_password {
                    Some(
                        ui::Button::new(("paseo-host-password", index), "Password…")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(ManageHosts.boxed_clone(), cx)
                            })
                            .into_any_element(),
                    )
                } else {
                    let host = name.clone();
                    Some(
                        ui::Button::new(("paseo-host-retry", index), "Try again")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| hosts::reconnect_host(&host, cx))
                            .into_any_element(),
                    )
                };
                Some(
                    h_flex()
                        .id(("paseo-host-problem", index))
                        .px_2()
                        .py_1()
                        .gap_2()
                        .tooltip(Tooltip::text(detail))
                        .child(Icon::new(IconName::Server).size(IconSize::Small).color(
                            if duplicate {
                                Color::Muted
                            } else {
                                Color::Error
                            },
                        ))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(format!("{name} · {problem}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                        .children(action)
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let group_focus = self.focus_handle.clone();
        h_flex()
            .h(px(40.))
            .flex_none()
            .px_2p5()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(self.render_host_menu(cx))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("paseo-command-palette", IconName::MagnifyingGlass)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Command palette",
                                    &zed_actions::command_palette::Toggle,
                                    &focus,
                                    cx,
                                )
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    zed_actions::command_palette::Toggle.boxed_clone(),
                                    cx,
                                )
                            }),
                    )
                    .child(crate::attention::attention_bell(
                        "paseo-attention",
                        self.workspace.clone(),
                        hosts::activity(cx).attention,
                        cx,
                    ))
                    .child(self.render_grouping_menu(group_focus, cx)),
            )
    }

    fn render_grouping_menu(&self, focus: FocusHandle, cx: &Context<Self>) -> impl IntoElement {
        let grouping = self.grouping;
        let panel = cx.weak_entity();
        PopoverMenu::new("paseo-grouping-menu")
            .trigger_with_tooltip(
                IconButton::new(
                    "paseo-group-toggle",
                    match grouping {
                        SidebarGrouping::Project => IconName::Folder,
                        SidebarGrouping::Status => IconName::ListTree,
                        SidebarGrouping::Labels => IconName::Hash,
                    },
                )
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted),
                move |_window, cx| {
                    Tooltip::for_action_in("Group agents", &ToggleGroupByStatus, &focus, cx)
                },
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    menu = menu.header("Group by");
                    for (choice, label) in [
                        (SidebarGrouping::Project, "Project"),
                        (SidebarGrouping::Status, "Status"),
                        (SidebarGrouping::Labels, "Labels"),
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            label,
                            grouping == choice,
                            ui::IconPosition::End,
                            None,
                            move |_, cx| {
                                if let Err(error) =
                                    panel.update(cx, |panel, cx| panel.set_grouping(choice, cx))
                                {
                                    log::debug!("Paseo sidebar released: {error}");
                                }
                            },
                        );
                    }
                    let host_names = hosts::configured_hosts(cx)
                        .into_iter()
                        .map(|(name, _)| name)
                        .collect::<Vec<_>>();
                    if host_names.len() > 1 {
                        let shown_hosts = effective_host_filter(
                            &cx.global::<SharedSidebarView>().hosts,
                            &host_names.iter().cloned().collect(),
                        );
                        menu = menu.separator().header("Hosts");
                        for name in host_names {
                            let shown = shown_hosts.contains(&name);
                            menu = menu.toggleable_entry(
                                name.clone(),
                                shown,
                                ui::IconPosition::End,
                                None,
                                move |_, cx| toggle_host_filter(&name, cx),
                            );
                        }
                    }
                    menu.separator()
                        .action("Add Project…", crate::AddProject.boxed_clone())
                        .action(
                            "New Project Directory…",
                            crate::NewProjectDirectory.boxed_clone(),
                        )
                }))
            })
    }

    /// The agent whose chat is the workspace's active item, if one is.
    fn active_agent_tab_id(&self, cx: &App) -> Option<String> {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .and_then(|item| item.downcast::<crate::agent_view::AgentTab>())
            .and_then(|tab| tab.read(cx).agent_id(cx))
    }

    /// Paseo highlights its History row while History is the screen in front.
    fn history_open(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .is_some_and(|item| {
                item.downcast::<crate::history::PaseoHistoryView>()
                    .is_some()
            })
    }

    fn render_nav(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let focus = self.focus_handle.clone();
        v_flex()
            .px_2()
            .pt_2()
            .pb_2p5()
            .gap_0p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                ui::ListItem::new("paseo-new-workspace")
                    .height(rems_from_px(28_f32))
                    .rounded()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(NewAgentWorkspace.boxed_clone(), cx)
                    })
                    .start_slot(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("New workspace"))
                    .end_slot(
                        KeyBinding::for_action_in(&NewAgentWorkspace, &focus, cx)
                            .size(rems_from_px(11_f32)),
                    ),
            )
            .child(
                ui::ListItem::new("paseo-history")
                    .height(rems_from_px(28_f32))
                    .rounded()
                    .toggle_state(self.history_open(cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(crate::OpenHistory.boxed_clone(), cx)
                    })
                    .start_slot(
                        Icon::new(IconName::HistoryRerun)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("History")),
            )
            .child(
                h_flex()
                    .mt_1p5()
                    .h(px(30.))
                    .px_2p5()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if self.filter.focus_handle(cx).is_focused(window) {
                        colors.border_focused.opacity(0.6)
                    } else {
                        colors.border_variant
                    })
                    .bg(colors.editor_background)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(self.filter.clone()),
                    ),
            )
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &SidebarEntry,
        row: &RowContext,
        cx: &Context<Self>,
    ) -> AnyElement {
        let keyboard_selected = row.panel_focused && self.selected == Some(index);
        match entry {
            SidebarEntry::Header { .. } => {
                self.render_header_row(index, entry, keyboard_selected, cx)
            }
            SidebarEntry::Workspace {
                workspace_id,
                collapsed,
                agent_count,
                highlight_positions,
                single_agent,
            } => self.render_workspace(
                index,
                WorkspaceEntry {
                    workspace_id,
                    collapsed: *collapsed,
                    agent_count: *agent_count,
                    highlight_positions,
                    single_agent: single_agent.as_deref(),
                },
                keyboard_selected,
                row,
                cx,
            ),
            SidebarEntry::Agent {
                agent_id,
                nested,
                highlight_positions,
            } => {
                let agent_row = self.render_agent(
                    index,
                    agent_id,
                    *nested,
                    highlight_positions.clone(),
                    keyboard_selected,
                    row,
                    cx,
                );
                if *nested {
                    div().pl_4().child(agent_row).into_any_element()
                } else {
                    agent_row
                }
            }
        }
    }

    fn render_header_row(
        &self,
        index: usize,
        entry: &SidebarEntry,
        keyboard_selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let SidebarEntry::Header {
            key,
            label,
            count,
            collapsed,
            directory,
            color,
            project_id,
        } = entry
        else {
            return div().into_any_element();
        };
        let project_id = project_id.clone();
        let project_icon = project_id.as_ref().and_then(|project_id| {
            self.row_hosts
                .projects
                .get(project_id)?
                .read(cx)
                .project_icons
                .get(project_id)
                .and_then(|(_, image)| image.clone())
        });
        let group = format!("paseo-group-{index}");
        let empty_project = *count == 0 && directory.is_some();
        let label_color = color.as_deref().map(|color| label_color(color, cx));
        let entry = entry.clone();
        let directory = directory.clone();
        let header = ui::ListItem::new(("paseo-sidebar-header", index))
            .group_name(group.clone())
            .height(rems_from_px(28_f32))
            .rounded()
            .focused(keyboard_selected)
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.selected = Some(index);
                panel.activate(&entry, false, window, cx)
            }))
            .when_some(label_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            })
            .when_some(project_icon.clone(), |this, image| {
                this.child(gpui::img(image).size_3p5().flex_none())
            })
            .when(label_color.is_none() && project_icon.is_none(), |this| {
                this.child(
                    Icon::new(if key.starts_with("status:") {
                        IconName::Circle
                    } else if key == "pinned" {
                        IconName::Pin
                    } else if key.starts_with("label:") {
                        IconName::Hash
                    } else if *collapsed {
                        IconName::Folder
                    } else {
                        IconName::FolderOpen
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(label.clone())
                        .size(LabelSize::Small)
                        .color(if empty_project {
                            Color::Placeholder
                        } else {
                            Color::Muted
                        })
                        .truncate(),
                ),
            )
            .when(!empty_project, |this| {
                this.child(
                    Label::new(count.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
            })
            .when_some(directory, |this, directory| {
                let project_host = project_id
                    .as_deref()
                    .and_then(|project_id| self.row_hosts.projects.get(project_id))
                    .cloned();
                this.child(
                    div().visible_on_hover(group.clone()).child(
                        IconButton::new(("paseo-group-new", index), IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("New agent in this project"))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                cx.stop_propagation();
                                let host = project_host.clone();
                                panel.new_agent_in(Some(directory.clone()), host, window, cx)
                            })),
                    ),
                )
            })
            .when(!empty_project, |this| {
                this.child(rotating_chevron(
                    SharedString::from(format!("paseo-group-chevron-{key}")),
                    IconName::ChevronRight,
                    IconSize::XSmall,
                    !*collapsed,
                    0.25,
                    cx,
                ))
            });
        let header = div().mt_3().mb_0p5().child(header).into_any_element();
        match project_id {
            Some(project_id) => {
                let workspace = self.workspace.clone();
                right_click_menu(("paseo-project-menu", index))
                    .trigger(move |_, _, _| header)
                    .menu(move |window, cx| {
                        let workspace = workspace.clone();
                        let project_id = project_id.clone();
                        ContextMenu::build(window, cx, move |menu, _, cx| {
                            workspace_tools::project_menu(menu, &project_id, workspace.clone(), cx)
                        })
                    })
                    .into_any_element()
            }
            None => header,
        }
    }

    fn render_workspace(
        &self,
        index: usize,
        entry: WorkspaceEntry,
        keyboard_selected: bool,
        row_context: &RowContext,
        cx: &Context<Self>,
    ) -> AnyElement {
        let WorkspaceEntry {
            workspace_id,
            collapsed,
            agent_count,
            highlight_positions,
            single_agent,
        } = entry;
        let Some(workspace_row) = self.row_hosts.workspaces.get(workspace_id) else {
            return div().into_any_element();
        };
        let host = workspace_row.store.clone();
        let host_label = self.row_hosts.name(&host);
        let store = host.read(cx);
        let Some(workspace) = store.state.workspaces.get(workspace_id) else {
            return div().into_any_element();
        };
        let single_agent_row = single_agent.and_then(|agent_id| self.row_hosts.agent(agent_id, cx));
        // One state, most urgent first, so a row never shows two competing signals.
        let alert = match single_agent_row {
            Some((agent_row, _)) => AgentAlert::for_bucket(agent_row.bucket),
            None => match workspace.status.as_str() {
                "needs_input" => Some(AgentAlert::NeedsInput),
                "failed" => Some(AgentAlert::Failed),
                _ if workspace_row.unread => Some(AgentAlert::Unread),
                "running" => Some(AgentAlert::Running),
                _ => None,
            },
        };
        let dot = alert.map(AgentAlert::dot_color);
        let is_active = single_agent.is_some_and(|agent_id| {
            agent_row_selected(
                row_context.focused_agent.as_deref(),
                row_context.active_tab_agent.as_deref(),
                agent_id,
            )
        });
        let timestamp = single_agent_row
            .and_then(|(_, agent)| agent_updated_at(agent))
            .or_else(|| workspace.activity_at.as_deref().and_then(parse_timestamp))
            .map(|updated| format_relative(updated, row_context.now));
        let is_worktree = workspace.is_worktree();
        let single_agent = single_agent.map(str::to_owned);
        let entry = SidebarEntry::Workspace {
            workspace_id: workspace_id.to_owned(),
            collapsed,
            agent_count,
            highlight_positions: Vec::new(),
            single_agent: single_agent.clone(),
        };
        let (details, muted) = workspace_row_details(
            workspace,
            agent_count,
            host_label,
            self.grouping != SidebarGrouping::Project,
            timestamp,
        );
        let group = format!("paseo-workspace-{index}");
        // The status grouping lists a workspace's agents as its tabs, not rows under it.
        let nests_agents = self.grouping != SidebarGrouping::Status;
        let chevron = (nests_agents && agent_count > 0 && single_agent.is_none()).then(|| {
            let collapse_key = workspace_collapse_key(workspace_id);
            let chevron_id = SharedString::from(format!("paseo-workspace-chevron-{collapse_key}"));
            div()
                .pt_0p5()
                .child(
                    ui::ButtonLike::new(("paseo-workspace-collapse", index))
                        .style(ButtonStyle::Subtle)
                        .size(ui::ButtonSize::Compact)
                        .when(!collapsed, |this| this.visible_on_hover(group.clone()))
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            cx.stop_propagation();
                            panel.toggle_collapsed(collapse_key.clone(), cx);
                        }))
                        .child(rotating_chevron(
                            chevron_id,
                            IconName::ChevronRight,
                            IconSize::XSmall,
                            !collapsed,
                            0.25,
                            cx,
                        )),
                )
                .into_any_element()
        });
        let row = SidebarRow {
            id: ("paseo-sidebar-workspace", index).into(),
            icon: Icon::new(match single_agent_row {
                Some((_, agent)) => provider_icon(agent_provider(agent)),
                None if is_worktree => IconName::GitBranch,
                None => IconName::Folder,
            })
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element(),
            title: workspace.name.clone().into(),
            highlight_positions: highlight_positions.to_vec(),
            title_generating: false,
            details,
            trailing: dot
                .map(|color| {
                    div()
                        .pt_1p5()
                        .child(Indicator::dot().color(color))
                        .into_any_element()
                })
                .into_iter()
                .chain(chevron)
                .collect(),
            selected: is_active,
            keyboard_selected,
            muted,
            path: workspace_row.path.clone(),
        }
        .render(cx)
        .group(group)
        .on_click(cx.listener(move |panel, _, window, cx| {
            panel.selected = Some(index);
            panel.activate(&entry, false, window, cx)
        }))
        .into_any_element();
        let menu_row = self.workspace_row_menu(index, row, workspace_id, host, single_agent);
        let row = alert_line(menu_row, alert, ("paseo-workspace-alert", index), cx);
        row_motion(
            row,
            &workspace_row.motion_key,
            &self.motion,
            row_context.animate,
            cx,
        )
    }

    /// A workspace row's right-click menu: the workspace's, and archiving the agent of a row that
    /// shows a single agent.
    fn workspace_row_menu(
        &self,
        index: usize,
        row: AnyElement,
        workspace_id: &str,
        host: Entity<PaseoStore>,
        single_agent: Option<String>,
    ) -> AnyElement {
        let menu_workspace = self.workspace.clone();
        let menu_workspace_id = workspace_id.to_owned();
        right_click_menu(("paseo-workspace-menu", index))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let workspace = menu_workspace.clone();
                let workspace_id = menu_workspace_id.clone();
                let host = host.clone();
                let single_agent = single_agent.clone();
                ContextMenu::build(window, cx, move |menu, _, cx| {
                    let menu =
                        workspace_tools::workspace_menu(menu, &workspace_id, workspace.clone(), cx);
                    let Some(agent_id) = single_agent.clone() else {
                        return menu;
                    };
                    menu.separator().entry(
                        "Archive Agent",
                        Some(ArchiveAgent.boxed_clone()),
                        move |_, cx| host.update(cx, |store, cx| store.archive(&agent_id, cx)),
                    )
                })
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_agent(
        &self,
        index: usize,
        agent_id: &str,
        nested: bool,
        highlight_positions: Vec<usize>,
        keyboard_selected: bool,
        row_context: &RowContext,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some((agent_row, agent)) = self.row_hosts.agent(agent_id, cx) else {
            return div().into_any_element();
        };
        let host = agent_row.store.clone();
        let host_label = self.row_hosts.name(&host);
        let bucket = agent_row.bucket;
        let alert = AgentAlert::for_bucket(bucket);
        let timestamp = agent_updated_at(agent)
            .map(|updated| format_relative(updated, row_context.now))
            .unwrap_or_default();
        let is_active = agent_row_selected(
            row_context.focused_agent.as_deref(),
            row_context.active_tab_agent.as_deref(),
            agent_id,
        );
        let agent_id_owned = agent_id.to_owned();
        let needs_attention = agent_requires_attention(agent);
        let project_name =
            (self.grouping != SidebarGrouping::Project).then(|| agent_project_name(agent));
        let icon = match bucket {
            AgentBucket::NeedsInput => Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Warning)
                .into_any_element(),
            AgentBucket::Failed => Icon::new(IconName::Close)
                .size(IconSize::XSmall)
                .color(Color::Error)
                .into_any_element(),
            AgentBucket::Attention if needs_attention => Icon::new(IconName::Circle)
                .size(IconSize::XSmall)
                .color(Color::Accent)
                .into_any_element(),
            _ => Icon::new(provider_icon(agent_provider(agent)))
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
        };
        let item = SidebarRow {
            id: ("paseo-agent", index).into(),
            icon,
            title: agent_row.title.clone(),
            highlight_positions,
            title_generating: agent.title.is_none() && bucket == AgentBucket::Running,
            details: RowDetails {
                // A nested agent's workspace row above it already shows its host.
                host: host_label.filter(|_| !nested),
                project: project_name,
                // A nested agent's workspace row above it already shows where it works.
                worktree: (!nested).then(|| agent_worktree_name(agent)).flatten(),
                branch: (!nested).then(|| agent_branch(agent)).flatten(),
                timestamp: Some(timestamp),
                note: None,
            },
            trailing: Vec::new(),
            selected: is_active,
            keyboard_selected,
            muted: false,
            path: agent_row.path.clone(),
        }
        .render(cx)
        .on_click(cx.listener(move |panel, _, window, cx| {
            panel.selected = Some(index);
            let entry = SidebarEntry::Agent {
                agent_id: agent_id_owned.clone(),
                nested,
                highlight_positions: Vec::new(),
            };
            panel.activate(&entry, true, window, cx);
        }));
        let menu_row = self.agent_row_menu(
            index,
            item.into_any_element(),
            agent_id,
            agent.directory.clone(),
            host,
            needs_attention,
            cx,
        );
        let row = alert_line(menu_row, alert, ("paseo-agent-alert", index), cx);
        row_motion(row, agent_id, &self.motion, row_context.animate, cx)
    }

    /// An agent row's right-click menu.
    #[allow(clippy::too_many_arguments)]
    fn agent_row_menu(
        &self,
        index: usize,
        item: AnyElement,
        agent_id: &str,
        directory: Option<PathBuf>,
        host: Entity<PaseoStore>,
        needs_attention: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let this = cx.weak_entity();
        let menu_agent = agent_id.to_owned();
        right_click_menu(("paseo-agent-menu", index))
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| {
                let this = this.clone();
                let agent_id = menu_agent.clone();
                let directory = directory.clone();
                let host = host.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let open = (this.clone(), agent_id.clone());
                    let rename = (this.clone(), agent_id.clone());
                    let archive = (host.clone(), agent_id.clone());
                    let new_here = (this.clone(), directory.clone(), Some(host.clone()));
                    let copy_id = agent_id.clone();
                    let copy_path = directory.clone();
                    let workspace_agent = (this.clone(), agent_id.clone());
                    let read_agent = (host.clone(), agent_id.clone());
                    let fork = (this.clone(), host.clone(), agent_id.clone());
                    let menu = menu
                        .entry("Open", None, move |window, cx| {
                            let (this, agent_id) = open.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                let entry = SidebarEntry::Agent {
                                    agent_id,
                                    nested: false,
                                    highlight_positions: Vec::new(),
                                };
                                panel.activate(&entry, true, window, cx)
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry(
                            "Rename…",
                            Some(RenameAgent.boxed_clone()),
                            move |window, cx| {
                                let (this, agent_id) = rename.clone();
                                if let Err(error) =
                                    this.update(cx, |panel, cx| panel.rename(agent_id, window, cx))
                                {
                                    log::debug!("Paseo sidebar released: {error}");
                                }
                            },
                        )
                        .entry("Fork", None, move |window, cx| {
                            let (this, host, agent_id) = fork.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                if let Some(workspace) = panel.workspace.upgrade() {
                                    workspace.update(cx, |workspace, cx| {
                                        crate::fork_agent(workspace, host, &agent_id, window, cx)
                                    });
                                }
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry("New Agent in Project", None, move |window, cx| {
                            let (this, directory, host) = new_here.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                panel.new_agent_in(directory, host, window, cx)
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry("Open in Editor", None, move |window, cx| {
                            let (this, agent_id) = workspace_agent.clone();
                            if let Err(error) = this.update(cx, |_, cx| {
                                hosts::set_focused_agent(agent_id, cx);
                                window.dispatch_action(OpenWorkspace.boxed_clone(), cx);
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .when(needs_attention, |menu| {
                            menu.entry("Mark as Read", None, move |_, cx| {
                                let (host, agent_id) = &read_agent;
                                host.update(cx, |store, cx| store.clear_attention(agent_id, cx))
                            })
                        })
                        .separator()
                        .entry("Copy Agent ID", None, move |_, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                        })
                        .when_some(copy_path, |menu, path| {
                            menu.entry("Copy Path", None, move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    path.to_string_lossy().into_owned(),
                                ));
                            })
                        })
                        .separator()
                        .entry("Archive", Some(ArchiveAgent.boxed_clone()), move |_, cx| {
                            let (host, agent_id) = &archive;
                            host.update(cx, |store, cx| store.archive(agent_id, cx))
                        });
                    menu
                })
            })
            .into_any_element()
    }

    fn render_empty(&self, cx: &Context<Self>) -> AnyElement {
        let stores = self.shown_stores(cx);
        let statuses = stores
            .iter()
            .map(|store| store.read(cx).status)
            .collect::<Vec<_>>();
        // Any connected host can start agents; otherwise the most hopeful state shows.
        let status = if statuses.contains(&ConnectionStatus::Connected) {
            ConnectionStatus::Connected
        } else if statuses.contains(&ConnectionStatus::Reconnecting) {
            ConnectionStatus::Reconnecting
        } else if statuses.contains(&ConnectionStatus::Connecting) {
            ConnectionStatus::Connecting
        } else {
            ConnectionStatus::Disconnected
        };
        // With one host, its own reason beats the generic hint.
        let connection_error = match stores.as_slice() {
            [store] => store.read(cx).state.error.clone(),
            _ => None,
        };
        let (title, detail) = match status {
            ConnectionStatus::Connected => {
                ("No agents yet", "Start a new agent to work on a project.")
            }
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => (
                "Connecting to Paseo…",
                "Agents appear once the daemon answers.",
            ),
            ConnectionStatus::Disconnected => (
                "Not connected",
                "Start the Paseo daemon, or choose another host.",
            ),
        };
        // A connection the profile itself rules out, such as a remote daemon without an editor
        // SSH mapping, needs its reason rather than the generic hint.
        let detail = match (status, connection_error) {
            (ConnectionStatus::Disconnected, Some(error)) => SharedString::from(error),
            _ => SharedString::from(detail),
        };
        let actions = (status == ConnectionStatus::Disconnected).then(|| {
            h_flex()
                .gap_1()
                .child(
                    ui::Button::new("paseo-retry", "Try again")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(|_, _, cx| crate::hosts::connect_all(true, cx)),
                )
                .child(
                    ui::Button::new("paseo-hosts", "Hosts…")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(ManageHosts.boxed_clone(), cx)
                        }),
                )
                .into_any_element()
        });
        // A column, so the message is no wider than the panel and long errors wrap.
        v_flex()
            .px_4()
            .items_center()
            .child(crate::render_message(title, Some(detail), actions))
            .into_any_element()
    }
}

/// Every shown host's sidebar state as one. Daemon IDs don't repeat across hosts, so nothing
/// collides; labels are names, so hosts that share one group under it once.
#[derive(Default)]
struct MergedState<'a> {
    agents: Vec<&'a AgentSummary>,
    workspaces: BTreeMap<&'a str, &'a WorkspaceDescriptor>,
    projects: BTreeMap<&'a str, &'a ProjectDescriptor>,
    labels: Vec<&'a WorkspaceLabel>,
    pending: HashSet<&'a str>,
}

fn merged_state<'a>(stores: &[Entity<PaseoStore>], cx: &'a App) -> MergedState<'a> {
    let mut merged = MergedState::default();
    for store in stores {
        let store = store.read(cx);
        merged.agents.extend(store.state.agents().iter());
        merged.workspaces.extend(
            store
                .state
                .workspaces
                .iter()
                .map(|(id, workspace)| (id.as_str(), workspace)),
        );
        merged.projects.extend(
            store
                .state
                .projects
                .iter()
                .map(|(id, project)| (id.as_str(), project)),
        );
        for label in &store.state.labels {
            if !merged
                .labels
                .iter()
                .any(|existing| existing.name == label.name)
            {
                merged.labels.push(label);
            }
        }
        merged.pending.extend(
            store
                .state
                .permissions
                .values()
                .map(|request| request.agent_id.as_str()),
        );
    }
    merged
}

/// The hosts the sidebar really shows: the saved filter's hosts that still exist, or every host
/// when it names none of them, so a removed host never empties the sidebar.
fn effective_host_filter(filter: &BTreeSet<String>, listed: &BTreeSet<String>) -> BTreeSet<String> {
    let kept = filter
        .intersection(listed)
        .cloned()
        .collect::<BTreeSet<_>>();
    if kept.is_empty() {
        listed.clone()
    } else {
        kept
    }
}

/// Shows or hides one host's agents. Hiding from "all hosts" lists every other host.
fn toggle_host_filter(name: &str, cx: &mut App) {
    let listed = hosts::configured_hosts(cx)
        .into_iter()
        .map(|(name, _)| name)
        .collect::<BTreeSet<_>>();
    let hosts = cx.update_global::<SharedSidebarView, _>(|shared, _| {
        let mut shown = effective_host_filter(&shared.hosts, &listed);
        if !shown.remove(name) {
            shown.insert(name.to_owned());
        }
        // Every host shown, or none, is the same as no filter.
        if shown.is_empty() || shown == listed {
            shown.clear();
        }
        shared.hosts = shown.clone();
        shown
    });
    match serde_json::to_string(&hosts) {
        Ok(value) => {
            let kvp = KeyValueStore::global(cx);
            db::write_and_log(cx, move || async move {
                kvp.write_kvp(HOST_FILTER_KEY.to_string(), value).await
            });
        }
        Err(error) => log::error!("Could not save the Paseo host filter: {error}"),
    }
}

pub(crate) fn open_row_at_index(
    workspace: &mut Workspace,
    index: usize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<PaseoPanel>(cx) else {
        return;
    };
    let targets = panel.update(cx, |panel, cx| {
        number_key_targets(panel.current_entries(cx))
    });
    match index.checked_sub(1).and_then(|index| targets.get(index)) {
        Some(NumberKeyTarget::Agent(agent_id)) => {
            let agent_id = agent_id.clone();
            open_agent(workspace, &agent_id, true, window, cx);
        }
        Some(NumberKeyTarget::Workspace(workspace_id)) => {
            let workspace_id = workspace_id.clone();
            crate::open_paseo_workspace_tabs(workspace, &workspace_id, window, cx);
        }
        None => {}
    }
}

pub(crate) fn open_adjacent_agent(
    workspace: &mut Workspace,
    step: isize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<PaseoPanel>(cx) else {
        return;
    };
    let order = panel.update(cx, |panel, cx| agent_order(panel.current_entries(cx)));
    if order.is_empty() {
        return;
    }
    let current = hosts::focused_agent(cx);
    let position = current
        .and_then(|current| order.iter().position(|agent_id| *agent_id == current))
        .map(|position| position as isize + step)
        .unwrap_or(0)
        .rem_euclid(order.len() as isize) as usize;
    if let Some(agent_id) = order.get(position) {
        let agent_id = agent_id.clone();
        open_agent(workspace, &agent_id, true, window, cx);
    }
}

impl EventEmitter<PanelEvent> for PaseoPanel {}

impl Focusable for PaseoPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Whether the sidebar has any agent to show, counting those inside a collapsed group.
fn lists_agents(entries: &[SidebarEntry]) -> bool {
    entries.iter().any(|entry| match entry {
        SidebarEntry::Agent { .. } => true,
        SidebarEntry::Workspace { agent_count, .. } => *agent_count > 0,
        SidebarEntry::Header { count, .. } => *count > 0,
    })
}

impl Render for PaseoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_store_changes(cx);
        let colors = cx.theme().colors();
        self.row_context = Some(RowContext {
            focused_agent: hosts::focused_agent(cx),
            active_tab_agent: self.active_agent_tab_id(cx),
            panel_focused: self.focus_handle.contains_focused(window, cx),
            animate: crate::PaseoSettings::get_global(cx).sidebar.animate_status,
            now: Utc::now(),
        });
        v_flex()
            .id("paseo-sidebar")
            .debug_selector(|| "paseo-sidebar".into())
            .key_context("PaseoSidebar PaseoView")
            .on_hover(
                cx.listener(|panel, hovered: &bool, _, cx| panel.set_pointer_inside(*hovered, cx)),
            )
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::toggle_group_by_status))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::archive_selected))
            .on_action(cx.listener(Self::rename_selected))
            .on_action(cx.listener(Self::copy_selected_id))
            .size_full()
            .bg(colors.panel_background)
            .child(self.render_header(cx))
            .child(self.render_nav(window, cx))
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(Self::render_list_item),
                )
                .flex_1()
                .min_h_0()
                .pb_3(),
            )
    }
}

impl PaseoPanel {
    fn render_list_item(
        &mut self,
        index: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // List items are laid out alone and would shrink to their content, and the list pads only
        // its top and bottom, so each item spans the list and pads its own sides.
        let column = v_flex()
            .w_full()
            .debug_selector(|| format!("paseo-sidebar-item-{index}"));
        let column = match index.checked_sub(LEADING_ROWS) {
            None => column
                .children(self.render_host_problems(cx))
                .when(!lists_agents(&self.entries), |this| {
                    this.child(self.render_empty(cx))
                }),
            Some(entry_index) => match (self.entries.get(entry_index), self.row_context.as_ref()) {
                (Some(entry), Some(row_context)) => {
                    column.child(self.render_entry(entry_index, entry, row_context, cx))
                }
                _ => return gpui::Empty.into_any_element(),
            },
        };
        div().w_full().px_2().child(column).into_any_element()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PaseoPanel {
    /// A sidebar outside any workspace, listing the hosts `crate::test_init` set up.
    pub fn test_new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(WeakEntity::new_invalid(), window, cx)
    }

    pub fn test_toggle_grouping(panel: &Entity<Self>, window: &mut Window, cx: &mut App) {
        panel.update(cx, |panel, cx| {
            panel.toggle_group_by_status(&ToggleGroupByStatus, window, cx)
        });
    }

    pub fn test_groups_by_status(panel: &Entity<Self>, cx: &App) -> bool {
        panel.read(cx).grouping == SidebarGrouping::Status
    }

    pub fn test_set_pointer_inside(panel: &Entity<Self>, inside: bool, cx: &mut App) {
        panel.update(cx, |panel, cx| panel.set_pointer_inside(inside, cx));
    }

    pub fn test_refresh_pending(panel: &Entity<Self>, cx: &App) -> bool {
        panel.read(cx).refresh_pending
    }

    /// Whether the row of `agent_id` flashes for a state change.
    pub fn test_flashing(panel: &Entity<Self>, agent_id: &str, cx: &App) -> bool {
        panel.read(cx).motion.changed_at.contains_key(agent_id)
    }

    pub fn test_rows_pending(panel: &Entity<Self>, cx: &App) -> bool {
        panel.read(cx).stale
    }

    /// The agents in the order next/previous agent steps through them.
    pub fn test_agent_order(panel: &Entity<Self>, cx: &mut App) -> Vec<String> {
        panel.update(cx, |panel, cx| agent_order(panel.current_entries(cx)))
    }

    /// What the dock does after the user resizes the sidebar.
    pub fn test_size_changed(panel: &Entity<Self>, window: &mut Window, cx: &mut App) {
        panel.update(cx, |panel, cx| panel.size_state_changed(window, cx));
    }
}

impl Panel for PaseoPanel {
    fn persistent_name() -> &'static str {
        "Paseo"
    }

    fn panel_key() -> &'static str {
        "PaseoPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    // The dock calls this while it is being updated, so the new size is read afterwards.
    fn size_state_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.defer_in(window, move |_, _, cx| {
            if let Some(workspace) = workspace.upgrade() {
                crate::remember_sidebar_size(&workspace, cx);
            }
        });
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Paseo Agents")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(TogglePanel)
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }

    // A hidden panel gets no hover-leave event, so a pending re-sort would otherwise wait
    // until the pointer next passes over the panel.
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            self.set_pointer_inside(false, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_display_title;
    use serde_json::json;

    /// The bounds of the element tagged `selector` in the window's last frame.
    fn bounds_of(
        cx: &mut gpui::VisualTestContext,
        selector: String,
    ) -> Option<gpui::Bounds<Pixels>> {
        cx.debug_bounds(selector.leak())
    }

    fn sidebar_with_agents(
        count: usize,
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<PaseoPanel>, &mut gpui::VisualTestContext) {
        cx.update(crate::test_init);
        cx.update(|cx| {
            crate::test_set_agents(
                (0..count)
                    .map(|index| crate::test_agent(&format!("agent-{index}"), "Fix login", "idle"))
                    .collect(),
                cx,
            )
        });
        let (panel, cx) = cx.add_window_view(PaseoPanel::test_new);
        cx.run_until_parked();
        (panel, cx)
    }

    #[gpui::test]
    fn rows_fill_the_panel_width(cx: &mut gpui::TestAppContext) {
        let (_, cx) = sidebar_with_agents(3, cx);
        let sidebar = bounds_of(cx, "paseo-sidebar".into()).expect("the sidebar is drawn");
        let mut rows = 0;
        for index in 1..=4 {
            let Some(row) = bounds_of(cx, format!("paseo-sidebar-item-{index}")) else {
                continue;
            };
            rows += 1;
            assert_eq!(
                (row.origin.x, row.size.width),
                (sidebar.origin.x + px(8.), sidebar.size.width - px(16.)),
                "row {index} spans the list, inside its padding"
            );
        }
        assert!(
            rows >= 3,
            "a header and three agents are drawn, got {rows} rows"
        );
    }

    /// The list items the last frame drew, by list index.
    fn drawn_items(cx: &mut gpui::VisualTestContext, count: usize) -> Vec<usize> {
        (0..count)
            .filter(|index| bounds_of(cx, format!("paseo-sidebar-item-{index}")).is_some())
            .collect()
    }

    #[gpui::test]
    fn only_rows_in_view_are_drawn(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = sidebar_with_agents(500, cx);
        let items = cx.read(|cx| panel.read(cx).entries.len()) + LEADING_ROWS;
        let drawn = drawn_items(cx, items);
        let window = bounds_of(cx, "paseo-sidebar".into()).expect("the sidebar is drawn");
        assert!(!drawn.is_empty());
        assert!(
            (drawn.len() as f32) * 42. < 2. * f32::from(window.size.height),
            "about a window's worth of {items} rows is drawn, got {}",
            drawn.len()
        );
        assert!(!drawn.contains(&(items - 1)), "the last row is out of view");
    }

    #[gpui::test]
    fn keyboard_selection_scrolls_into_view(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = sidebar_with_agents(500, cx);
        let items = cx.read(|cx| panel.read(cx).entries.len()) + LEADING_ROWS;
        cx.focus(&panel);
        cx.dispatch_action(SelectLast);
        cx.run_until_parked();
        let window = bounds_of(cx, "paseo-sidebar".into()).expect("the sidebar is drawn");
        let last = bounds_of(cx, format!("paseo-sidebar-item-{}", items - 1))
            .expect("the selected last row is drawn");
        assert!(
            last.bottom() <= window.bottom() && last.top() >= window.top(),
            "the selected row {last:?} is inside the sidebar {window:?}"
        );
        assert!(
            !drawn_items(cx, items).contains(&1),
            "the first rows scrolled away"
        );
    }

    #[gpui::test]
    fn select_first_after_last_lands_on_the_top_row(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = sidebar_with_agents(500, cx);
        let items = cx.read(|cx| panel.read(cx).entries.len()) + LEADING_ROWS;
        cx.focus(&panel);
        cx.dispatch_action(SelectLast);
        cx.run_until_parked();
        cx.dispatch_action(SelectFirst);
        cx.run_until_parked();
        let drawn = drawn_items(cx, items);
        assert!(
            drawn.contains(&LEADING_ROWS),
            "the first row is drawn: {drawn:?}"
        );
        assert!(!drawn.contains(&(items - 1)), "the last row scrolled away");
    }

    #[gpui::test]
    fn stepping_past_the_bottom_scrolls_one_row(cx: &mut gpui::TestAppContext) {
        let (panel, cx) = sidebar_with_agents(500, cx);
        let items = cx.read(|cx| panel.read(cx).entries.len()) + LEADING_ROWS;
        cx.focus(&panel);
        let window = bounds_of(cx, "paseo-sidebar".into()).expect("the sidebar is drawn");
        let fully_shown = |cx: &mut gpui::VisualTestContext| {
            drawn_items(cx, items)
                .into_iter()
                .filter(|index| {
                    bounds_of(cx, format!("paseo-sidebar-item-{index}"))
                        .is_some_and(|row| row.bottom() <= window.bottom())
                })
                .max()
                .expect("rows are drawn")
        };
        let last_shown = fully_shown(cx);
        loop {
            cx.dispatch_action(SelectNext);
            cx.run_until_parked();
            let selected = cx
                .read(|cx| panel.read(cx).selected)
                .expect("a row is selected");
            if selected + LEADING_ROWS > last_shown {
                break;
            }
        }
        let first_drawn = drawn_items(cx, items)[0];
        assert!(
            (1..=3).contains(&first_drawn),
            "one step past the bottom scrolls about one row, not a page; first drawn {first_drawn}"
        );
    }

    #[gpui::test]
    fn the_empty_state_shows_without_agents(cx: &mut gpui::TestAppContext) {
        let (_, cx) = sidebar_with_agents(0, cx);
        let empty = bounds_of(cx, "paseo-sidebar-item-0".into()).expect("the empty state");
        assert!(empty.size.height > px(0.), "the empty state is shown");
        assert_eq!(drawn_items(cx, 3), [0], "no rows without agents");
    }

    #[gpui::test]
    fn host_problems_sit_above_the_rows(cx: &mut gpui::TestAppContext) {
        let (_, cx) = sidebar_with_agents(2, cx);
        let leading = bounds_of(cx, "paseo-sidebar-item-0".into()).expect("the leading item");
        assert_eq!(
            leading.size.height,
            px(0.),
            "one host with agents has nothing above its rows"
        );
        cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.paseo = Some(settings::PaseoSettingsContent {
                        profiles: Some(
                            ["One", "Two"]
                                .map(|name| crate::PaseoConnectionProfile {
                                    name: name.into(),
                                    target_uri: format!("ws://127.0.0.1:9/{name}"),
                                    editor_ssh_uri: None,
                                    client_id: String::new(),
                                })
                                .to_vec(),
                        ),
                        ..Default::default()
                    });
                });
            })
        });
        cx.run_until_parked();
        let banner = bounds_of(cx, "paseo-sidebar-item-0".into()).expect("the host problems");
        let first_row = bounds_of(cx, "paseo-sidebar-item-1".into()).expect("the first row");
        assert!(
            banner.size.height > px(0.),
            "two offline hosts show their problems"
        );
        assert!(
            banner.bottom() <= first_row.top(),
            "the problems sit above the rows"
        );
    }

    #[gpui::test]
    fn label_colors_follow_the_theme_terminal_palette(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let colors = cx.theme().colors().clone();
            for (name, expected) in [
                ("red", colors.terminal_ansi_red),
                ("amber", colors.terminal_ansi_yellow),
                ("emerald", colors.terminal_ansi_green),
                ("sky", colors.terminal_ansi_cyan),
                ("teal", colors.terminal_ansi_bright_cyan),
                ("blue", colors.terminal_ansi_blue),
                ("indigo", colors.terminal_ansi_bright_blue),
                ("violet", colors.terminal_ansi_magenta),
                ("pink", colors.terminal_ansi_bright_magenta),
                ("orange", colors.terminal_ansi_bright_red),
                ("unknown", colors.terminal_ansi_blue),
            ] {
                assert_eq!(label_color(name, cx), expected, "{name}");
            }
        });
    }

    #[test]
    fn collapsed_group_still_counts_as_agents() {
        let header = |count| SidebarEntry::Header {
            key: "status:done".into(),
            label: "Done".into(),
            count,
            collapsed: true,
            directory: None,
            project_id: None,
            color: None,
        };
        assert!(lists_agents(&[header(7)]));
        assert!(!lists_agents(&[header(0)]));
        assert!(!lists_agents(&[]));
    }

    #[test]
    fn host_filter_menu_ignores_removed_hosts() {
        let names = |names: &[&str]| -> BTreeSet<String> {
            names.iter().map(|name| name.to_string()).collect()
        };
        let listed = names(&["Local", "box"]);
        assert_eq!(effective_host_filter(&names(&[]), &listed), listed);
        // A filter naming only removed hosts would empty the sidebar, so it lists everything.
        assert_eq!(effective_host_filter(&names(&["gone"]), &listed), listed);
        assert_eq!(
            effective_host_filter(&names(&["box", "gone"]), &listed),
            names(&["box"])
        );
    }

    #[gpui::test]
    fn sidebar_merges_hosts(cx: &mut gpui::TestAppContext) {
        let label = |name: &str| WorkspaceLabel {
            name: name.into(),
            color: "sky".into(),
        };
        let (local, remote) = cx.update(|cx| {
            let local = cx.new(|_| {
                let mut store = PaseoStore::default();
                store.state.test_set_agents(vec![agent(
                    "a1",
                    "Local",
                    "idle",
                    "/home/me/a",
                    "2026-10-01T10:00:00Z",
                )]);
                store.state.labels = vec![label("urgent")];
                store
            });
            let remote = cx.new(|_| {
                let mut store = PaseoStore::default();
                store.state.test_set_agents(vec![agent(
                    "b1",
                    "Remote",
                    "idle",
                    "/srv/b",
                    "2026-10-01T11:00:00Z",
                )]);
                store.state.labels = vec![label("urgent"), label("infra")];
                store
            });
            (local, remote)
        });
        cx.update(|cx| {
            let configured = vec![
                ("Local".to_owned(), local.clone()),
                ("Remote".to_owned(), remote.clone()),
            ];
            let row_hosts = RowHosts::new(&[local.clone(), remote.clone()], configured, None, cx);
            let host_of = |agent_id: &str| row_hosts.agents.get(agent_id).map(|row| &row.store);
            assert_eq!(row_hosts.name(&remote).as_deref(), Some("Remote"));
            assert_eq!(host_of("a1"), Some(&local));
            assert_eq!(host_of("b1"), Some(&remote));
            let merged = merged_state(&[local, remote], cx);
            let agents = merged
                .agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(agents, vec!["a1", "b1"]);
            let labels = merged
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(labels, vec!["urgent", "infra"]);
        });
    }

    #[gpui::test]
    fn row_cache_gathers_titles_buckets_and_unread_workspaces(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| {
            cx.new(|_| {
                let mut store = PaseoStore::default();
                store.state.test_set_agents(vec![
                    placed(
                        agent(
                            "alone",
                            "Agent title",
                            "running",
                            "/w",
                            "2026-10-01T10:00:00Z",
                        ),
                        "wks-alone",
                    ),
                    AgentSummary {
                        extra: json!({"workspaceId": "wks-shared", "requiresAttention": true}),
                        ..agent("first", "First", "idle", "/w", "2026-10-01T10:00:00Z")
                    },
                    placed(
                        agent("second", "Second", "running", "/w", "2026-10-01T10:00:00Z"),
                        "wks-shared",
                    ),
                ]);
                for (id, name) in [("wks-alone", "Alone"), ("wks-shared", "Shared")] {
                    store
                        .state
                        .workspaces
                        .insert(id.into(), workspace(id, "prj", name));
                }
                store.state.permissions.insert(
                    "request".into(),
                    paseo_client::PermissionRequest {
                        agent_id: "second".into(),
                        request_id: "request".into(),
                        title: "Run a command".into(),
                        description: None,
                        extra: json!({}),
                    },
                );
                store
            })
        });
        cx.update(|cx| {
            let row_hosts = RowHosts::new(std::slice::from_ref(&store), Vec::new(), None, cx);
            let row = |agent_id: &str| {
                let (row, agent) = row_hosts.agent(agent_id, cx).expect("row");
                assert_eq!(agent.id, agent_id);
                (row.title.to_string(), row.bucket)
            };
            assert_eq!(row("alone"), ("Alone".into(), AgentBucket::Running));
            assert_eq!(row("first"), ("First".into(), AgentBucket::Attention));
            assert_eq!(row("second"), ("Second".into(), AgentBucket::NeedsInput));
            let unread =
                |workspace_id: &str| row_hosts.workspaces.get(workspace_id).map(|row| row.unread);
            assert_eq!(unread("wks-shared"), Some(true));
            assert_eq!(unread("wks-alone"), Some(false));
        });
    }

    #[gpui::test]
    fn row_cache_keeps_rows_that_left_while_a_reorder_waits(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| {
            cx.new(|_| {
                let mut store = PaseoStore::default();
                store.state.test_set_agents(vec![
                    agent("kept", "Kept", "idle", "/w", "2026-10-01T10:00:00Z"),
                    agent("leaving", "Leaving", "idle", "/w", "2026-10-01T10:00:00Z"),
                ]);
                store
            })
        });
        let previous =
            cx.update(|cx| RowHosts::new(std::slice::from_ref(&store), Vec::new(), None, cx));
        store.update(cx, |store, _| {
            let kept = AgentSummary {
                title: Some("Renamed".into()),
                ..agent("kept", "Kept", "idle", "/w", "2026-10-01T10:00:00Z")
            };
            store.state.test_set_agents(vec![kept]);
        });
        cx.update(|cx| {
            let held = RowHosts::new(std::slice::from_ref(&store), Vec::new(), Some(previous), cx);
            assert_eq!(
                held.agents
                    .get("kept")
                    .map(|row| row.title.to_string())
                    .as_deref(),
                Some("Renamed"),
                "rows stay live while the order waits"
            );
            assert!(held.agents.contains_key("leaving"));
            let fresh = RowHosts::new(std::slice::from_ref(&store), Vec::new(), None, cx);
            assert!(!fresh.agents.contains_key("leaving"));
        });
    }

    #[test]
    fn workspace_counts_title_agents_like_a_scan() {
        let agents = vec![
            placed(
                agent("alone", "Alone agent", "idle", "/w", "2026-10-01T10:00:00Z"),
                "wks-1",
            ),
            placed(
                agent("pair-a", "Pair A", "idle", "/w", "2026-10-01T10:00:00Z"),
                "wks-2",
            ),
            placed(
                agent("pair-b", "Pair B", "idle", "/w", "2026-10-01T10:00:00Z"),
                "wks-2",
            ),
            placed(
                agent("unnamed", "Unnamed", "idle", "/w", "2026-10-01T10:00:00Z"),
                "wks-3",
            ),
            placed(
                agent("missing", "Missing", "idle", "/w", "2026-10-01T10:00:00Z"),
                "wks-4",
            ),
            agent("loose", "", "idle", "/w", "2026-10-01T10:00:00Z"),
        ];
        let workspaces = BTreeMap::from([
            ("wks-1".to_owned(), workspace("wks-1", "prj", "One")),
            ("wks-2".to_owned(), workspace("wks-2", "prj", "Two")),
            ("wks-3".to_owned(), workspace("wks-3", "prj", " ")),
        ]);
        let counts = WorkspaceAgentCounts::new(&agents);
        for agent in &agents {
            assert_eq!(
                counts.display_title(&workspaces, agent),
                agent_display_title(&agents, &workspaces, agent),
                "{}",
                agent.id
            );
        }
    }

    #[test]
    fn title_matches_ignore_case_without_lowercasing_the_whole_title() {
        assert_eq!(
            title_match_positions("Fix Login", "login"),
            Some(vec![4, 5, 6, 7, 8])
        );
        assert_eq!(title_match_positions("ÉCOLE école", "éc"), Some(vec![0, 2]));
        assert_eq!(title_match_positions("abc", "abcd"), None);
        assert_eq!(title_match_positions("abc", ""), Some(Vec::new()));
        assert_eq!(title_match_positions("", "a"), None);
    }

    fn agent(id: &str, title: &str, status: &str, cwd: &str, updated: &str) -> AgentSummary {
        AgentSummary {
            id: id.into(),
            title: Some(title.into()),
            status: status.into(),
            directory: Some(PathBuf::from(cwd)),
            project: None,
            extra: json!({"updatedAt": updated}),
        }
    }

    #[test]
    fn row_motion_flashes_state_changes_and_eases_in_new_agents() {
        let now = Instant::now();
        let mut motion = RowMotion::default();
        motion.observe(
            [
                ("a".to_owned(), AgentBucket::Running),
                ("b".to_owned(), AgentBucket::Done),
            ],
            now,
        );
        assert!(motion.appeared_at.is_empty());
        assert!(motion.changed_at.is_empty());

        motion.observe(
            [
                ("a".to_owned(), AgentBucket::NeedsInput),
                ("b".to_owned(), AgentBucket::Done),
                ("c".to_owned(), AgentBucket::Running),
            ],
            now,
        );
        assert_eq!(
            motion.changed_at.get("a").map(|(_, alert)| *alert),
            Some(AgentAlert::NeedsInput)
        );
        assert!(!motion.changed_at.contains_key("b"));
        assert!(motion.appeared_at.contains_key("c"));

        motion.observe(
            [
                ("a".to_owned(), AgentBucket::Done),
                ("b".to_owned(), AgentBucket::Done),
                ("c".to_owned(), AgentBucket::Running),
            ],
            now,
        );
        assert!(!motion.changed_at.contains_key("a"));

        motion.observe(
            [("c".to_owned(), AgentBucket::Running)],
            now + STATE_FLASH * 3,
        );
        assert!(motion.appeared_at.is_empty());

        // A host's list arriving after an empty start, or in a burst, doesn't animate.
        let mut starting = RowMotion::default();
        starting.observe([], now);
        starting.observe([("a".to_owned(), AgentBucket::Done)], now);
        assert!(starting.appeared_at.is_empty());
        let burst = (0..=MAX_ANIMATED_APPEARANCES)
            .map(|index| (format!("new-{index}"), AgentBucket::Done))
            .chain([("a".to_owned(), AgentBucket::Done)]);
        starting.observe(burst, now);
        assert!(starting.appeared_at.is_empty());
    }

    #[test]
    fn agent_alert_pulses_for_running_and_waiting_agents() {
        let running = agent("a", "A", "running", "/one", "2026-09-30T00:00:00Z");
        let mut unread = agent("b", "B", "idle", "/one", "2026-09-30T00:00:00Z");
        unread.extra["requiresAttention"] = json!(true);
        let idle = agent("c", "C", "idle", "/one", "2026-09-30T00:00:00Z");
        let failed = agent("d", "D", "error", "/one", "2026-09-30T00:00:00Z");

        assert_eq!(
            AgentAlert::for_bucket(agent_bucket(&running, true)),
            Some(AgentAlert::NeedsInput)
        );
        assert_eq!(
            AgentAlert::for_bucket(agent_bucket(&running, false)),
            Some(AgentAlert::Running)
        );
        assert_eq!(
            AgentAlert::for_bucket(agent_bucket(&unread, false)),
            Some(AgentAlert::Unread)
        );
        assert_eq!(
            AgentAlert::for_bucket(agent_bucket(&failed, false)),
            Some(AgentAlert::Failed)
        );
        assert_eq!(AgentAlert::for_bucket(agent_bucket(&idle, false)), None);
        assert!(AgentAlert::NeedsInput.pulse() < AgentAlert::Running.pulse());
        assert_eq!(AgentAlert::Unread.pulse(), None);
        assert_eq!(AgentAlert::Failed.pulse(), None);
    }

    fn placed(mut agent: AgentSummary, workspace_id: &str) -> AgentSummary {
        agent.extra["workspaceId"] = json!(workspace_id);
        agent
    }

    fn workspace(id: &str, project_id: &str, name: &str) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            id: id.into(),
            project_id: project_id.into(),
            project_display_name: project_id.trim_start_matches("prj_").into(),
            project_root_path: PathBuf::from(format!(
                "/work/{}",
                project_id.trim_start_matches("prj_")
            )),
            directory: PathBuf::from(format!("/work/{}", project_id.trim_start_matches("prj_"))),
            kind: "directory".into(),
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
            is_paseo_worktree: false,
            extra: json!({}),
        }
    }

    fn project(id: &str, name: &str, root: &str) -> ProjectDescriptor {
        ProjectDescriptor {
            id: id.into(),
            display_name: name.into(),
            custom_name: None,
            icon_revision: None,
            root_path: PathBuf::from(root),
            kind: "git".into(),
        }
    }

    struct Fixture {
        agents: Vec<AgentSummary>,
        workspaces: BTreeMap<String, WorkspaceDescriptor>,
        projects: BTreeMap<String, ProjectDescriptor>,
        labels: Vec<WorkspaceLabel>,
        pending: HashSet<String>,
    }

    impl Fixture {
        fn new(agents: Vec<AgentSummary>) -> Self {
            Self {
                agents,
                workspaces: BTreeMap::new(),
                projects: BTreeMap::new(),
                labels: Vec::new(),
                pending: HashSet::new(),
            }
        }

        fn with_workspaces(mut self, workspaces: Vec<WorkspaceDescriptor>) -> Self {
            self.workspaces = workspaces
                .into_iter()
                .map(|workspace| (workspace.id.clone(), workspace))
                .collect();
            self
        }

        fn with_projects(mut self, projects: Vec<ProjectDescriptor>) -> Self {
            self.projects = projects
                .into_iter()
                .map(|project| (project.id.clone(), project))
                .collect();
            self
        }

        fn entries(
            &self,
            grouping: SidebarGrouping,
            collapsed: &HashSet<String>,
            filter: &str,
        ) -> Vec<SidebarEntry> {
            let agents = self.agents.iter().collect::<Vec<_>>();
            let workspaces = self
                .workspaces
                .iter()
                .map(|(id, workspace)| (id.as_str(), workspace))
                .collect();
            let projects = self
                .projects
                .iter()
                .map(|(id, project)| (id.as_str(), project))
                .collect();
            let labels = self.labels.iter().collect::<Vec<_>>();
            let pending = self.pending.iter().map(String::as_str).collect();
            build_entries(&SidebarInputs {
                agents: &agents,
                workspaces: &workspaces,
                projects: &projects,
                labels: &labels,
                pending_permission_agents: &pending,
                grouping,
                collapsed,
                filter,
            })
        }
    }

    /// Each entry as a short line: `# header`, `> workspace`, `- agent`, `  - nested agent`.
    fn outline(entries: &[SidebarEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                SidebarEntry::Header { label, .. } => format!("# {label}"),
                SidebarEntry::Workspace {
                    workspace_id,
                    single_agent,
                    ..
                } => match single_agent {
                    Some(agent_id) => format!("> {workspace_id} = {agent_id}"),
                    None => format!("> {workspace_id}"),
                },
                SidebarEntry::Agent {
                    agent_id, nested, ..
                } => {
                    if *nested {
                        format!("  - {agent_id}")
                    } else {
                        format!("- {agent_id}")
                    }
                }
            })
            .collect()
    }

    #[test]
    fn project_groups_sort_by_recent_activity_and_filter_by_title() {
        let fixture = Fixture::new(vec![
            agent(
                "a",
                "Fix login",
                "idle",
                "/work/web",
                "2026-09-26T10:00:00Z",
            ),
            agent(
                "b",
                "Add tests",
                "running",
                "/work/api",
                "2026-09-26T11:00:00Z",
            ),
            agent("c", "Refactor", "idle", "/work/web", "2026-09-26T09:00:00Z"),
        ]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(outline(&entries), ["# api", "- b", "# web", "- a", "- c"]);
        let filtered = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "login");
        assert!(filtered.iter().any(|entry| matches!(
            entry,
            SidebarEntry::Agent { agent_id, highlight_positions, .. }
                if agent_id == "a" && highlight_positions == &vec![4, 5, 6, 7, 8]
        )));
        assert_eq!(outline(&filtered), ["# web", "- a"]);
    }

    #[test]
    fn highlight_positions_stay_on_character_boundaries() {
        assert_eq!(title_match_positions("Café fix", "é"), Some(vec![3]));
        assert_eq!(
            title_match_positions("Café fix", "fix"),
            Some(vec![6, 7, 8])
        );
        assert_eq!(
            title_match_positions("İstanbul", "stan"),
            Some(vec![2, 3, 4, 5])
        );
        assert_eq!(title_match_positions("Café", "x"), None);
    }

    #[test]
    fn a_lone_agent_takes_its_workspace_name_everywhere() {
        let fixture = Fixture::new(vec![
            placed(
                agent(
                    "a",
                    "for cortex i need",
                    "running",
                    "/w",
                    "2026-09-26T10:00:00Z",
                ),
                "wks_alone",
            ),
            placed(
                agent("b", "Audit reflex", "idle", "/w", "2026-09-26T09:00:00Z"),
                "wks_shared",
            ),
            placed(
                agent("c", "Audit synapse", "idle", "/w", "2026-09-26T08:00:00Z"),
                "wks_shared",
            ),
            agent("d", "Unplaced", "idle", "/w", "2026-09-26T07:00:00Z"),
        ])
        .with_workspaces(vec![
            workspace("wks_alone", "prj_axon", "Design Cortex testing tool"),
            workspace("wks_shared", "prj_axon", "saanu-audit-fixes"),
        ]);
        let title = |id: &str| {
            let agent = fixture
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("agent");
            agent_display_title(&fixture.agents, &fixture.workspaces, agent)
        };
        assert_eq!(title("a"), "Design Cortex testing tool");
        assert_eq!(title("b"), "Audit reflex");
        assert_eq!(title("d"), "Unplaced");
        let lone_workspace = |id: &str| {
            let agent = fixture
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("agent");
            crate::store::lone_agent_workspace(&fixture.agents, &fixture.workspaces, agent)
                .map(|workspace| workspace.id.as_str())
        };
        assert_eq!(lone_workspace("a"), Some("wks_alone"));
        assert_eq!(lone_workspace("b"), None);
        assert_eq!(lone_workspace("d"), None);
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "cortex testing");
        assert_eq!(outline(&entries), ["# Working", "> wks_alone = a"]);
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "i need");
        assert_eq!(
            outline(&entries),
            ["# Working", "> wks_alone = a"],
            "the agent's own title still finds it"
        );
    }

    #[test]
    fn status_rows_follow_the_last_message_not_every_update() {
        let message = |mut agent: AgentSummary, at: &str| {
            agent.extra["lastUserMessageAt"] = json!(at);
            agent
        };
        // "reporting" reported last, but the user messaged "asked" more recently.
        let fixture = Fixture::new(vec![
            message(
                agent(
                    "reporting",
                    "Reporting",
                    "running",
                    "/w",
                    "2026-09-26T12:00:00Z",
                ),
                "2026-09-26T09:00:00Z",
            ),
            message(
                agent("asked", "Asked", "running", "/w", "2026-09-26T11:00:00Z"),
                "2026-09-26T10:00:00Z",
            ),
        ]);
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "");
        assert_eq!(outline(&entries), ["# Working", "- asked", "- reporting"]);
    }

    #[test]
    fn status_groups_follow_paseo_order_and_collapse() {
        let mut fixture = Fixture::new(vec![
            agent("a", "Done", "idle", "/w", "2026-09-26T10:00:00Z"),
            agent("b", "Busy", "running", "/w", "2026-09-26T11:00:00Z"),
            agent("c", "Ask", "idle", "/w", "2026-09-26T09:00:00Z"),
        ]);
        fixture.pending = HashSet::from(["c".to_owned()]);
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            ["# Needs input", "- c", "# Working", "- b", "# Done", "- a"]
        );
        let collapsed = HashSet::from([format!("status:{}", AgentBucket::Running as u8)]);
        let entries = fixture.entries(SidebarGrouping::Status, &collapsed, "");
        assert_eq!(
            outline(&entries),
            ["# Needs input", "- c", "# Working", "# Done", "- a"]
        );
    }

    #[test]
    fn status_grouping_lists_each_workspace_once_under_its_most_urgent_agent() {
        let fixture = Fixture::new(vec![
            placed(
                agent("a", "First ask", "idle", "/w", "2026-09-26T10:00:00Z"),
                "wks_cortex",
            ),
            placed(
                agent("b", "yo", "running", "/w", "2026-09-26T11:00:00Z"),
                "wks_cortex",
            ),
            placed(
                agent("c", "Alone", "idle", "/w", "2026-09-26T09:00:00Z"),
                "wks_alone",
            ),
            agent("d", "Unplaced", "idle", "/w", "2026-09-26T08:00:00Z"),
        ])
        .with_workspaces(vec![
            workspace("wks_cortex", "prj", "Design Cortex testing tool"),
            workspace("wks_alone", "prj", "Alone workspace"),
        ]);
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            [
                "# Working",
                "> wks_cortex",
                "# Done",
                "> wks_alone = c",
                "- d"
            ]
        );
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "yo");
        assert_eq!(
            outline(&entries),
            ["# Working", "> wks_cortex"],
            "an agent's title still finds its workspace"
        );
        let entries = fixture.entries(SidebarGrouping::Status, &HashSet::new(), "cortex");
        assert_eq!(outline(&entries), ["# Working", "> wks_cortex"]);
    }

    #[test]
    fn sidebar_nests_agents_under_workspaces() {
        let fixture = Fixture::new(vec![
            placed(
                agent(
                    "a",
                    "Fix login",
                    "idle",
                    "/work/web",
                    "2026-09-26T10:00:00Z",
                ),
                "wks_login",
            ),
            placed(
                agent("b", "Review", "idle", "/work/web", "2026-09-26T12:00:00Z"),
                "wks_review",
            ),
            placed(
                agent(
                    "c",
                    "Retry login",
                    "idle",
                    "/work/web",
                    "2026-09-26T11:00:00Z",
                ),
                "wks_login",
            ),
        ])
        .with_workspaces(vec![
            workspace("wks_login", "prj_web", "Login"),
            workspace("wks_review", "prj_web", "Review"),
            workspace("wks_idle", "prj_web", "Terminal only"),
        ])
        .with_projects(vec![project("prj_web", "web", "/work/web")]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            [
                "# web",
                "> wks_review = b",
                "> wks_login",
                "  - c",
                "  - a",
                "> wks_idle",
            ]
        );
        let collapsed = HashSet::from([workspace_collapse_key("wks_login")]);
        let entries = fixture.entries(SidebarGrouping::Project, &collapsed, "");
        assert_eq!(
            outline(&entries),
            ["# web", "> wks_review = b", "> wks_login", "> wks_idle"]
        );
        let filtered = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "retry");
        assert_eq!(outline(&filtered), ["# web", "> wks_login = c"]);
        let by_name = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "terminal");
        assert_eq!(outline(&by_name), ["# web", "> wks_idle"]);
    }

    #[test]
    fn empty_workspace_row_is_muted_with_no_agents() {
        let mut empty = workspace("wks_empty", "prj_web", "Empty");
        empty.current_branch = Some("main".into());
        empty.activity_at = Some("2026-09-26T10:00:00Z".into());
        let (details, muted) = workspace_row_details(&empty, 0, None, true, None);
        assert!(muted);
        assert_eq!(details.note.as_deref(), Some("No agents"));
        assert_eq!(details.branch, None);
        assert_eq!(details.timestamp, None);
        let (details, muted) = workspace_row_details(&empty, 1, None, true, Some("1h".into()));
        assert!(!muted);
        assert_eq!(details.note, None);
        assert_eq!(details.branch.as_deref(), Some("main"));
    }

    #[test]
    fn agent_row_is_not_selected_while_history_is_active() {
        assert!(!agent_row_selected(Some("a"), None, "a"));
        assert!(!agent_row_selected(Some("a"), Some("b"), "a"));
        assert!(!agent_row_selected(Some("b"), Some("a"), "a"));
        assert!(agent_row_selected(Some("a"), Some("a"), "a"));
    }

    #[test]
    fn single_agent_workspace_is_one_row() {
        let fixture = Fixture::new(vec![
            placed(
                agent("a", "Solo", "idle", "/work/web", "2026-09-26T10:00:00Z"),
                "wks_solo",
            ),
            placed(
                agent("b", "First", "idle", "/work/web", "2026-09-26T11:00:00Z"),
                "wks_pair",
            ),
            placed(
                agent("c", "Second", "idle", "/work/web", "2026-09-26T09:00:00Z"),
                "wks_pair",
            ),
        ])
        .with_workspaces(vec![
            workspace("wks_solo", "prj_web", "Solo"),
            workspace("wks_pair", "prj_web", "Pair"),
        ])
        .with_projects(vec![project("prj_web", "web", "/work/web")]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            ["# web", "> wks_pair", "  - b", "  - c", "> wks_solo = a"]
        );
        assert!(entries.iter().any(|entry| matches!(
            entry,
            SidebarEntry::Workspace { workspace_id, single_agent: Some(agent_id), .. }
                if workspace_id == "wks_solo" && agent_id == "a"
        )));
        assert!(!entries.iter().any(|entry| matches!(
            entry,
            SidebarEntry::Agent { agent_id, .. } if agent_id == "a"
        )));
    }

    #[test]
    fn agent_order_includes_single_agent_rows() {
        let fixture = Fixture::new(vec![
            placed(
                agent("a", "Solo", "idle", "/work/web", "2026-09-26T12:00:00Z"),
                "wks_solo",
            ),
            placed(
                agent("b", "First", "idle", "/work/web", "2026-09-26T11:00:00Z"),
                "wks_pair",
            ),
            placed(
                agent("c", "Second", "idle", "/work/web", "2026-09-26T09:00:00Z"),
                "wks_pair",
            ),
            agent("d", "Loose", "idle", "/work/web", "2026-09-26T08:00:00Z"),
        ])
        .with_workspaces(vec![
            workspace("wks_solo", "prj_web", "Solo"),
            workspace("wks_pair", "prj_web", "Pair"),
        ])
        .with_projects(vec![project("prj_web", "web", "/work/web")]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(agent_order(&entries), ["a", "b", "c", "d"]);
    }

    #[test]
    fn only_changed_rows_are_measured_again() {
        let row = |id: &str| SidebarEntry::Agent {
            agent_id: id.into(),
            nested: false,
            highlight_positions: Vec::new(),
        };
        let rows = |ids: &[&str]| ids.iter().map(|id| row(id)).collect::<Vec<_>>();
        let before = rows(&["a", "b", "c", "d"]);
        assert_eq!(changed_rows(&before, &before), None);
        assert_eq!(
            changed_rows(&before, &rows(&["a", "c", "b", "d"])),
            Some((1..3, 2)),
            "a swap re-measures only the swapped rows"
        );
        assert_eq!(
            changed_rows(&before, &rows(&["a", "b", "c", "d", "e"])),
            Some((4..4, 1))
        );
        assert_eq!(
            changed_rows(&before, &rows(&["b", "c", "d"])),
            Some((0..1, 0))
        );
        assert_eq!(changed_rows(&before, &[]), Some((0..4, 0)));
        assert_eq!(
            changed_rows(&rows(&["a", "a"]), &rows(&["a"])),
            Some((1..2, 0)),
            "a shared row counts once, not as both a prefix and a suffix"
        );
    }

    #[test]
    fn number_keys_follow_the_visible_workspace_rows() {
        let fixture = Fixture::new(vec![
            placed(
                agent("a", "Solo", "idle", "/work/web", "2026-09-26T12:00:00Z"),
                "wks_solo",
            ),
            placed(
                agent("b", "First", "idle", "/work/web", "2026-09-26T11:00:00Z"),
                "wks_pair",
            ),
            placed(
                agent("c", "Second", "idle", "/work/web", "2026-09-26T09:00:00Z"),
                "wks_pair",
            ),
            agent("d", "Loose", "idle", "/work/web", "2026-09-26T08:00:00Z"),
        ])
        .with_workspaces(vec![
            workspace("wks_solo", "prj_web", "Solo"),
            workspace("wks_pair", "prj_web", "Pair"),
        ])
        .with_projects(vec![project("prj_web", "web", "/work/web")]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(
            number_key_targets(&entries),
            [
                NumberKeyTarget::Agent("a".into()),
                NumberKeyTarget::Workspace("wks_pair".into()),
                NumberKeyTarget::Agent("d".into()),
            ],
            "{:?}",
            outline(&entries)
        );
    }

    #[test]
    fn pinned_workspaces_come_first() {
        let mut pinned = workspace("wks_pinned", "prj_api", "Pinned work");
        pinned.pinned_at = Some("2026-09-26T08:00:00Z".into());
        let fixture = Fixture::new(vec![placed(
            agent("a", "Busy", "running", "/work/web", "2026-09-26T12:00:00Z"),
            "wks_web",
        )])
        .with_workspaces(vec![pinned, workspace("wks_web", "prj_web", "Web")])
        .with_projects(vec![
            project("prj_web", "web", "/work/web"),
            project("prj_api", "api", "/work/api"),
            project("prj_empty", "empty", "/work/empty"),
        ]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            [
                "# Pinned",
                "> wks_pinned",
                "# web",
                "> wks_web = a",
                "# empty"
            ],
            "a project with only a pinned workspace isn't listed again as empty"
        );
    }

    #[test]
    fn agents_without_workspace_fall_back_to_directory() {
        let fixture = Fixture::new(vec![
            agent(
                "a",
                "Old daemon",
                "idle",
                "/work/web",
                "2026-09-26T10:00:00Z",
            ),
            agent(
                "b",
                "Elsewhere",
                "idle",
                "/tmp/scratch",
                "2026-09-26T09:00:00Z",
            ),
            placed(
                agent(
                    "c",
                    "Unknown workspace",
                    "idle",
                    "/work/web",
                    "2026-09-26T08:00:00Z",
                ),
                "wks_gone",
            ),
        ])
        .with_projects(vec![project("prj_web", "web", "/work/web")]);
        let entries = fixture.entries(SidebarGrouping::Project, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            ["# web", "- a", "- c", "# scratch", "- b"]
        );
    }

    #[test]
    fn label_grouping_lists_a_workspace_under_each_label() {
        let mut both = workspace("wks_both", "prj_web", "Both");
        both.labels = vec!["review".into(), "urgent".into()];
        let mut fixture = Fixture::new(Vec::new())
            .with_workspaces(vec![both, workspace("wks_plain", "prj_web", "Plain")]);
        fixture.labels = vec![
            WorkspaceLabel {
                name: "review".into(),
                color: "sky".into(),
            },
            WorkspaceLabel {
                name: "urgent".into(),
                color: "red".into(),
            },
            WorkspaceLabel {
                name: "unused".into(),
                color: "teal".into(),
            },
        ];
        let entries = fixture.entries(SidebarGrouping::Labels, &HashSet::new(), "");
        assert_eq!(
            outline(&entries),
            [
                "# review",
                "> wks_both",
                "# urgent",
                "> wks_both",
                "# No label",
                "> wks_plain",
            ]
        );
    }
}
