use std::collections::{HashMap, hash_map::Entry};
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription};
use settings::{PaseoConnectionProfile, Settings as _, SettingsStore};

use crate::{
    PaseoSettings,
    attention::HostActivity,
    client_id_for,
    store::{ConnectionStatus, PaseoStore, SavedSet, StoreEvent, agent_workspace_id},
};

/// Events from any host's store that app-wide views handle, so they subscribe once instead of
/// tracking hosts as they come and go.
pub(crate) enum HostsEvent {
    NeedsAttention {
        store: Entity<PaseoStore>,
        agent_id: String,
        message: String,
    },
    FocusChanged,
    WorkspaceRemoved {
        workspace_id: String,
        worktree_directory: Option<PathBuf>,
    },
}

pub(crate) struct PaseoHost {
    /// `None` only for the stand-in host kept while no profile is configured.
    pub profile: Option<PaseoConnectionProfile>,
    pub store: Entity<PaseoStore>,
    _subscriptions: [Subscription; 2],
}

impl PaseoHost {
    pub fn name(&self) -> &str {
        self.profile
            .as_ref()
            .map_or("", |profile| profile.name.as_str())
    }
}

/// Every configured Paseo host, each with its own connection and store, in settings order. Like
/// Paseo's app, all of them stay connected at once.
pub(crate) struct PaseoHosts {
    hosts: Vec<PaseoHost>,
    /// The stand-in host's store, kept for the app's lifetime so there is always a default store.
    fallback: Entity<PaseoStore>,
    /// Whether hosts connect when added. Off until startup asks, so tests never reach a daemon.
    connecting: bool,
    /// Passwords typed this session by host name, so a host saved before settings reload it
    /// connects with its password. Never saved.
    passwords: HashMap<String, String>,
    index: HostIndex,
    /// Saved once for the app and shared by every host's store.
    archived_subagents: SavedSet,
    reviewed_edits: SavedSet,
}

/// Which host lists what, rebuilt whenever a store notifies and when hosts change, before the
/// registry notifies its own observers. Lookups across hosts run per tab and per row on every
/// store change, so they read this instead of scanning every host's agents.
///
/// A lookup that misses still scans the hosts: a store changed earlier in the same update hasn't
/// been indexed yet.
#[derive(Default)]
struct HostIndex {
    /// For each host, the earlier host whose daemon it also reaches, matched by server ID.
    duplicate_of: Vec<Option<usize>>,
    /// Each ID's first listed host, so a duplicate resolves to the earlier host.
    agents: HashMap<String, Entity<PaseoStore>>,
    workspaces: HashMap<String, Entity<PaseoStore>>,
    projects: HashMap<String, Entity<PaseoStore>>,
    activity: HostActivity,
}

impl EventEmitter<HostsEvent> for PaseoHosts {}

struct GlobalPaseoHosts(Entity<PaseoHosts>);
impl Global for GlobalPaseoHosts {}

pub(crate) fn init(cx: &mut App) {
    let registry = cx.new(|cx| {
        let archived_subagents = SavedSet::load(crate::store::ARCHIVED_SUBAGENTS_KEY, cx);
        let reviewed_edits = SavedSet::load(crate::store::REVIEWED_EDITS_KEY, cx);
        let stand_in = PaseoHosts::new_host(None, &archived_subagents, &reviewed_edits, cx);
        let mut registry = PaseoHosts {
            fallback: stand_in.store.clone(),
            hosts: vec![stand_in],
            connecting: false,
            passwords: Default::default(),
            index: HostIndex::default(),
            archived_subagents,
            reviewed_edits,
        };
        registry.rebuild_index(cx);
        registry
    });
    cx.set_global(GlobalPaseoHosts(registry));
    sync_with_settings(cx);
    cx.observe_global::<SettingsStore>(sync_with_settings)
        .detach();
}

pub(crate) fn registry(cx: &App) -> Entity<PaseoHosts> {
    cx.global::<GlobalPaseoHosts>().0.clone()
}

/// Starts connecting every host, and every host added later.
pub(crate) fn start_connecting(cx: &mut App) {
    registry(cx).update(cx, |registry, cx| {
        registry.connecting = true;
        registry.connect_all(false, cx);
    });
}

