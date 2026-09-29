use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, SharedString, Task, WeakEntity, Window, prelude::*,
};
use picker::{Picker, PickerDelegate};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::composer::{BaseRef, Composer, base_ref_choices};
use crate::sidebar::provider_icon;
use crate::store::{
    AgentBucket, agent_bucket, agent_project_directory, agent_project_name, agent_provider,
    agent_title, agent_updated_at,
};
use crate::{
    ArchiveAgent, CopyAgentId, CycleMode, FocusComposer, ManageHosts, NewAgent, OpenWorkspace,
    Reconnect, RenameAgent, ToggleArchived, ToggleGroupByStatus, ToggleModePicker,
    ToggleModelPicker, TogglePanel, ToggleThinkingPicker, open_agent, store,
};

enum CommandTarget {
    Action(Box<dyn Action>),
    Agent(String),
    Terminal {
        info: paseo_client::TerminalInfo,
        directory: String,
    },
}

struct CommandEntry {
    label: String,
    detail: Option<String>,
    icon: IconName,
    icon_color: Color,
    target: CommandTarget,
    is_agent: bool,
}

pub(crate) fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.weak_entity();
    let previous_focus = window.focused(cx);
    let current_agent = crate::current_agent_id(workspace, cx);
    workspace.toggle_modal(window, cx, move |window, cx| {
        CommandCenter::new(workspace_handle, previous_focus, current_agent, window, cx)
    });
}

pub struct CommandCenter {
    picker: Entity<Picker<CommandCenterDelegate>>,
}

impl CommandCenter {
    fn new(
        workspace: WeakEntity<Workspace>,
        previous_focus: Option<FocusHandle>,
        current_agent: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = command_entries(current_agent, cx);
        let delegate = CommandCenterDelegate {
            command_center: cx.weak_entity(),
            workspace,
            previous_focus,
            candidates: Arc::new(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        StringMatchCandidate::new(
                            index,
                            &format!("{} {}", entry.label, entry.detail.as_deref().unwrap_or("")),
                        )
                    })
                    .collect(),
            ),
            entries,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::list(delegate, window, cx));
        Self { picker }
    }
}

fn command_entries(current_agent: Option<String>, cx: &App) -> Vec<CommandEntry> {
    let action = |label: &str, icon: IconName, action: Box<dyn Action>| CommandEntry {
        label: label.to_owned(),
        detail: None,
        icon,
        icon_color: Color::Muted,
        target: CommandTarget::Action(action),
        is_agent: false,
    };
    let mut entries = vec![
        action("New agent", IconName::Plus, Box::new(NewAgent)),
        action(
            "Focus message input",
            IconName::Chat,
            Box::new(FocusComposer),
        ),
        action(
            "Change model",
            IconName::Sparkle,
            Box::new(ToggleModelPicker),
        ),
        action(
            "Change thinking effort",
            IconName::ToolThink,
            Box::new(ToggleThinkingPicker),
        ),
        action("Change mode", IconName::Lock, Box::new(ToggleModePicker)),
        action("Cycle mode", IconName::ArrowCircle, Box::new(CycleMode)),
        action("Rename agent", IconName::Pencil, Box::new(RenameAgent)),
        action("Archive agent", IconName::Archive, Box::new(ArchiveAgent)),
        action("Copy agent ID", IconName::Copy, Box::new(CopyAgentId)),
        action(
            "Fork agent",
            IconName::GitBranchPlus,
            Box::new(crate::ForkAgent),
        ),
        action(
            "Open project in editor",
            IconName::FolderOpen,
            Box::new(OpenWorkspace),
        ),
        action(
            "Toggle sidebar",
            IconName::ThreadsSidebarLeftOpen,
            Box::new(TogglePanel),
        ),
        action(
            "Group sidebar by project or status",
            IconName::ListTree,
            Box::new(ToggleGroupByStatus),
        ),
        action(
            "Show archived agents",
            IconName::Archive,
            Box::new(ToggleArchived),
        ),
        action(
            "Review last turn's changes",
            IconName::FileDiff,
            Box::new(crate::ReviewLastTurn),
        ),
        action(
            "New terminal",
            IconName::Terminal,
            Box::new(crate::NewTerminal),
        ),
        action("Dictate", IconName::Mic, Box::new(crate::ToggleDictation)),
        action(
            "Provider usage",
            IconName::Sparkle,
            Box::new(crate::OpenProviderUsage),
        ),
        action("Manage hosts", IconName::Server, Box::new(ManageHosts)),
        action("Reconnect to host", IconName::RotateCw, Box::new(Reconnect)),
    ];
    if let Some(directory) = crate::terminal::agent_directory(current_agent, cx) {
        for info in crate::terminal::terminals_for(&directory, cx) {
            entries.push(CommandEntry {
                label: crate::terminal::terminal_title(&info),
                detail: Some("Terminal".into()),
                icon: IconName::Terminal,
                icon_color: Color::Muted,
                target: CommandTarget::Terminal {
                    info,
                    directory: directory.clone(),
                },
                is_agent: false,
            });
        }
    }
    let store = store(cx);
    let store = store.read(cx);
    let mut agents = store.state.agents.iter().collect::<Vec<_>>();
    agents.sort_by_key(|agent| std::cmp::Reverse(agent_updated_at(agent)));
    for agent in agents {
        let has_permission = store
            .state
            .permissions
            .values()
            .any(|request| request.agent_id == agent.id);
        let bucket = agent_bucket(agent, has_permission);
        entries.push(CommandEntry {
            label: agent_title(agent),
            detail: Some(format!(
                "{} · {}",
                agent_project_name(agent),
                bucket.label()
            )),
            icon: provider_icon(agent_provider(agent)),
            icon_color: match bucket {
                AgentBucket::NeedsInput => Color::Warning,
                AgentBucket::Failed => Color::Error,
                AgentBucket::Running => Color::Info,
                AgentBucket::Attention => Color::Success,
                AgentBucket::Done => Color::Muted,
            },
            target: CommandTarget::Agent(agent.id.clone()),
            is_agent: true,
        });
    }
    entries
}

