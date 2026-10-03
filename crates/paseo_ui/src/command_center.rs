use command_palette_hooks::CommandInterceptItem;
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
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::composer::{BaseRef, Composer, base_ref_choices};
use crate::sidebar::WorkspaceAgentCounts;
use crate::store::{
    PaseoStore, agent_bucket, agent_project_directory, agent_project_name, agent_updated_at,
};
use crate::{
    ArchiveAgent, CopyAgentId, CycleMode, FocusComposer, ManageHosts, NewAgent, OpenHistory,
    OpenWorkspace, Reconnect, RenameAgent, ToggleGroupByStatus, ToggleModePicker,
    ToggleModelPicker, TogglePanel, ToggleThinkingPicker,
};

/// A Paseo result for Zed's command palette: the text shown and matched, and what it does.
struct PaletteEntry {
    label: String,
    action: Box<dyn Action>,
    /// An agent or terminal, as opposed to a command.
    is_agent_or_terminal: bool,
}

/// How many agents and terminals one query lists, so they don't bury the commands.
const MAX_PLACE_RESULTS: usize = 8;

/// Adds Paseo to Zed's command palette: for a typed query, matching agents, the current agent's
/// terminals, and Paseo's commands under their own names, which then stand in for the palette's
/// generic "paseo ui: …" entries.
pub(crate) fn init_palette_source(cx: &mut App) {
    command_palette_hooks::CommandPaletteSources::add(cx, palette_results);
}

fn palette_results(
    query: &str,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Task<Vec<CommandInterceptItem>> {
    let query = query.trim().to_owned();
    if query.is_empty() {
        return Task::ready(recent_agent_results(cx));
    }
    let current_agent = workspace
        .upgrade()
        .and_then(|workspace| crate::current_agent(workspace.read(cx), cx));
    let entries = palette_entries(current_agent, cx);
    let candidates = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| StringMatchCandidate::new(index, &entry.label))
        .collect::<Vec<_>>();
    let background = cx.background_executor().clone();
    cx.spawn(async move |_| {
        let matches = match_strings(
            &candidates,
            &query,
            false,
            true,
            200,
            &Default::default(),
            background,
        )
        .await;
        let matches = matches
            .into_iter()
            .filter(|matched| {
                entries
                    .get(matched.candidate_id)
                    .is_some_and(|entry| contains_every_word(&entry.label, &query))
            })
            .collect();
        pick_matches(&entries, matches)
            .into_iter()
            .map(|(entry, positions)| CommandInterceptItem {
                action: entry.action.boxed_clone(),
                string: entry.label.clone(),
                positions,
            })
            .collect()
    })
}

/// Whether every word of `query` appears in `label`, ignoring case. The palette lists these
/// results above its own matches whatever their score, so a loose fuzzy match must not qualify.
fn contains_every_word(label: &str, query: &str) -> bool {
    let label = label.to_lowercase();
    query
        .split_whitespace()
        .all(|word| label.contains(&word.to_lowercase()))
}

/// The best matches in order, with at most [`MAX_PLACE_RESULTS`] agents and terminals.
fn pick_matches(
    entries: &[PaletteEntry],
    matches: Vec<StringMatch>,
) -> Vec<(&PaletteEntry, Vec<usize>)> {
    let mut places = 0;
    matches
        .into_iter()
        .filter_map(|matched| {
            let entry = entries.get(matched.candidate_id)?;
            if entry.is_agent_or_terminal {
                places += 1;
                if places > MAX_PLACE_RESULTS {
                    return None;
                }
            }
            Some((entry, matched.positions))
        })
        .collect()
}

/// The most recently active agents, which the palette lists above its commands before anything is
/// typed.
fn recent_agent_results(cx: &App) -> Vec<CommandInterceptItem> {
    agent_entries(Some(MAX_PLACE_RESULTS), cx)
        .into_iter()
        .map(|entry| CommandInterceptItem {
            action: entry.action,
            string: entry.label,
            positions: Vec::new(),
        })
        .collect()
}