/// Connects one host with a password typed in the hosts modal. A host just saved may not exist
/// until settings reload, so the password also waits for it.
pub(crate) fn connect_host(profile: PaseoConnectionProfile, password: String, cx: &mut App) {
    registry(cx).update(cx, |registry, cx| {
        if !password.is_empty() {
            registry.passwords.insert(profile.name.clone(), password);
        }
        if let Some(index) = registry
            .hosts()
            .iter()
            .position(|host| host.name() == profile.name)
        {
            // Recorded now, so the settings reload that follows the save doesn't connect again.
            if let Some(host) = registry.hosts.get_mut(index) {
                host.profile = Some(profile);
            }
            registry.connect_host_at(index, cx);
        }
    });
}

/// Keeps a renamed host's store, so its open chats stay connected through the rename.
pub(crate) fn rename_host(old_name: &str, new_name: &str, cx: &mut App) {
    registry(cx).update(cx, |registry, _| {
        if let Some(password) = registry.passwords.remove(old_name) {
            registry.passwords.insert(new_name.to_owned(), password);
        }
        if let Some(profile) = registry
            .hosts
            .iter_mut()
            .find(|host| host.name() == old_name)
            .and_then(|host| host.profile.as_mut())
        {
            profile.name = new_name.to_owned();
        }
    });
}

/// Reconnects one host by name, with any password typed for it this session.
pub(crate) fn reconnect_host(name: &str, cx: &mut App) {
    registry(cx).update(cx, |registry, cx| {
        if let Some(index) = registry.hosts().iter().position(|host| host.name() == name) {
            registry.connect_host_at(index, cx);
        }
    });
}

/// Reconnects every host. `force` also restarts hosts that are connected or connecting.
pub(crate) fn connect_all(force: bool, cx: &mut App) {
    registry(cx).update(cx, |registry, cx| registry.connect_all(force, cx));
}

fn sync_with_settings(cx: &mut App) {
    let profiles = PaseoSettings::get_global(cx).profiles.clone();
    registry(cx).update(cx, |registry, cx| registry.sync(profiles, cx));
}

/// Each listed host the user configured, by name, leaving out the stand-in host of an empty
/// profile list. Multi-host UI, such as host pickers and labels, appears only when there are two.
pub(crate) fn configured_hosts(cx: &App) -> Vec<(String, Entity<PaseoStore>)> {
    registry(cx)
        .read(cx)
        .listed()
        .filter(|host| host.profile.is_some())
        .map(|host| (host.name().to_owned(), host.store.clone()))
        .collect()
}

/// The stores of every listed host, skipping hosts that reach a daemon an earlier host already
/// reaches.
pub(crate) fn stores(cx: &App) -> Vec<Entity<PaseoStore>> {
    registry(cx)
        .read(cx)
        .listed()
        .map(|host| host.store.clone())
        .collect()
}

/// How many agents run and need the user across every listed host.
pub(crate) fn activity(cx: &App) -> HostActivity {
    registry(cx).read(cx).index.activity
}

/// The host new agents go to by default: `paseo.active_profile`, else the first host.
pub(crate) fn default_store(cx: &App) -> Entity<PaseoStore> {
    let registry = registry(cx);
    let registry = registry.read(cx);
    registry
        .listed_store_named(&PaseoSettings::get_global(cx).active_profile)
        .or_else(|| registry.listed_store_at(0))
        .unwrap_or_else(|| registry.fallback.clone())
}

/// The host of the agent the user last focused, else the default host, for views about one host
/// such as provider usage and daemon status.
pub(crate) fn current_store(cx: &App) -> Entity<PaseoStore> {
    focused_store(cx).unwrap_or_else(|| default_store(cx))
}

/// The host of the agent most recently focused in a Paseo view; only that host has a focused
/// agent (see [`focus_agent_on`]).
pub(crate) fn focused_store(cx: &App) -> Option<Entity<PaseoStore>> {
    find_store(cx, |store| store.focused_agent.is_some())
}

/// The first host whose daemon runs on this machine, else the default host. Only its agents can
/// work in local editor projects.
pub(crate) fn local_store(cx: &App) -> Entity<PaseoStore> {
    stores(cx)
        .into_iter()
        .find(|store| store.read(cx).is_local_host())
        .unwrap_or_else(|| default_store(cx))
}