impl Render for CommandCenter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PaseoCommandCenter")
            .w(rems(40.))
            .child(self.picker.clone())
    }
}

impl Focusable for CommandCenter {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for CommandCenter {}
impl ModalView for CommandCenter {}

pub struct CommandCenterDelegate {
    command_center: WeakEntity<CommandCenter>,
    workspace: WeakEntity<Workspace>,
    previous_focus: Option<FocusHandle>,
    entries: Vec<CommandEntry>,
    candidates: Arc<Vec<StringMatchCandidate>>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl PickerDelegate for CommandCenterDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "Paseo command center"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search commands and agents…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        self.matches
            .windows(2)
            .enumerate()
            .filter_map(|(index, pair)| {
                let first = self.entries.get(pair[0].candidate_id)?;
                let second = self.entries.get(pair[1].candidate_id)?;
                (first.is_agent != second.is_agent).then_some(index)
            })
            .collect()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self.candidates.clone();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.trim().is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string.clone(),
                        positions: Vec::new(),
                        score: 0.,
                    })
                    .collect::<Vec<_>>()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    200,
                    &Default::default(),
                    background,
                )
                .await
            };
            if let Err(error) = picker.update(cx, |picker, cx| {
                picker.delegate.matches = matches;
                picker.delegate.selected_index = 0;
                cx.notify();
            }) {
                log::debug!("Paseo command center closed: {error}");
            }
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|matched| self.entries.get(matched.candidate_id))
        else {
            return;
        };
        match &entry.target {
            CommandTarget::Agent(agent_id) => {
                let agent_id = agent_id.clone();
                if let Some(workspace) = self.workspace.upgrade() {
                    self.dismissed(window, cx);
                    workspace.update(cx, |workspace, cx| {
                        open_agent(workspace, &agent_id, true, window, cx);
                    });
                    return;
                }
            }
            CommandTarget::Terminal { info, directory } => {
                let (info, directory) = (info.clone(), directory.clone());
                if let Some(workspace) = self.workspace.upgrade() {
                    self.dismissed(window, cx);
                    workspace.update(cx, |workspace, cx| {
                        crate::terminal::open_terminal(workspace, info, directory, window, cx);
                    });
                    return;
                }
            }
            CommandTarget::Action(action) => {
                let action = action.boxed_clone();
                let previous_focus = self.previous_focus.clone();
                self.dismissed(window, cx);
                window.defer(cx, move |window, cx| {
                    if let Some(previous_focus) = previous_focus {
                        window.focus(&previous_focus, cx);
                    }
                    window.dispatch_action(action, cx);
                });
                return;
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Err(error) = self
            .command_center
            .update(cx, |_, cx| cx.emit(DismissEvent))
        {
            log::debug!("Paseo command center closed: {error}");
        }
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matched = self.matches.get(index)?;
        let entry = self.entries.get(matched.candidate_id)?;
        let label_positions = matched
            .positions
            .iter()
            .copied()
            .filter(|position| *position < entry.label.len())
            .collect::<Vec<_>>();
        let binding = match &entry.target {
            CommandTarget::Action(action) => self
                .previous_focus
                .as_ref()
                .map(|focus| KeyBinding::for_action_in(action.as_ref(), focus, cx)),
            CommandTarget::Agent(_) | CommandTarget::Terminal { .. } => None,
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(entry.icon)
                        .size(IconSize::Small)
                        .color(entry.icon_color),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(HighlightedLabel::new(entry.label.clone(), label_positions))
                        .when_some(entry.detail.clone(), |this, detail| {
                            this.child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        }),
                )
                .end_slot::<KeyBinding>(binding),
        )
    }
}