fn palette_entries(
    current_agent: Option<(Entity<PaseoStore>, String)>,
    cx: &App,
) -> Vec<PaletteEntry> {
    let action = |label: &str, action: Box<dyn Action>| PaletteEntry {
        label: label.to_owned(),
        action,
        is_agent_or_terminal: false,
    };
    let mut entries = vec![
        action("New agent", Box::new(NewAgent)),
        action("New workspace", Box::new(crate::NewAgentWorkspace)),
        action("Open history", Box::new(OpenHistory)),
        action("Focus message input", Box::new(FocusComposer)),
        action("Change model", Box::new(ToggleModelPicker)),
        action("Change thinking effort", Box::new(ToggleThinkingPicker)),
        action("Change mode", Box::new(ToggleModePicker)),
        action("Cycle mode", Box::new(CycleMode)),
        action("Rename agent", Box::new(RenameAgent)),
        action("Archive agent", Box::new(ArchiveAgent)),
        action("Copy agent ID", Box::new(CopyAgentId)),
        action("Fork agent", Box::new(crate::ForkAgent)),
        action("Open workspace in editor", Box::new(OpenWorkspace)),
        action("Toggle sidebar", Box::new(TogglePanel)),
        action(
            "Group sidebar by project or status",
            Box::new(ToggleGroupByStatus),
        ),
        action(
            "Review last turn's changes",
            Box::new(crate::ReviewLastTurn),
        ),
        action("New terminal", Box::new(crate::NewTerminal)),
        action("Dictate", Box::new(crate::ToggleDictation)),
        action("Provider usage", Box::new(crate::OpenProviderUsage)),
        action("Manage hosts", Box::new(ManageHosts)),
        action("Reconnect to host", Box::new(Reconnect)),
    ];
    if let Some((store, directory)) = crate::terminal::agent_directory(current_agent, cx) {
        for info in crate::terminal::terminals_for(&store, &directory, cx) {
            entries.push(PaletteEntry {
                label: format!("Terminal: {}", crate::terminal::terminal_title(&info)),
                action: Box::new(crate::OpenPaseoTerminal {
                    directory: directory.clone(),
                    terminal_id: info.id.clone(),
                }),
                is_agent_or_terminal: true,
            });
        }
    }
    entries.extend(agent_entries(None, cx));
    entries
}

/// Every host's agents, most recently active first, or only the first `limit` of them.
fn agent_entries(limit: Option<usize>, cx: &App) -> Vec<PaletteEntry> {
    let stores = crate::hosts::stores(cx);
    let hosts = stores
        .iter()
        .map(|store| {
            let store = store.read(cx);
            (
                store,
                WorkspaceAgentCounts::new(store.state.agents()),
                crate::attention::pending_permission_agents(store),
            )
        })
        .collect::<Vec<_>>();
    let agents = hosts
        .iter()
        .enumerate()
        .flat_map(|(host, (store, _, _))| {
            store.state.agents().iter().map(move |agent| (host, agent))
        })
        .collect::<Vec<_>>();
    most_recent_first(agents, limit, |(_, agent)| agent_updated_at(agent))
        .into_iter()
        .filter_map(|(host, agent)| {
            let (store, titles, pending) = hosts.get(host)?;
            Some(PaletteEntry {
                label: format!(
                    "Agent: {} — {} · {}",
                    titles.display_title(&store.state.workspaces, agent),
                    agent_project_name(agent),
                    agent_bucket(agent, pending.contains(agent.id.as_str())).label()
                ),
                action: Box::new(crate::OpenAgentById {
                    agent_id: agent.id.clone(),
                }),
                is_agent_or_terminal: true,
            })
        })
        .collect()
}