/// The store of the host with this profile name, falling back to the default host for names no
/// longer configured.
pub(crate) fn store_named(name: &str, cx: &App) -> Entity<PaseoStore> {
    registry(cx)
        .read(cx)
        .listed_store_named(name)
        .unwrap_or_else(|| default_store(cx))
}

/// The profile name of the host `store` belongs to, or `None` for the stand-in host.
pub(crate) fn host_name(store: &Entity<PaseoStore>, cx: &App) -> Option<String> {
    registry(cx)
        .read(cx)
        .hosts()
        .iter()
        .find(|host| &host.store == store)
        .and_then(|host| host.profile.as_ref())
        .map(|profile| profile.name.clone())
}

/// The host whose daemon knows this agent or subagent timeline. Daemon IDs are random, so they
/// don't repeat across hosts.
pub(crate) fn store_for_agent(agent_id: &str, cx: &App) -> Option<Entity<PaseoStore>> {
    let registry = registry(cx);
    let index = &registry.read(cx).index;
    let indexed = match paseo_client::parse_subagent_timeline_id(agent_id) {
        // A subagent lives on its parent's host, once the parent's subagent list has loaded.
        Some((parent_agent_id, _)) => index
            .agents
            .get(parent_agent_id)
            .filter(|store| store.read(cx).subagent(agent_id).is_some()),
        None => index.agents.get(agent_id),
    };
    indexed.cloned().or_else(|| {
        find_store(cx, |store| {
            store.agent(agent_id).is_some() || store.subagent(agent_id).is_some()
        })
    })
}

/// The host of a Paseo workspace, from its descriptor or, before that loads, its agents.
pub(crate) fn store_for_workspace(workspace_id: &str, cx: &App) -> Option<Entity<PaseoStore>> {
    let registry = registry(cx);
    registry
        .read(cx)
        .index
        .workspaces
        .get(workspace_id)
        .cloned()
        .or_else(|| {
            find_store(cx, |store| {
                store.state.workspaces.contains_key(workspace_id)
                    || store
                        .state
                        .agents()
                        .iter()
                        .any(|agent| agent_workspace_id(agent) == Some(workspace_id))
            })
        })
}

pub(crate) fn store_for_project(project_id: &str, cx: &App) -> Option<Entity<PaseoStore>> {
    let registry = registry(cx);
    registry
        .read(cx)
        .index
        .projects
        .get(project_id)
        .cloned()
        .or_else(|| find_store(cx, |store| store.state.projects.contains_key(project_id)))
}

/// The hosts that duplicate no earlier host's daemon, in settings order.
fn listed_hosts<'a>(
    hosts: &'a [PaseoHost],
    duplicate_of: &'a [Option<usize>],
) -> impl Iterator<Item = &'a PaseoHost> {
    hosts
        .iter()
        .zip(duplicate_of)
        .filter(|(_, original)| original.is_none())
        .map(|(host, _)| host)
}

fn find_store(cx: &App, matches: impl Fn(&PaseoStore) -> bool) -> Option<Entity<PaseoStore>> {
    let registry = registry(cx);
    registry
        .read(cx)
        .listed()
        .find(|host| matches(host.store.read(cx)))
        .map(|host| host.store.clone())
}

/// The host running a daemon terminal. Terminal IDs are random UUIDs.
pub(crate) fn store_for_terminal(terminal_id: &str, cx: &App) -> Option<Entity<PaseoStore>> {
    find_store(cx, |store| {
        store
            .terminals
            .values()
            .flatten()
            .any(|info| info.id == terminal_id)
    })
}

/// The agent most recently focused in a Paseo view, on any host.
pub(crate) fn focused_agent(cx: &App) -> Option<String> {
    let registry = registry(cx);
    registry
        .read(cx)
        .listed()
        .find_map(|host| host.store.read(cx).focused_agent.clone())
}

/// Focuses an agent on its own host and clears focus on the others, so exactly one host
/// has a focused agent.
pub(crate) fn set_focused_agent(agent_id: String, cx: &mut App) {
    if let Some(owner) = store_for_agent(&agent_id, cx) {
        focus_agent_on(&owner, agent_id, cx);
    }
}