pub(crate) fn choose_directory(
    workspace: &mut Workspace,
    composer: WeakEntity<Composer>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    pick_directory(
        workspace,
        "Search directories, or type an absolute path…",
        move |directory, window, cx| {
            if let Err(error) = composer.update(cx, |composer, cx| {
                composer.set_draft_directory(directory, cx);
                composer.focus(window, cx);
            }) {
                log::debug!("Paseo composer closed: {error}");
            }
        },
        window,
        cx,
    );
}

/// Picks a directory on the Paseo host: recent project directories first, then the host's
/// suggestions for what is typed, or any typed absolute path.
pub(crate) fn pick_directory(
    workspace: &mut Workspace,
    placeholder: &'static str,
    on_choose: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)> = Rc::new(on_choose);
    let mut recent = BTreeSet::new();
    if store(cx).read(cx).is_local_host() {
        for worktree in workspace.project().read(cx).visible_worktrees(cx) {
            recent.insert(worktree.read(cx).abs_path().to_path_buf());
        }
    }
    {
        let store = store(cx);
        let store = store.read(cx);
        let mut agents = store.state.agents.iter().collect::<Vec<_>>();
        agents.sort_by_key(|agent| std::cmp::Reverse(agent_updated_at(agent)));
        for agent in agents {
            if let Some(directory) = agent_project_directory(agent) {
                recent.insert(directory);
            }
        }
    }
    let recent = recent.into_iter().collect::<Vec<_>>();
    workspace.toggle_modal(window, cx, move |window, cx| {
        DirectoryPicker::new(placeholder, on_choose, recent, window, cx)
    });
}

pub struct DirectoryPicker {
    picker: Entity<Picker<DirectoryPickerDelegate>>,
}

impl DirectoryPicker {
    fn new(
        placeholder: &'static str,
        on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)>,
        recent: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = DirectoryPickerDelegate {
            directory_picker: cx.weak_entity(),
            placeholder,
            on_choose,
            recent: recent.clone(),
            paths: recent,
            query: String::new(),
            selected_index: 0,
            error: None,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for DirectoryPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PaseoDirectoryPicker")
            .w(rems(36.))
            .child(self.picker.clone())
    }
}

impl Focusable for DirectoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for DirectoryPicker {}
impl ModalView for DirectoryPicker {}

pub struct DirectoryPickerDelegate {
    directory_picker: WeakEntity<DirectoryPicker>,
    placeholder: &'static str,
    on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)>,
    recent: Vec<PathBuf>,
    paths: Vec<PathBuf>,
    query: String,
    selected_index: usize,
    error: Option<SharedString>,
}