/// `items` by `updated_at`, latest first and in their listed order on ties, keeping only the first
/// `limit` when there is one. A limit picks those before sorting them, so the rest are never
/// sorted.
fn most_recent_first<T>(
    items: Vec<T>,
    limit: Option<usize>,
    updated_at: impl Fn(&T) -> Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<T> {
    // The listed position breaks ties, so picking the first few matches a stable sort's order.
    let mut keyed = items
        .into_iter()
        .enumerate()
        .map(|(position, item)| ((std::cmp::Reverse(updated_at(&item)), position), item))
        .collect::<Vec<_>>();
    if let Some(limit) = limit
        && limit < keyed.len()
    {
        if limit == 0 {
            return Vec::new();
        }
        keyed.select_nth_unstable_by_key(limit - 1, |(key, _)| *key);
        keyed.truncate(limit);
    }
    keyed.sort_unstable_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, item)| item).collect()
}

pub(crate) fn choose_directory(
    workspace: &mut Workspace,
    composer: WeakEntity<Composer>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store = composer
        .upgrade()
        .map(|composer| composer.read(cx).store.clone())
        .unwrap_or_else(|| crate::hosts::default_store(cx));
    pick_directory(
        workspace,
        store,
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

/// Picks a directory on `store`'s host: recent project directories first, then the host's
/// suggestions for what is typed, or any typed absolute path.
pub(crate) fn pick_directory(
    workspace: &mut Workspace,
    store: Entity<PaseoStore>,
    placeholder: &'static str,
    on_choose: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)> = Rc::new(on_choose);
    let recent = recent_directories(workspace, &store, cx);
    workspace.toggle_modal(window, cx, move |window, cx| {
        DirectoryPicker::new(store, placeholder, on_choose, recent, window, cx)
    });
}

/// The editor's folders on the local host, then the folders of `store`'s agents.
pub(crate) fn recent_directories(
    workspace: &Workspace,
    store: &Entity<PaseoStore>,
    cx: &App,
) -> Vec<PathBuf> {
    let mut recent = BTreeSet::new();
    let store = store.read(cx);
    if store.is_local_host() {
        for worktree in workspace.project().read(cx).visible_worktrees(cx) {
            recent.insert(worktree.read(cx).abs_path().to_path_buf());
        }
    }
    let mut agents = store.state.agents().iter().collect::<Vec<_>>();
    agents.sort_by_cached_key(|agent| std::cmp::Reverse(agent_updated_at(agent)));
    for agent in agents {
        if let Some(directory) = agent_project_directory(agent) {
            recent.insert(directory);
        }
    }
    recent.into_iter().collect()
}

pub struct DirectoryPicker {
    picker: Entity<Picker<DirectoryPickerDelegate>>,
}

impl DirectoryPicker {
    pub(crate) fn new(
        store: Entity<PaseoStore>,
        placeholder: &'static str,
        on_choose: Rc<dyn Fn(PathBuf, &mut Window, &mut App)>,
        recent: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = DirectoryPickerDelegate {
            store,
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
    store: Entity<PaseoStore>,
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
        let suggestions = self.store.update(cx, |store, cx| {
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
    let store = composer
        .upgrade()
        .map(|composer| composer.read(cx).store.clone())
        .unwrap_or_else(|| crate::hosts::default_store(cx));
    workspace.toggle_modal(window, cx, move |window, cx| {
        BaseBranchPicker::new(store, composer, directory, selected, window, cx)
    });
}

pub struct BaseBranchPicker {
    picker: Entity<Picker<BaseBranchPickerDelegate>>,
}

impl BaseBranchPicker {
    /// Picks the base branch for `composer`'s new worktree from `directory`'s branches.
    pub(crate) fn new(
        store: Entity<PaseoStore>,
        composer: WeakEntity<Composer>,
        directory: String,
        selected: Option<BaseRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = BaseBranchPickerDelegate {
            store,
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
        Self { picker }
    }
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
    store: Entity<PaseoStore>,
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
        let suggestions = self.store.update(cx, |store, cx| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_results_need_every_typed_word() {
        assert!(contains_every_word(
            "Agent: Fix login — zaseo · Done",
            "fix log"
        ));
        assert!(contains_every_word("Rename agent", "AGENT ren"));
        // A scattered subsequence would jump above Zed's own commands.
        assert!(!contains_every_word(
            "Agent: Improve composer footer — zaseo · Done",
            "open"
        ));
    }

    #[test]
    fn palette_lists_a_few_agents_and_terminals_but_every_command() {
        let entry = |label: &str, is_agent_or_terminal: bool| PaletteEntry {
            label: label.to_owned(),
            action: Box::new(NewAgent),
            is_agent_or_terminal,
        };
        let mut entries = (0..MAX_PLACE_RESULTS + 3)
            .map(|index| entry(&format!("Agent: {index}"), true))
            .collect::<Vec<_>>();
        entries.push(entry("Rename agent", false));
        let matches = (0..entries.len())
            .map(|candidate_id| StringMatch {
                candidate_id,
                string: String::new(),
                positions: vec![candidate_id],
                score: 1.,
            })
            .collect();
        let picked = pick_matches(&entries, matches);
        assert_eq!(
            picked
                .iter()
                .filter(|(entry, _)| entry.is_agent_or_terminal)
                .count(),
            MAX_PLACE_RESULTS
        );
        assert_eq!(
            picked.last().map(|(entry, _)| entry.label.as_str()),
            Some("Rename agent")
        );
        assert_eq!(
            picked.first().map(|(_, positions)| positions.clone()),
            Some(vec![0])
        );
    }

    #[gpui::test]
    fn empty_query_lists_recent_agents_first(cx: &mut gpui::TestAppContext) {
        use settings::Settings as _;
        let store = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::PaseoSettings::register(cx);
            crate::hosts::init(cx);
            crate::hosts::default_store(cx)
        });
        let agents = (0..MAX_PLACE_RESULTS + 2)
            .map(|index| paseo_client::AgentSummary {
                title: Some(format!("Task {index}")),
                ..crate::store::test_agent(
                    &format!("agent-{index}"),
                    "idle",
                    serde_json::json!({ "updatedAt": format!("2026-10-01T10:{index:02}:00Z") }),
                )
            })
            .collect();
        store.update(cx, |store, cx| {
            store.handle_event(paseo_client::PaseoEvent::AgentsChanged(agents), cx)
        });
        let labels = cx.update(|cx| {
            recent_agent_results(cx)
                .into_iter()
                .map(|item| item.string)
                .collect::<Vec<_>>()
        });
        assert_eq!(labels.len(), MAX_PLACE_RESULTS);
        assert!(labels[0].starts_with("Agent: Task 9"), "{labels:?}");
        assert!(labels[7].starts_with("Agent: Task 2"), "{labels:?}");
    }

    #[test]
    fn picking_the_most_recent_matches_a_full_sort() {
        let time = |minute: u32| {
            crate::timeline::parse_timestamp(&format!("2026-10-01T10:{minute:02}:00Z"))
        };
        // Ties and agents without a time keep their listed order, as a stable sort keeps them.
        let items = [3, 7, 7, 1, 9, 0, 7, 4, 2, 9, 5]
            .into_iter()
            .enumerate()
            .map(|(position, minute)| (position, (minute > 0).then(|| time(minute)).flatten()))
            .collect::<Vec<_>>();
        let mut sorted = items.clone();
        sorted.sort_by_key(|(_, updated_at)| std::cmp::Reverse(*updated_at));
        for limit in [0, 1, 4, MAX_PLACE_RESULTS, items.len(), items.len() + 2] {
            let picked =
                most_recent_first(items.clone(), Some(limit), |(_, updated_at)| *updated_at);
            assert_eq!(
                picked,
                sorted.iter().take(limit).cloned().collect::<Vec<_>>(),
                "limit {limit}"
            );
        }
        assert_eq!(
            most_recent_first(items, None, |(_, updated_at)| *updated_at),
            sorted
        );
    }
}