/// [`set_focused_agent`] for a caller that knows the agent's host, such as a chat whose agent
/// was just created and isn't listed yet.
pub(crate) fn focus_agent_on(owner: &Entity<PaseoStore>, agent_id: String, cx: &mut App) {
    let owner = Some(owner.clone());
    for store in registry(cx)
        .read(cx)
        .hosts()
        .iter()
        .map(|host| host.store.clone())
        .collect::<Vec<_>>()
    {
        let owns = owner.as_ref() == Some(&store);
        store.update(cx, |store, cx| {
            if owns {
                store.set_focused_agent(agent_id.clone(), cx);
            } else {
                store.clear_focused_agent(cx);
            }
        });
    }
}

impl PaseoHosts {
    pub fn hosts(&self) -> &[PaseoHost] {
        &self.hosts
    }

    /// Hosts in settings order, without those that reach an earlier host's daemon.
    pub fn listed(&self) -> impl Iterator<Item = &PaseoHost> {
        listed_hosts(&self.hosts, &self.index.duplicate_of)
    }

    /// The earlier host whose daemon the host at `index` also reaches, matched by the daemon's
    /// server ID.
    pub fn duplicate_of(&self, index: usize) -> Option<&PaseoHost> {
        let original = (*self.index.duplicate_of.get(index)?)?;
        self.hosts.get(original)
    }

    /// The store of the host named `name`, or of the earlier host it duplicates, so a lookup by
    /// name never lands on a hidden duplicate.
    fn listed_store_named(&self, name: &str) -> Option<Entity<PaseoStore>> {
        let index = self.hosts.iter().position(|host| host.name() == name)?;
        self.listed_store_at(index)
    }

    fn listed_store_at(&self, index: usize) -> Option<Entity<PaseoStore>> {
        let host = self.duplicate_of(index).or_else(|| self.hosts.get(index))?;
        Some(host.store.clone())
    }

