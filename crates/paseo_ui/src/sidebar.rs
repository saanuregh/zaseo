use anyhow::Result;
use chrono::{DateTime, Utc};
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    Action as _, Animation, AnimationExt as _, AnyElement, App, AppContext as _,
    AsyncWindowContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Global, IntoElement, Pixels, ScrollHandle, Subscription, Task, WeakEntity, Window, ease_in_out,
    prelude::*, pulsating_between, px, relative,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use paseo_client::{AgentSummary, ProjectDescriptor, WorkspaceDescriptor, WorkspaceLabel};
use settings::Settings as _;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ui::{
    ContextMenu, HighlightedLabel, IconButton, Indicator, KeyBinding, PopoverMenu, Tooltip,
    prelude::*, right_click_menu,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::store::{
    AgentBucket, ConnectionStatus, PaseoStore, StoreEvent, agent_branch, agent_bucket,
    agent_display_title, agent_project_directory, agent_project_key, agent_project_name,
    agent_provider, agent_requires_attention, agent_title, agent_updated_at, agent_workspace_id,
    agent_worktree_name,
};
use crate::stream::{ENTRANCE, fade_in_since, rotating_chevron};
use crate::timeline::{format_relative, parse_timestamp};
use crate::workspace_tools;
use crate::{
    ArchiveAgent, CopyAgentId, FocusSidebarFilter, ManageHosts, NewAgentWorkspace, OpenWorkspace,
    PaseoSettings, Reconnect, RenameAgent, ToggleGroupByStatus, TogglePanel, auto_connect,
    client_id_for, connection_picker, open_agent, open_draft, store,
};

const GROUPING_KEY: &str = "paseo_sidebar_group_by_status";

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

fn matches_filter(agent: &AgentSummary, inputs: &SidebarInputs) -> Option<Vec<usize>> {
    let filter = inputs.filter;
    if filter.is_empty() {
        return Some(Vec::new());
    }
    let title = agent_display_title(inputs.agents, inputs.workspaces, agent);
    let lower_filter = filter.to_lowercase();
    if let Some(positions) = title_match_positions(&title, &lower_filter) {
        return Some(positions);
    }
    let project = agent_project_name(agent).to_lowercase();
    (project.contains(&lower_filter)
        || agent_title(agent).to_lowercase().contains(&lower_filter)
        || agent_provider(agent).contains(&lower_filter)
        || agent.id.starts_with(filter))
    .then(Vec::new)
}

/// Byte offsets of the title characters matching a lowercase filter. Offsets come from the original
/// title so they stay on character boundaries even when lowercasing changes byte lengths.
pub(crate) fn title_match_positions(title: &str, lower_filter: &str) -> Option<Vec<usize>> {
    let filter_length = lower_filter.chars().count();
    title.char_indices().find_map(|(start, _)| {
        let rest = title.get(start..)?;
        rest.to_lowercase().starts_with(lower_filter).then(|| {
            rest.char_indices()
                .take(filter_length)
                .map(|(offset, _)| start + offset)
                .collect()
        })
    })
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
    pub agents: &'a [AgentSummary],
    pub workspaces: &'a BTreeMap<String, WorkspaceDescriptor>,
    pub projects: &'a BTreeMap<String, ProjectDescriptor>,
    pub labels: &'a [WorkspaceLabel],
    pub pending_permission_agents: &'a HashSet<String>,
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

/// Paseo's label palette, named after Tailwind colors; these are their 500 shades.
pub(crate) fn label_color(name: &str) -> gpui::Hsla {
    let hex = match name {
        "violet" => 0x8b5cf6,
        "sky" => 0x0ea5e9,
        "emerald" => 0x10b981,
        "orange" => 0xf97316,
        "pink" => 0xec4899,
        "indigo" => 0x6366f1,
        "teal" => 0x14b8a6,
        "red" => 0xef4444,
        "amber" => 0xf59e0b,
        _ => 0x3b82f6,
    };
    gpui::rgb(hex).into()
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
    let mut sorted = inputs.agents.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|agent| std::cmp::Reverse(agent_updated_at(agent)));
    match inputs.grouping {
        SidebarGrouping::Status => status_entries(&sorted, inputs),
        SidebarGrouping::Project | SidebarGrouping::Labels => workspace_entries(&sorted, inputs),
    }
}

/// One row per workspace, as Paseo lists them, under its most urgent agent's state, so opening
/// another agent in a workspace neither adds a row nor retitles the ones there. Agents outside
/// any workspace keep rows of their own.
fn status_entries(sorted: &[&AgentSummary], inputs: &SidebarInputs) -> Vec<SidebarEntry> {
    struct StatusWorkspace<'a> {
        bucket: AgentBucket,
        agents: Vec<&'a AgentSummary>,
        matched: bool,
    }
    let lower_filter = inputs.filter.to_lowercase();
    enum StatusRow<'a> {
        Workspace(&'a str),
        Agent(&'a AgentSummary, AgentBucket, Vec<usize>),
    }
    // Rows in order of their most recent agent; a workspace's slot is its first agent's.
    let mut order: Vec<StatusRow> = Vec::new();
    let mut workspaces: HashMap<&str, StatusWorkspace> = HashMap::new();
    for agent in sorted {
        let bucket = agent_bucket(agent, inputs.pending_permission_agents.contains(&agent.id));
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
                workspace.matched |= matches_filter(agent, inputs).is_some();
            }
            None => {
                if let Some(highlights) = matches_filter(agent, inputs) {
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
                        title_match_positions(&descriptor.name, &lower_filter)
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

fn workspace_entries(sorted: &[&AgentSummary], inputs: &SidebarInputs) -> Vec<SidebarEntry> {
    let lower_filter = inputs.filter.to_lowercase();
    let mut groups: BTreeMap<&str, WorkspaceGroup> = BTreeMap::new();
    let mut unplaced: Vec<(&AgentSummary, Vec<usize>)> = Vec::new();
    for workspace in inputs.workspaces.values() {
        let highlight_positions = if inputs.filter.is_empty() {
            Some(Vec::new())
        } else {
            title_match_positions(&workspace.name, &lower_filter)
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
                let highlights =
                    matches_filter(agent, inputs).or_else(|| name_matches(group).then(Vec::new));
                if let Some(highlights) = highlights {
                    group.agents.push((agent, highlights));
                }
            }
            None => {
                if let Some(highlights) = matches_filter(agent, inputs) {
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
    fn for_agent(agent: &AgentSummary, has_pending_permission: bool) -> Option<Self> {
        Self::for_bucket(agent_bucket(agent, has_pending_permission))
    }

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
    project: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    diff_stat: Option<paseo_client::DiffStat>,
    timestamp: Option<String>,
}

impl RowDetails {
    /// The details as one line of small parts after `icon`, wrapping when the sidebar is narrow.
    fn render(self, icon: AnyElement) -> AnyElement {
        let separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(Color::Placeholder)
                .into_any_element()
        };
        let muted = |text: String| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        let shrinking = |text: String| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate()
                .into_any_element()
        };
        // A long worktree or branch name ends in an ellipsis rather than running past the
        // sidebar, since a wrapped line can't be narrower than one unbreakable part.
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
                    this.child(div().flex_shrink_0().child(muted(worktree)))
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
        let diff_stat = self.diff_stat.map(|stat| {
            h_flex()
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
                .into_any_element()
        });
        let joined = |icon: Option<AnyElement>, parts: Vec<AnyElement>| {
            let mut line = h_flex()
                .w_full()
                .min_w_0()
                .flex_wrap()
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
        let place = [self.project.map(muted), checkout]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let change = [diff_stat, self.timestamp.map(muted)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        // A worktree name makes the line long, so where it works and what changed get a line
        // each, and a wrap never starts a line with a separator.
        if self.worktree.is_some() && !change.is_empty() {
            v_flex()
                .min_w_0()
                .child(joined(Some(icon), place))
                .child(joined(None, change))
                .into_any_element()
        } else {
            joined(Some(icon), place.into_iter().chain(change).collect()).into_any_element()
        }
    }
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
}

impl SidebarRow {
    fn render(self, cx: &App) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
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
            Label::new(self.title).color(title_color).into_any_element()
        } else {
            HighlightedLabel::new(self.title, self.highlight_positions)
                .color(title_color)
                .into_any_element()
        };
        let details = self.details.render(self.icon);
        h_flex()
            .id(self.id)
            .w_full()
            .min_h(px(28.))
            .py_1p5()
            .px_2p5()
            .gap_1p5()
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
            .hover(|style| style.bg(colors.ghost_element_hover))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    // Two lines, then an ellipsis, so a long title reads without taking over
                    // the list.
                    .child(
                        div()
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
        .left_2()
        .right_2()
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

fn project_sections(
    mut groups: Vec<WorkspaceGroup>,
    unplaced: Vec<(&AgentSummary, Vec<usize>)>,
    inputs: &SidebarInputs,
) -> Vec<SidebarEntry> {
    groups.sort_by_key(|group| std::cmp::Reverse(group.activity));
    let (pinned, groups): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .partition(|group| group.workspace.pinned_at.is_some());
    let mut sections: Vec<ProjectSection> = Vec::new();
    let section_index = |sections: &mut Vec<ProjectSection>,
                         key: String,
                         label: String,
                         directory: Option<PathBuf>,
                         project_id: Option<String>| {
        match sections.iter().position(|section| section.key == key) {
            Some(index) => index,
            None => {
                sections.push(ProjectSection {
                    key,
                    label,
                    directory,
                    project_id,
                    workspaces: Vec::new(),
                    agents: Vec::new(),
                    activity: None,
                });
                sections.len() - 1
            }
        }
    };
    for group in groups {
        let project = inputs.projects.get(&group.workspace.project_id);
        let index = section_index(
            &mut sections,
            format!("project:{}", group.workspace.project_id),
            project
                .map(project_label)
                .unwrap_or_else(|| group.workspace.project_display_name.clone()),
            Some(group.workspace.project_root_path.clone()),
            Some(group.workspace.project_id.clone()),
        );
        let section = &mut sections[index];
        section.activity = section.activity.max(group.activity);
        section.workspaces.push(group);
    }
    for (agent, highlights) in unplaced {
        let directory = agent_project_directory(agent);
        let project = inputs
            .projects
            .values()
            .find(|project| directory.as_ref() == Some(&project.root_path));
        let index = match project {
            Some(project) => section_index(
                &mut sections,
                format!("project:{}", project.id),
                project_label(project),
                Some(project.root_path.clone()),
                Some(project.id.clone()),
            ),
            None => section_index(
                &mut sections,
                format!("directory:{}", agent_project_key(agent)),
                agent_project_name(agent),
                agent.directory.clone(),
                None,
            ),
        };
        let section = &mut sections[index];
        section.activity = section.activity.max(agent_updated_at(agent));
        section.agents.push((agent, highlights));
    }
    if inputs.filter.is_empty() {
        let mut empty = inputs
            .projects
            .values()
            .filter(|project| {
                !sections
                    .iter()
                    .any(|section| section.project_id.as_deref() == Some(project.id.as_str()))
                    && !pinned
                        .iter()
                        .any(|group| group.workspace.project_id == project.id)
            })
            .collect::<Vec<_>>();
        empty.sort_by_key(|project| project_label(project).to_lowercase());
        for project in empty {
            section_index(
                &mut sections,
                format!("project:{}", project.id),
                project_label(project),
                Some(project.root_path.clone()),
                Some(project.id.clone()),
            );
        }
    }
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
}

impl Global for SharedSidebarView {}

pub struct PaseoPanel {
    store: Entity<PaseoStore>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    filter: Entity<Editor>,
    position: DockPosition,
    grouping: SidebarGrouping,
    collapsed: HashSet<String>,
    selected: Option<usize>,
    entries: Vec<SidebarEntry>,
    scroll_handle: ScrollHandle,
    pointer_inside: bool,
    refresh_pending: bool,
    motion: RowMotion,
    _subscriptions: Vec<Subscription>,
}

/// More rows than this appearing at once is a load, so they appear without easing in.
const MAX_ANIMATED_APPEARANCES: usize = 3;

/// How long a row's flash takes to fade after its agent changes state.
const STATE_FLASH: Duration = Duration::from_millis(900);

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
    fade_in_since(
        frame,
        SharedString::from(format!("paseo-row-appear-{key}")),
        motion.appeared_at.get(key).copied(),
        px(0.),
    )
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
        let store = store(cx);
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search agents", window, cx);
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
                        .ok()
                        .flatten()
                        .map(|value| SidebarGrouping::from_stored(&value))
                })
                .unwrap_or_default();
            cx.set_global(SharedSidebarView {
                grouping,
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
            cx.observe(&store, |panel: &mut Self, _, cx| panel.store_changed(cx)),
            cx.subscribe(&store, |_: &mut Self, _, event: &StoreEvent, cx| {
                if matches!(event, StoreEvent::FocusChanged) {
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
            store,
            workspace,
            focus_handle: cx.focus_handle(),
            filter,
            position: DockPosition::Left,
            grouping: shared.grouping,
            collapsed: shared.collapsed,
            selected: None,
            entries: Vec::new(),
            scroll_handle: ScrollHandle::new(),
            pointer_inside: false,
            refresh_pending: false,
            motion: RowMotion::default(),
            _subscriptions: subscriptions,
        };
        panel.observe_motion(cx);
        panel.refresh_entries(cx);
        panel
    }

    /// Rows moving under the pointer cause misclicks, so store-driven reorders wait until the
    /// pointer leaves. Rows read the store at render time, so their content stays live.
    fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.observe_motion(cx);
        if self.pointer_inside {
            self.refresh_pending = true;
            cx.notify();
        } else {
            self.refresh_entries(cx);
        }
    }

    fn observe_motion(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let mut rows = HashMap::new();
        for agent in &store.state.agents {
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
        self.motion.observe(rows, Instant::now());
    }

    fn shared_view_changed(&mut self, cx: &mut Context<Self>) {
        let shared = cx.global::<SharedSidebarView>().clone();
        self.grouping = shared.grouping;
        self.collapsed = shared.collapsed;
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
        self.refresh_pending = false;
        let previously_selected = self
            .selected
            .and_then(|selected| self.entries.get(selected))
            .cloned();
        let filter = self.filter.read(cx).text(cx);
        let store = self.store.read(cx);
        let pending = store
            .state
            .permissions
            .values()
            .map(|request| request.agent_id.clone())
            .collect::<HashSet<_>>();
        self.entries = build_entries(&SidebarInputs {
            agents: &store.state.agents,
            workspaces: &store.state.workspaces,
            projects: &store.state.projects,
            labels: &store.state.labels,
            pending_permission_agents: &pending,
            grouping: self.grouping,
            collapsed: &self.collapsed,
            filter: filter.trim(),
        });
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
        cx.notify();
    }

    /// Active agents in sidebar order, each once: grouped by label, an agent can be listed under
    /// several labels.
    pub(crate) fn agent_order(&self) -> Vec<String> {
        agent_order(&self.entries)
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
            self.scroll_handle.scroll_to_item(selected);
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
        if let Some(agent_id) = self.selected_agent() {
            self.store
                .update(cx, |store, cx| store.archive(&agent_id, cx));
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
                connection_picker::open_rename(workspace, agent_id, window, cx);
            });
        }
    }

    fn copy_selected_id(&mut self, _: &CopyAgentId, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(agent_id) = self.selected_agent() {
            cx.write_to_clipboard(ClipboardItem::new_string(agent_id));
        }
    }

    fn new_agent_in(
        &mut self,
        directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| match directory {
                Some(directory) => {
                    crate::open_draft_in(workspace, directory, window, cx).detach_and_log_err(cx);
                }
                None => {
                    open_draft(workspace, None, window, cx);
                }
            });
        }
    }

    fn render_host_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let status = store.status;
        let profile_name = store
            .active_profile
            .as_ref()
            .map(|profile| profile.name.clone())
            .or_else(|| {
                PaseoSettings::get_global(cx)
                    .active()
                    .map(|profile| profile.name.clone())
            })
            .unwrap_or_else(|| "No host".into());
        let (dot, tooltip) = match status {
            ConnectionStatus::Connected => (Color::Success, "Connected"),
            ConnectionStatus::Connecting => (Color::Warning, "Connecting…"),
            ConnectionStatus::Reconnecting => (Color::Warning, "Reconnecting…"),
            ConnectionStatus::Disconnected => (Color::Error, "Disconnected"),
        };
        let profiles = PaseoSettings::get_global(cx).profiles.clone();
        let store_handle = self.store.clone();
        PopoverMenu::new("paseo-host-menu")
            .trigger_with_tooltip(
                ui::Button::new("paseo-host-button", profile_name)
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
                let profiles = profiles.clone();
                let store_handle = store_handle.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    menu = menu.header("Hosts");
                    let active = store_handle
                        .read(cx)
                        .active_profile
                        .as_ref()
                        .map(|profile| profile.name.clone());
                    for profile in profiles {
                        let store_handle = store_handle.clone();
                        let is_active = active.as_deref() == Some(profile.name.as_str());
                        let profile_for_click = profile.clone();
                        menu = menu.toggleable_entry(
                            profile.name.clone(),
                            is_active,
                            ui::IconPosition::End,
                            None,
                            move |_, cx| {
                                let mut profile = profile_for_click.clone();
                                profile.client_id = client_id_for(&profile, cx);
                                store_handle.update(cx, |store, cx| {
                                    let generation = store.begin_connection(profile.clone());
                                    store.connect(profile, None, generation, cx);
                                });
                            },
                        );
                    }
                    let disconnect_store = store_handle;
                    menu.separator()
                        .action("Manage Hosts…", ManageHosts.boxed_clone())
                        .action("Provider Usage", crate::OpenProviderUsage.boxed_clone())
                        .action("Daemon Status", crate::OpenDaemonStatus.boxed_clone())
                        .action("Reconnect", Reconnect.boxed_clone())
                        .entry("Disconnect", None, move |_, cx| {
                            disconnect_store.update(cx, |store, cx| store.disconnect(cx));
                        })
                }))
            })
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        let group_focus = self.focus_handle.clone();
        h_flex()
            .h(px(36.))
            .flex_none()
            .px_2()
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
                                    "Search commands and agents",
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
                        self.store.clone(),
                        self.workspace.clone(),
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
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
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
                    menu.separator()
                        .action("Add Project…", crate::AddProject.boxed_clone())
                        .action(
                            "New Project Directory…",
                            crate::NewProjectDirectory.boxed_clone(),
                        )
                }))
            })
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
            .gap_1()
            .child(
                h_flex()
                    .id("paseo-new-workspace")
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(
                        cx.listener(|panel, _, window, cx| panel.new_agent_in(None, window, cx)),
                    )
                    .child(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("New workspace"))
                    .child(div().flex_1())
                    .child(
                        KeyBinding::for_action_in(&NewAgentWorkspace, &focus, cx)
                            .size(rems_from_px(11_f32)),
                    ),
            )
            .child(
                h_flex()
                    .id("paseo-history")
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(self.history_open(cx), |this| this.bg(colors.element_active))
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(crate::OpenHistory.boxed_clone(), cx)
                    })
                    .child(
                        Icon::new(IconName::HistoryRerun)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("History")),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .px_2()
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
        panel_focused: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let keyboard_selected = panel_focused && self.selected == Some(index);
        match entry {
            SidebarEntry::Header {
                key,
                label,
                count,
                collapsed,
                directory,
                color,
                project_id,
            } => {
                let project_id = project_id.clone();
                let project_icon = project_id.as_ref().and_then(|project_id| {
                    self.store
                        .read(cx)
                        .project_icons
                        .get(project_id)
                        .and_then(|(_, image)| image.clone())
                });
                let group = format!("paseo-group-{index}");
                let empty_project = *count == 0 && directory.is_some();
                let label_color = color.as_deref().map(label_color);
                let entry = entry.clone();
                let directory = directory.clone();
                let header = h_flex()
                    .id(("paseo-sidebar-header", index))
                    .group(group.clone())
                    .h(px(28.))
                    .mt_2()
                    .px_2()
                    .gap_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if keyboard_selected {
                        colors.panel_focused_border
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|style| style.bg(colors.ghost_element_hover))
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
                        this.child(
                            div().visible_on_hover(group.clone()).child(
                                IconButton::new(("paseo-group-new", index), IconName::Plus)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("New agent in this project"))
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        cx.stop_propagation();
                                        panel.new_agent_in(Some(directory.clone()), window, cx)
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
                    })
                    .into_any_element();
                match project_id {
                    Some(project_id) => {
                        let workspace = self.workspace.clone();
                        right_click_menu(("paseo-project-menu", index))
                            .trigger(move |_, _, _| header)
                            .menu(move |window, cx| {
                                let workspace = workspace.clone();
                                let project_id = project_id.clone();
                                ContextMenu::build(window, cx, move |menu, _, cx| {
                                    workspace_tools::project_menu(
                                        menu,
                                        &project_id,
                                        workspace.clone(),
                                        cx,
                                    )
                                })
                            })
                            .into_any_element()
                    }
                    None => header,
                }
            }
            SidebarEntry::Workspace {
                workspace_id,
                collapsed,
                agent_count,
                highlight_positions,
                single_agent,
            } => self.render_workspace(
                index,
                workspace_id,
                *collapsed,
                *agent_count,
                highlight_positions.clone(),
                single_agent.clone(),
                keyboard_selected,
                cx,
            ),
            SidebarEntry::Agent {
                agent_id,
                nested,
                highlight_positions,
            } => {
                let row = self.render_agent(
                    index,
                    agent_id,
                    *nested,
                    highlight_positions.clone(),
                    keyboard_selected,
                    cx,
                );
                if *nested {
                    div().pl_4().child(row).into_any_element()
                } else {
                    row
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_workspace(
        &self,
        index: usize,
        workspace_id: &str,
        collapsed: bool,
        agent_count: usize,
        highlight_positions: Vec<usize>,
        single_agent: Option<String>,
        keyboard_selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(workspace) = store.state.workspaces.get(workspace_id) else {
            return div().into_any_element();
        };
        let single_agent_summary = single_agent
            .as_deref()
            .and_then(|agent_id| store.agent(agent_id));
        // One state, most urgent first, so a row never shows two competing signals.
        let alert = match single_agent_summary {
            Some(agent) => {
                let has_permission = store
                    .state
                    .permissions
                    .values()
                    .any(|request| request.agent_id == agent.id);
                AgentAlert::for_agent(agent, has_permission)
            }
            None => match workspace.status.as_str() {
                "needs_input" => Some(AgentAlert::NeedsInput),
                "failed" => Some(AgentAlert::Failed),
                _ if store.state.agents.iter().any(|agent| {
                    agent_workspace_id(agent) == Some(workspace_id)
                        && agent_requires_attention(agent)
                }) =>
                {
                    Some(AgentAlert::Unread)
                }
                "running" => Some(AgentAlert::Running),
                _ => None,
            },
        };
        let dot = alert.map(AgentAlert::dot_color);
        let is_active = single_agent.is_some() && store.focused_agent == single_agent;
        let timestamp = single_agent_summary
            .and_then(agent_updated_at)
            .or_else(|| workspace.activity_at.as_deref().and_then(parse_timestamp))
            .map(|updated| format_relative(updated, Utc::now()));
        let is_worktree = workspace.kind == "worktree" || workspace.is_paseo_worktree;
        let entry = SidebarEntry::Workspace {
            workspace_id: workspace_id.to_owned(),
            collapsed,
            agent_count,
            highlight_positions: Vec::new(),
            single_agent: single_agent.clone(),
        };
        let branch = workspace.current_branch.clone();
        let diff_stat = workspace
            .diff_stat
            .filter(|stat| stat.additions + stat.deletions > 0);
        let tooltip = workspace.directory.display().to_string();
        let group = format!("paseo-workspace-{index}");
        let collapse_key = workspace_collapse_key(workspace_id);
        let chevron_id = SharedString::from(format!("paseo-workspace-chevron-{collapse_key}"));
        // The status grouping lists a workspace's agents as its tabs, not rows under it.
        let nests_agents = self.grouping != SidebarGrouping::Status;
        let chevron = (nests_agents && agent_count > 0 && single_agent.is_none()).then(|| {
            div()
                .id(("paseo-workspace-collapse", index))
                .pt_0p5()
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
                ))
                .into_any_element()
        });
        let row = SidebarRow {
            id: ("paseo-sidebar-workspace", index).into(),
            icon: Icon::new(if is_worktree {
                IconName::GitBranch
            } else {
                IconName::Folder
            })
            .size(IconSize::XSmall)
            .color(Color::Muted)
            .into_any_element(),
            title: workspace.name.clone().into(),
            highlight_positions,
            title_generating: false,
            details: RowDetails {
                project: (self.grouping != SidebarGrouping::Project)
                    .then(|| workspace.project_display_name.clone()),
                worktree: is_worktree
                    .then(|| workspace.worktree_slug.clone())
                    .flatten(),
                branch,
                diff_stat,
                timestamp,
            },
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
            muted: false,
        }
        .render(cx)
        .group(group)
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(move |panel, _, window, cx| {
            panel.selected = Some(index);
            panel.activate(&entry, false, window, cx)
        }))
        .into_any_element();
        let menu_workspace = self.workspace.clone();
        let menu_workspace_id = workspace_id.to_owned();
        let this = cx.weak_entity();
        let menu_row = right_click_menu(("paseo-workspace-menu", index))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let workspace = menu_workspace.clone();
                let workspace_id = menu_workspace_id.clone();
                let this = this.clone();
                let single_agent = single_agent.clone();
                ContextMenu::build(window, cx, move |menu, _, cx| {
                    let menu =
                        workspace_tools::workspace_menu(menu, &workspace_id, workspace.clone(), cx);
                    let Some(agent_id) = single_agent.clone() else {
                        return menu;
                    };
                    let archive = (this, agent_id);
                    menu.separator().entry("Archive Agent", None, move |_, cx| {
                        let (this, agent_id) = archive.clone();
                        if let Err(error) = this.update(cx, |panel, cx| {
                            panel
                                .store
                                .update(cx, |store, cx| store.archive(&agent_id, cx))
                        }) {
                            log::debug!("Paseo sidebar released: {error}");
                        }
                    })
                })
            })
            .into_any_element();
        let row = alert_line(menu_row, alert, ("paseo-workspace-alert", index), cx);
        let animate = crate::PaseoSettings::get_global(cx).sidebar.animate_status;
        row_motion(
            row,
            &workspace_motion_key(workspace_id),
            &self.motion,
            animate,
            cx,
        )
    }

    fn render_agent(
        &self,
        index: usize,
        agent_id: &str,
        nested: bool,
        highlight_positions: Vec<usize>,
        keyboard_selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(agent) = store.agent(agent_id) else {
            return div().into_any_element();
        };
        let has_permission = store
            .state
            .permissions
            .values()
            .any(|request| request.agent_id == agent_id);
        let bucket = agent_bucket(agent, has_permission);
        let alert = AgentAlert::for_agent(agent, has_permission);
        let timestamp = agent_updated_at(agent)
            .map(|updated| format_relative(updated, Utc::now()))
            .unwrap_or_default();
        let is_active = store.focused_agent.as_deref() == Some(agent_id);
        let title = store.display_title(agent);
        let provider = agent_provider(agent).to_owned();
        let agent_id_owned = agent_id.to_owned();
        let menu_agent = agent_id.to_owned();
        let directory = agent.directory.clone();
        let needs_attention = agent_requires_attention(agent);
        let project_name =
            (self.grouping != SidebarGrouping::Project).then(|| agent_project_name(agent));
        let directory_label = directory
            .as_ref()
            .map(|directory| directory.display().to_string())
            .unwrap_or_default();
        let this = cx.weak_entity();
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
            _ => Icon::new(provider_icon(&provider))
                .size(IconSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
        };
        let item = SidebarRow {
            id: ("paseo-agent", index).into(),
            icon,
            title: title.into(),
            highlight_positions,
            title_generating: agent.title.is_none() && bucket == AgentBucket::Running,
            details: RowDetails {
                project: project_name,
                // A nested agent's workspace row above it already shows where it works.
                worktree: (!nested).then(|| agent_worktree_name(agent)).flatten(),
                branch: (!nested).then(|| agent_branch(agent)).flatten(),
                diff_stat: None,
                timestamp: Some(timestamp),
            },
            trailing: Vec::new(),
            selected: is_active,
            keyboard_selected,
            muted: false,
        }
        .render(cx)
        .tooltip(Tooltip::text(directory_label))
        .on_click(cx.listener(move |panel, _, window, cx| {
            panel.selected = Some(index);
            let entry = SidebarEntry::Agent {
                agent_id: agent_id_owned.clone(),
                nested,
                highlight_positions: Vec::new(),
            };
            panel.activate(&entry, true, window, cx);
        }));
        let menu_row = right_click_menu(("paseo-agent-menu", index))
            .trigger(move |_, _, _| item)
            .menu(move |window, cx| {
                let this = this.clone();
                let agent_id = menu_agent.clone();
                let directory = directory.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let open = (this.clone(), agent_id.clone());
                    let rename = (this.clone(), agent_id.clone());
                    let archive = (this.clone(), agent_id.clone());
                    let new_here = (this.clone(), directory.clone());
                    let copy_id = agent_id.clone();
                    let copy_path = directory.clone();
                    let workspace_agent = (this.clone(), agent_id.clone());
                    let read_agent = (this.clone(), agent_id.clone());
                    let fork = (this.clone(), agent_id.clone());
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
                        .entry("Rename…", None, move |window, cx| {
                            let (this, agent_id) = rename.clone();
                            if let Err(error) =
                                this.update(cx, |panel, cx| panel.rename(agent_id, window, cx))
                            {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry("Fork", None, move |window, cx| {
                            let (this, agent_id) = fork.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                if let Some(workspace) = panel.workspace.upgrade() {
                                    workspace.update(cx, |workspace, cx| {
                                        crate::fork_agent(workspace, &agent_id, window, cx)
                                    });
                                }
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry("New Agent in Project", None, move |window, cx| {
                            let (this, directory) = new_here.clone();
                            if let Err(error) = this
                                .update(cx, |panel, cx| panel.new_agent_in(directory, window, cx))
                            {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .entry("Open in Editor", None, move |window, cx| {
                            let (this, agent_id) = workspace_agent.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                panel.store.update(cx, |store, cx| {
                                    store.set_focused_agent(agent_id, cx);
                                });
                                window.dispatch_action(OpenWorkspace.boxed_clone(), cx);
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        })
                        .when(needs_attention, |menu| {
                            menu.entry("Mark as Read", None, move |_, cx| {
                                let (this, agent_id) = read_agent.clone();
                                if let Err(error) = this.update(cx, |panel, cx| {
                                    panel.store.update(cx, |store, cx| {
                                        store.clear_attention(&agent_id, cx)
                                    })
                                }) {
                                    log::debug!("Paseo sidebar released: {error}");
                                }
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
                        .entry("Archive", None, move |_, cx| {
                            let (this, agent_id) = archive.clone();
                            if let Err(error) = this.update(cx, |panel, cx| {
                                panel
                                    .store
                                    .update(cx, |store, cx| store.archive(&agent_id, cx))
                            }) {
                                log::debug!("Paseo sidebar released: {error}");
                            }
                        });
                    menu
                })
            })
            .into_any_element();
        let row = alert_line(menu_row, alert, ("paseo-agent-alert", index), cx);
        let animate = crate::PaseoSettings::get_global(cx).sidebar.animate_status;
        row_motion(row, agent_id, &self.motion, animate, cx)
    }

    fn render_empty(&self, cx: &Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let status = store.status;
        let connection_error = store.state.error.clone();
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
        v_flex()
            .p_4()
            .gap_2()
            .items_center()
            .child(Label::new(title).color(Color::Muted))
            .child(
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Placeholder),
            )
            .when(status == ConnectionStatus::Disconnected, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            ui::Button::new("paseo-retry", "Retry")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .on_click(|_, _, cx| auto_connect(true, cx)),
                        )
                        .child(
                            ui::Button::new("paseo-hosts", "Hosts…")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(ManageHosts.boxed_clone(), cx)
                                }),
                        ),
                )
            })
            .into_any_element()
    }
}

pub(crate) fn open_agent_at_index(
    workspace: &mut Workspace,
    index: usize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<PaseoPanel>(cx) else {
        return;
    };
    let order = panel.read(cx).agent_order();
    if let Some(agent_id) = index.checked_sub(1).and_then(|index| order.get(index)) {
        let agent_id = agent_id.clone();
        open_agent(workspace, &agent_id, true, window, cx);
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
    let order = panel.read(cx).agent_order();
    if order.is_empty() {
        return;
    }
    let current = store(cx).read(cx).focused_agent.clone();
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

impl Render for PaseoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let entries = self.entries.clone();
        let panel_focused = self.focus_handle.contains_focused(window, cx);
        let has_agents = entries.iter().any(|entry| {
            matches!(entry, SidebarEntry::Agent { .. })
                || matches!(entry, SidebarEntry::Workspace { agent_count, .. } if *agent_count > 0)
        });
        v_flex()
            .id("paseo-sidebar")
            .key_context("PaseoSidebar")
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
                v_flex()
                    .id("paseo-sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .pb_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .when(!has_agents, |this| this.child(self.render_empty(cx)))
                    .children(
                        entries.iter().enumerate().map(|(index, entry)| {
                            self.render_entry(index, entry, panel_focused, cx)
                        }),
                    ),
            )
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PaseoPanel {
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
    use serde_json::json;

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
            AgentAlert::for_agent(&running, true),
            Some(AgentAlert::NeedsInput)
        );
        assert_eq!(
            AgentAlert::for_agent(&running, false),
            Some(AgentAlert::Running)
        );
        assert_eq!(
            AgentAlert::for_agent(&unread, false),
            Some(AgentAlert::Unread)
        );
        assert_eq!(
            AgentAlert::for_agent(&failed, false),
            Some(AgentAlert::Failed)
        );
        assert_eq!(AgentAlert::for_agent(&idle, false), None);
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
            build_entries(&SidebarInputs {
                agents: &self.agents,
                workspaces: &self.workspaces,
                projects: &self.projects,
                labels: &self.labels,
                pending_permission_agents: &self.pending,
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