impl PickerDelegate for DirectoryPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "Paseo project directory"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(
            self.error
                .clone()
                .unwrap_or_else(|| "No matching directories".into()),
        )
    }

    fn match_count(&self) -> usize {
        self.paths.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        if query.trim().is_empty() {
            self.paths = self.recent.clone();
            self.selected_index = 0;
            cx.notify();
            return Task::ready(());
        }
        let lower = query.to_lowercase();
        let local = self
            .recent
            .iter()
            .filter(|path| path.to_string_lossy().to_lowercase().contains(&lower))
            .cloned()
            .collect::<Vec<_>>();
        let suggestions = store(cx).update(cx, |store, cx| {
            store.directory_suggestions(query.clone(), None, false, true, cx)
        });
        cx.spawn_in(window, async move |picker, cx| {
            let (remote, error) = match suggestions.await {
                Ok(suggestions) => (
                    suggestions
                        .into_iter()
                        .map(|suggestion| PathBuf::from(suggestion.path))
                        .collect::<Vec<_>>(),
                    None,
                ),
                Err(error) => (Vec::new(), Some(SharedString::from(error.to_string()))),
            };
            if let Err(error) = picker.update(cx, |picker, cx| {
                if picker.delegate.query != query {
                    return;
                }
                let mut paths = local;
                for path in remote {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                picker.delegate.paths = paths;
                picker.delegate.error = error;
                picker.delegate.selected_index = 0;
                cx.notify();
            }) {
                log::debug!("Paseo directory picker closed: {error}");
            }
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let chosen = self.paths.get(self.selected_index).cloned().or_else(|| {
            let typed = self.query.trim();
            paseo_client::is_absolute_workspace_path(typed).then(|| PathBuf::from(typed))
        });
        self.dismissed(window, cx);
        if let Some(directory) = chosen {
            (self.on_choose)(directory, window, cx);
        }
    }

    fn confirm_input(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let typed = self.query.trim().to_owned();
        if paseo_client::is_absolute_workspace_path(&typed) {
            self.dismissed(window, cx);
            (self.on_choose)(PathBuf::from(typed), window, cx);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Err(error) = self
            .directory_picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
        {
            log::debug!("Paseo directory picker closed: {error}");
        }
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let path = self.paths.get(index)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::Folder)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex().gap_2().child(Label::new(name)).child(
                        Label::new(path.to_string_lossy().into_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                ),
        )
    }
}

const BRANCH_SUGGESTION_LIMIT: usize = 50;

/// Paseo's "Base" picker: which branch a new worktree branches off.
pub(crate) fn choose_worktree_base(
    workspace: &mut Workspace,
    composer: WeakEntity<Composer>,
    directory: String,
    selected: Option<BaseRef>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = BaseBranchPickerDelegate {
            base_branch_picker: cx.weak_entity(),
            composer,
            directory,
            selected,
            choices: Vec::new(),
            query: String::new(),
            selected_index: 0,
            error: None,
            loading: true,
        };
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(30.)));
        BaseBranchPicker { picker }
    });
}

pub struct BaseBranchPicker {
    picker: Entity<Picker<BaseBranchPickerDelegate>>,
}

impl Render for BaseBranchPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PaseoBaseBranchPicker")
            .child(self.picker.clone())
    }
}

impl Focusable for BaseBranchPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for BaseBranchPicker {}
impl ModalView for BaseBranchPicker {}

pub struct BaseBranchPickerDelegate {
    base_branch_picker: WeakEntity<BaseBranchPicker>,
    composer: WeakEntity<Composer>,
    directory: String,
    selected: Option<BaseRef>,
    choices: Vec<BaseRef>,
    query: String,
    selected_index: usize,
    error: Option<SharedString>,
    loading: bool,
}

impl PickerDelegate for BaseBranchPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "Paseo worktree base"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Branch off…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.loading {
            "Loading branches…".into()
        } else {
            self.error
                .clone()
                .unwrap_or_else(|| "No matching branches".into())
        })
    }

    fn match_count(&self) -> usize {
        self.choices.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        let directory = self.directory.clone();
        let suggestions = store(cx).update(cx, |store, cx| {
            let query = query.clone();
            store.session_request(cx, move |session| async move {
                session
                    .branch_suggestions(&directory, &query, BRANCH_SUGGESTION_LIMIT)
                    .await
            })
        });
        cx.spawn_in(window, async move |picker, cx| {
            let result = suggestions.await;
            if let Err(error) = picker.update(cx, |picker, cx| {
                let delegate = &mut picker.delegate;
                if delegate.query != query {
                    return;
                }
                delegate.loading = false;
                match result {
                    Ok(suggestions) => {
                        // The current base stays reachable while browsing, but a search shows
                        // only what matches.
                        let pinned = query
                            .trim()
                            .is_empty()
                            .then_some(delegate.selected.as_ref())
                            .flatten();
                        delegate.choices = base_ref_choices(&suggestions, pinned);
                        delegate.error = None;
                    }
                    Err(error) => {
                        delegate.choices = Vec::new();
                        delegate.error = Some(error.to_string().into());
                    }
                }
                delegate.selected_index = 0;
                cx.notify();
            }) {
                log::debug!("Paseo base branch picker closed: {error}");
            }
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(base) = self.choices.get(self.selected_index).cloned() {
            if let Err(error) = self.composer.update(cx, |composer, cx| {
                composer.set_worktree_base(base, cx);
                composer.focus(window, cx);
            }) {
                log::debug!("Paseo composer closed: {error}");
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Err(error) = self
            .base_branch_picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
        {
            log::debug!("Paseo base branch picker closed: {error}");
        }
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = self.choices.get(index)?;
        let is_current = self
            .selected
            .as_ref()
            .is_some_and(|current| current.ref_name == choice.ref_name);
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::GitBranch)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(choice.label.clone()).truncate())
                        .children(choice.detail.clone().map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                )
                .when(is_current, |item| {
                    item.end_slot(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                }),
        )
    }
}