    /// Recomputes [`HostIndex`] from every host's store.
    fn rebuild_index(&mut self, cx: &App) {
        let mut first_with_server_id = HashMap::new();
        let duplicate_of = self
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let server_id = host.store.read(cx).server_info.server_id.as_deref()?;
                match first_with_server_id.entry(server_id) {
                    Entry::Occupied(original) => Some(*original.get()),
                    Entry::Vacant(entry) => {
                        entry.insert(index);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        let mut index = HostIndex {
            duplicate_of,
            ..HostIndex::default()
        };
        let listed_stores = listed_hosts(&self.hosts, &index.duplicate_of)
            .map(|host| &host.store)
            .collect::<Vec<_>>();
        for store in &listed_stores {
            let state = &store.read(cx).state;
            let archived = store.read(cx).archived.iter().flatten();
            for agent in state.agents().iter().chain(archived) {
                if let Entry::Vacant(entry) = index.agents.entry(agent.id.clone()) {
                    entry.insert((*store).clone());
                }
            }
            let workspace_ids = state
                .workspaces
                .keys()
                .map(String::as_str)
                .chain(state.agents().iter().filter_map(agent_workspace_id));
            for workspace_id in workspace_ids {
                if let Entry::Vacant(entry) = index.workspaces.entry(workspace_id.to_owned()) {
                    entry.insert((*store).clone());
                }
            }
            for project_id in state.projects.keys() {
                if let Entry::Vacant(entry) = index.projects.entry(project_id.clone()) {
                    entry.insert((*store).clone());
                }
            }
        }
        index.activity = HostActivity::of_stores(listed_stores.iter().map(|store| store.read(cx)));
        self.index = index;
    }

    /// Connects the host at `index` with its client ID and any password typed this session.
    /// The client ID is resolved here, not when settings load, so a profile saved without one
    /// gets the same stable ID every time.
    fn connect_host_at(&self, index: usize, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index) else {
            return;
        };
        let Some(mut profile) = host.profile.clone() else {
            return;
        };
        profile.client_id = client_id_for(&profile, cx);
        let password = self.passwords.get(&profile.name).cloned();
        host.store.update(cx, |store, cx| {
            let generation = store.begin_connection(profile.clone());
            store.connect(profile, password, generation, cx);
        });
    }

    fn connect_all(&mut self, force: bool, cx: &mut Context<Self>) {
        for index in 0..self.hosts.len() {
            let disconnected = self
                .hosts
                .get(index)
                .is_some_and(|host| host.store.read(cx).status == ConnectionStatus::Disconnected);
            if force || disconnected {
                self.connect_host_at(index, cx);
            }
        }
    }

    /// A host reaching a daemon an earlier host already reaches would stream everything twice
    /// and alert twice, so it disconnects once its daemon is known.
    fn disconnect_duplicates(&self, cx: &mut Context<Self>) {
        let duplicates = (0..self.hosts.len())
            .filter(|index| self.duplicate_of(*index).is_some())
            .filter_map(|index| self.hosts.get(index))
            .filter(|host| host.store.read(cx).status != ConnectionStatus::Disconnected)
            .map(|host| host.store.clone())
            .collect::<Vec<_>>();
        for store in duplicates {
            store.update(cx, |store, cx| store.disconnect(cx));
        }
    }

    fn sync(&mut self, profiles: Vec<PaseoConnectionProfile>, cx: &mut Context<Self>) {
        let configured = self
            .hosts
            .iter()
            .filter_map(|host| host.profile.clone())
            .collect::<Vec<_>>();
        // Settings change for many reasons; hosts only when the profiles do.
        if configured == profiles {
            return;
        }
        let mut previous = std::mem::take(&mut self.hosts);
        let mut hosts = Vec::with_capacity(profiles.len().max(1));
        let mut changed = Vec::new();
        for profile in profiles {
            let existing = previous
                .iter()
                .position(|host| host.name() == profile.name)
                // The stand-in becomes the first configured host, so views built before any
                // profile existed keep a live store.
                .or_else(|| previous.iter().position(|host| host.profile.is_none()));
            let host = match existing {
                Some(index) => {
                    let mut host = previous.remove(index);
                    if host.profile.as_ref() != Some(&profile) {
                        host.profile = Some(profile);
                        changed.push(hosts.len());
                    }
                    host
                }
                None => {
                    changed.push(hosts.len());
                    Self::new_host(
                        Some(profile),
                        &self.archived_subagents,
                        &self.reviewed_edits,
                        cx,
                    )
                }
            };
            hosts.push(host);
        }
        for removed in previous {
            removed.store.update(cx, |store, cx| store.disconnect(cx));
        }
        if hosts.is_empty() {
            // With no profiles left, the long-lived fallback store stands in again.
            hosts.push(Self::host_for_store(self.fallback.clone(), cx));
        }
        self.hosts = hosts;
        self.rebuild_index(cx);
        if self.connecting {
            for index in changed {
                self.connect_host_at(index, cx);
            }
        }
        cx.notify();
    }

    fn new_host(
        profile: Option<PaseoConnectionProfile>,
        archived_subagents: &SavedSet,
        reviewed_edits: &SavedSet,
        cx: &mut Context<Self>,
    ) -> PaseoHost {
        let store = cx.new(|_| {
            let mut store = PaseoStore::default();
            store.archived_subagents = archived_subagents.clone();
            store.reviewed_edits = reviewed_edits.clone();
            store
        });
        let mut host = Self::host_for_store(store, cx);
        host.profile = profile;
        host
    }

    fn host_for_store(store: Entity<PaseoStore>, cx: &mut Context<Self>) -> PaseoHost {
        let subscriptions = [
            cx.observe(&store, |registry: &mut Self, _, cx| {
                registry.rebuild_index(cx);
                registry.disconnect_duplicates(cx);
                cx.notify()
            }),
            cx.subscribe(&store, |_, store, event: &StoreEvent, cx| match event {
                StoreEvent::NeedsAttention { agent_id, message } => {
                    cx.emit(HostsEvent::NeedsAttention {
                        store,
                        agent_id: agent_id.clone(),
                        message: message.clone(),
                    })
                }
                StoreEvent::FocusChanged => cx.emit(HostsEvent::FocusChanged),
                StoreEvent::WorkspaceRemoved {
                    workspace_id,
                    worktree_directory,
                } => cx.emit(HostsEvent::WorkspaceRemoved {
                    workspace_id: workspace_id.clone(),
                    worktree_directory: worktree_directory.clone(),
                }),
                StoreEvent::Stream(_) | StoreEvent::TimelineChanged(_) => {}
            }),
        ];
        PaseoHost {
            profile: None,
            store,
            _subscriptions: subscriptions,
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{BorrowAppContext as _, TestAppContext};
    use paseo_client::{AgentSummary, ServerInfo};
    use settings::{PaseoSettingsContent, SettingsStore};

    use super::*;

    fn profile(name: &str) -> PaseoConnectionProfile {
        PaseoConnectionProfile {
            name: name.into(),
            target_uri: format!("ws://localhost:{}/ws", 6767 + name.len()),
            editor_ssh_uri: None,
            client_id: format!("client-{name}"),
        }
    }

    fn agent(id: &str) -> AgentSummary {
        AgentSummary {
            id: id.into(),
            title: None,
            status: "idle".into(),
            directory: None,
            project: None,
            extra: serde_json::json!({}),
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            PaseoSettings::register(cx);
            init(cx);
        });
    }

    fn set_profiles(profiles: Vec<PaseoConnectionProfile>, cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.paseo = Some(PaseoSettingsContent {
                        profiles: Some(profiles),
                        ..Default::default()
                    });
                });
            });
        });
        cx.run_until_parked();
    }

    fn host_names(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            registry(cx)
                .read(cx)
                .hosts()
                .iter()
                .map(|host| host.name().to_owned())
                .collect()
        })
    }

    #[gpui::test]
    fn hosts_follow_profiles(cx: &mut TestAppContext) {
        init_test(cx);
        // The default settings' Local profile takes over the stand-in host.
        assert_eq!(host_names(cx), vec!["Local"]);
        let first = cx.update(|cx| default_store(cx));

        set_profiles(vec![profile("Local"), profile("box")], cx);
        assert_eq!(host_names(cx), vec!["Local", "box"]);
        assert_eq!(cx.update(|cx| store_named("Local", cx)), first);

        set_profiles(vec![profile("box")], cx);
        assert_eq!(host_names(cx), vec!["box"]);

        set_profiles(Vec::new(), cx);
        assert_eq!(host_names(cx), vec![String::new()]);
        assert_eq!(cx.update(|cx| default_store(cx)), first);
    }

    #[gpui::test]
    fn lookup_finds_agent_on_its_host(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("box")], cx);
        let (local, remote) = cx.update(|cx| (store_named("local", cx), store_named("box", cx)));
        local.update(cx, |store, _| {
            store.state.set_agents(vec![agent("local-agent")])
        });
        remote.update(cx, |store, _| {
            store.state.set_agents(vec![agent("box-agent")])
        });

        cx.update(|cx| {
            assert_eq!(store_for_agent("local-agent", cx), Some(local.clone()));
            assert_eq!(store_for_agent("box-agent", cx), Some(remote.clone()));
            assert_eq!(store_for_agent("missing", cx), None);
        });

        cx.update(|cx| set_focused_agent("box-agent".into(), cx));
        cx.update(|cx| assert_eq!(focused_agent(cx).as_deref(), Some("box-agent")));
        cx.update(|cx| set_focused_agent("local-agent".into(), cx));
        cx.update(|cx| {
            assert_eq!(focused_agent(cx).as_deref(), Some("local-agent"));
            assert_eq!(remote.read(cx).focused_agent, None);
        });
    }

    #[gpui::test]
    fn attention_lists_agents_from_every_host(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("box")], cx);
        let (local, remote) = cx.update(|cx| (store_named("local", cx), store_named("box", cx)));
        let failed = |id: &str| AgentSummary {
            status: "error".into(),
            extra: serde_json::json!({"requiresAttention": true}),
            ..agent(id)
        };
        local.update(cx, |store, cx| {
            store.state.set_agents(vec![failed("local-failed")]);
            cx.notify();
        });
        remote.update(cx, |store, cx| {
            store.state.set_agents(vec![failed("box-failed")]);
            cx.notify();
        });

        cx.update(|cx| {
            let mut agent_ids = crate::attention::all_attention_entries(cx)
                .into_iter()
                .map(|(_, entry)| entry.agent_id)
                .collect::<Vec<_>>();
            agent_ids.sort();
            assert_eq!(agent_ids, vec!["box-failed", "local-failed"]);
            assert_eq!(
                activity(cx).attention,
                crate::attention::AttentionSummary {
                    count: 2,
                    most_urgent: Some(crate::attention::AttentionReason::Failed),
                }
            );
        });
    }

    #[gpui::test]
    fn hosts_share_their_saved_sets(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("box")], cx);
        let (local, remote) = cx.update(|cx| (store_named("local", cx), store_named("box", cx)));
        local.update(cx, |store, cx| {
            store.mark_edits_reviewed(vec!["agent|edit".into()], cx)
        });
        remote.update(cx, |store, cx| {
            store.archive_subagents(["agent:subagent".to_owned()], cx)
        });
        cx.update(|cx| {
            for store in [&local, &remote] {
                let store = store.read(cx);
                assert!(store.reviewed_edits.contains("agent|edit"));
                assert!(store.archived_subagents.contains("agent:subagent"));
            }
        });
    }

    #[gpui::test]
    fn duplicate_daemon_is_listed_once(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("same-machine")], cx);
        let (first, second) =
            cx.update(|cx| (store_named("local", cx), store_named("same-machine", cx)));
        for store in [&first, &second] {
            store.update(cx, |store, cx| {
                store.server_info = ServerInfo {
                    server_id: Some("srv-1".into()),
                    ..Default::default()
                };
                store.state.set_agents(vec![agent("shared-agent")]);
                cx.notify();
            });
        }

        cx.update(|cx| {
            assert_eq!(stores(cx), vec![first.clone()]);
            assert_eq!(store_for_agent("shared-agent", cx), Some(first.clone()));
            let registry = registry(cx);
            let registry = registry.read(cx);
            assert_eq!(registry.duplicate_of(1).map(PaseoHost::name), Some("local"));
        });
    }

    fn workspace(id: &str) -> paseo_client::WorkspaceDescriptor {
        paseo_client::WorkspaceDescriptor {
            id: id.into(),
            project_id: "prj".into(),
            project_display_name: "project".into(),
            project_root_path: PathBuf::from("/work"),
            directory: PathBuf::from("/work"),
            kind: "directory".into(),
            worktree_slug: None,
            name: id.into(),
            title: None,
            pinned_at: None,
            labels: Vec::new(),
            status: "done".into(),
            activity_at: None,
            diff_stat: None,
            scripts: Vec::new(),
            current_branch: None,
            is_paseo_worktree: false,
            extra: serde_json::Value::Null,
        }
    }

    #[gpui::test]
    fn index_resolves_each_id_to_its_first_listed_host(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(
            vec![profile("local"), profile("same-machine"), profile("box")],
            cx,
        );
        let (first, duplicate, remote) = cx.update(|cx| {
            (
                store_named("local", cx),
                registry(cx).read(cx).hosts()[1].store.clone(),
                store_named("box", cx),
            )
        });
        for store in [&first, &duplicate] {
            store.update(cx, |store, cx| {
                store.server_info = ServerInfo {
                    server_id: Some("srv-1".into()),
                    ..Default::default()
                };
                store.state.set_agents(vec![AgentSummary {
                    extra: serde_json::json!({"workspaceId": "wks-shared"}),
                    ..agent("shared-agent")
                }]);
                cx.notify();
            });
        }
        duplicate.update(cx, |store, cx| {
            store.state.set_agents(vec![agent("only-on-duplicate")]);
            cx.notify();
        });
        remote.update(cx, |store, cx| {
            store.state.set_agents(vec![AgentSummary {
                extra: serde_json::json!({"workspaceId": "wks-unloaded"}),
                ..agent("box-agent")
            }]);
            store
                .state
                .workspaces
                .insert("wks-box".into(), workspace("wks-box"));
            store.archived = Some(vec![agent("box-archived")]);
            cx.notify();
        });

        cx.update(|cx| {
            let registry = registry(cx);
            let index = &registry.read(cx).index;
            assert_eq!(
                index.duplicate_of,
                vec![None, Some(0), None],
                "only the second host reaches an earlier host's daemon"
            );
            assert_eq!(index.agents.get("shared-agent"), Some(&first));
            assert_eq!(
                index.agents.get("only-on-duplicate"),
                None,
                "a hidden duplicate's agents aren't indexed"
            );
            assert_eq!(index.agents.get("box-agent"), Some(&remote));
            assert_eq!(index.agents.get("box-archived"), Some(&remote));
            assert_eq!(index.workspaces.get("wks-shared"), Some(&first));
            assert_eq!(index.workspaces.get("wks-box"), Some(&remote));
            assert_eq!(
                index.workspaces.get("wks-unloaded"),
                Some(&remote),
                "a workspace whose descriptor hasn't loaded is found by its agents"
            );

            assert_eq!(store_for_agent("shared-agent", cx), Some(first.clone()));
            assert_eq!(store_for_agent("box-archived", cx), Some(remote.clone()));
            assert_eq!(store_for_agent("only-on-duplicate", cx), None);
            assert_eq!(
                store_for_workspace("wks-unloaded", cx),
                Some(remote.clone())
            );
            assert_eq!(
                crate::workspace_tabs::agent_paseo_workspace("shared-agent", cx).as_deref(),
                Some("wks-shared")
            );
        });
    }

    #[gpui::test]
    fn lookup_finds_an_agent_added_before_its_host_is_indexed(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("box")], cx);
        let remote = cx.update(|cx| store_named("box", cx));
        cx.update(|cx| {
            remote.update(cx, |store, cx| {
                store.state.set_agents(vec![agent("just-created")]);
                cx.notify();
            });
            assert_eq!(store_for_agent("just-created", cx), Some(remote.clone()));
        });
    }

    #[gpui::test]
    fn listing_follows_server_ids_as_hosts_connect(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("same-machine")], cx);
        let (first, second) =
            cx.update(|cx| (store_named("local", cx), store_named("same-machine", cx)));
        let set_server_id = |store: &Entity<PaseoStore>, id: &str, cx: &mut TestAppContext| {
            let id = id.to_owned();
            store.update(cx, |store, cx| {
                store.server_info = ServerInfo {
                    server_id: Some(id),
                    ..Default::default()
                };
                cx.notify();
            });
        };
        let listed_names = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                registry(cx)
                    .read(cx)
                    .listed()
                    .map(|host| host.name().to_owned())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(listed_names(cx), vec!["local", "same-machine"]);

        set_server_id(&first, "srv-1", cx);
        set_server_id(&second, "srv-2", cx);
        assert_eq!(listed_names(cx), vec!["local", "same-machine"]);

        set_server_id(&second, "srv-1", cx);
        assert_eq!(listed_names(cx), vec!["local"]);
        assert_eq!(
            cx.update(|cx| store_named("same-machine", cx)),
            first,
            "a lookup by name lands on the host it duplicates"
        );

        set_profiles(vec![profile("same-machine"), profile("local")], cx);
        assert_eq!(
            listed_names(cx),
            vec!["same-machine"],
            "reordering hosts relists them"
        );
    }

    #[gpui::test]
    fn activity_counts_running_and_attention_across_hosts(cx: &mut TestAppContext) {
        init_test(cx);
        set_profiles(vec![profile("local"), profile("box")], cx);
        let (local, remote) = cx.update(|cx| (store_named("local", cx), store_named("box", cx)));
        local.update(cx, |store, cx| {
            store.state.set_agents(vec![
                AgentSummary {
                    status: "running".into(),
                    ..agent("working")
                },
                AgentSummary {
                    status: "idle".into(),
                    extra: serde_json::json!({"requiresAttention": true}),
                    ..agent("finished")
                },
            ]);
            cx.notify();
        });
        remote.update(cx, |store, cx| {
            store.state.set_agents(vec![AgentSummary {
                status: "running".into(),
                ..agent("asking")
            }]);
            store.state.permissions.insert(
                "request".into(),
                paseo_client::PermissionRequest {
                    agent_id: "asking".into(),
                    request_id: "request".into(),
                    title: "Run a command".into(),
                    description: None,
                    extra: serde_json::json!({}),
                },
            );
            cx.notify();
        });

        cx.update(|cx| {
            assert_eq!(
                activity(cx),
                HostActivity {
                    running: 1,
                    attention: crate::attention::AttentionSummary {
                        count: 2,
                        most_urgent: Some(crate::attention::AttentionReason::NeedsInput),
                    },
                },
                "an agent waiting on a permission needs input rather than running"
            );
        });
    }

    #[gpui::test]
    fn draft_uses_selected_host(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        set_profiles(vec![profile("local"), profile("box")], cx);
        let remote = cx.update(|cx| store_named("box", cx));
        let window = cx.add_empty_window();
        let draft = window.update(|window, cx| {
            cx.new(|cx| {
                crate::agent_view::AgentView::on_host(remote.clone(), None, None, None, window, cx)
            })
        });
        window.update(|_, cx| {
            let draft = draft.read(cx);
            assert_eq!(draft.store, remote);
            assert_eq!(draft.composer.read(cx).store, remote);
            assert_ne!(
                default_store(cx),
                remote,
                "the default host stays the first host"
            );
        });
    }
}
