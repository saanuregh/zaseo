use anyhow::Result;
use base64::Engine as _;
use editor::{CompletionProvider, Editor, EditorElement, EditorMode, EditorStyle, MultiBuffer};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, ClipboardEntry, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Image, IntoElement, Subscription, Task, TaskExt, TextStyle, WeakEntity,
    Window, prelude::*,
};
use language::{Buffer, CodeLabel};
use lsp::CompletionContext;
use paseo_client::{
    ActiveTurnBehavior, AgentSummary, BranchSuggestion, CheckoutStatus, CreateAgent, DraftConfig,
    ImageAttachment, PaseoEvent, Provider, SendMessage, WorktreeTarget,
};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};
use serde_json::Value;
use settings::Settings as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use text::{Anchor, ToOffset as _};
use theme_settings::ThemeSettings;
use ui::{
    CircularProgress, CommonAnimationExt, ContextMenu, IconButton, IconButtonShape, Indicator,
    PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};

use crate::dictation::{
    AudioMessage, CHUNK_SAMPLES, CaptureFormat, Pcm16Encoder, Recorder, start_capture,
};
use crate::store::{PaseoStore, StoreEvent, agent_is_running, agent_provider};
use crate::{
    CreatePreferences, CycleMode, FocusComposer, InterruptAgent, ProviderPreference, QueueMessage,
    ReviewLastTurn, SendMessage as SendMessageAction, ToggleDictation, ToggleModePicker,
    ToggleModelPicker, ToggleThinkingPicker,
};

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub color_tier: Option<String>,
}

/// The chip color Paseo gives a permission mode (`AgentModeColorTier`).
fn mode_color(color_tier: Option<&str>) -> Color {
    match color_tier {
        Some("safe") => Color::Success,
        Some("dangerous") => Color::Error,
        Some("planning") => Color::Info,
        Some(hex) if hex.starts_with('#') => gpui::Rgba::try_from(hex)
            .map(|rgba| Color::Custom(rgba.into()))
            .unwrap_or(Color::Muted),
        _ => Color::Muted,
    }
}

pub(crate) fn choices(value: Option<&Value>) -> Vec<Choice> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|option| option.get("isSelectable").and_then(Value::as_bool) != Some(false))
        .filter_map(|option| {
            let id = option.get("id").and_then(Value::as_str)?.to_owned();
            Some(Choice {
                label: option
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|label| !label.is_empty())
                    .unwrap_or(&id)
                    .to_owned(),
                description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                is_default: option.get("isDefault").and_then(Value::as_bool) == Some(true),
                color_tier: option
                    .get("colorTier")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                id,
            })
        })
        .collect()
}

pub fn provider_models(provider: &Provider) -> Vec<Choice> {
    choices(provider.extra.get("models"))
}

fn model_value<'a>(provider: &'a Provider, model_id: &str) -> Option<&'a Value> {
    provider
        .extra
        .get("models")
        .and_then(Value::as_array)?
        .iter()
        .find(|model| {
            model.get("id").and_then(Value::as_str) == Some(model_id)
                || model
                    .get("aliases")
                    .and_then(Value::as_array)
                    .is_some_and(|aliases| {
                        aliases.iter().any(|alias| alias.as_str() == Some(model_id))
                    })
        })
}

pub fn thinking_options(provider: &Provider, model_id: &str) -> Vec<Choice> {
    choices(model_value(provider, model_id).and_then(|model| model.get("thinkingOptions")))
}

pub fn context_window_max(provider: &Provider, model_id: &str) -> Option<u64> {
    model_value(provider, model_id)
        .and_then(|model| model.get("contextWindowMaxTokens"))
        .and_then(Value::as_u64)
}

/// Resolves a choice the way Paseo's `resolve-agent-form.ts` does: the preferred ID when it is
/// still offered, then the option marked default, then the first option.
pub fn resolve_choice(
    options: &[Choice],
    preferred: Option<&str>,
    default_id: Option<&str>,
) -> Option<String> {
    preferred
        .and_then(|preferred| options.iter().find(|option| option.id == preferred))
        .or_else(|| default_id.and_then(|id| options.iter().find(|option| option.id == id)))
        .or_else(|| options.iter().find(|option| option.is_default))
        .or_else(|| options.first())
        .map(|option| option.id.clone())
}

/// What the controls row shows and changes: a live agent's settings or a draft's choices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentChoices {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub mode: Option<String>,
}

#[derive(Clone)]
struct QueuedMessage {
    id: usize,
    text: String,
    images: Vec<PastedImage>,
}

#[derive(Clone)]
struct PastedImage {
    id: usize,
    image: Arc<Image>,
}

impl PastedImage {
    fn attachment(&self) -> ImageAttachment {
        ImageAttachment {
            data_base64: base64::engine::general_purpose::STANDARD.encode(&self.image.bytes),
            mime_type: self.image.format.mime_type().to_owned(),
        }
    }
}

pub enum ComposerEvent {
    AgentCreated(String),
    ClearRequested { directory: Option<PathBuf> },
    Submitted,
}

pub struct Composer {
    store: Entity<PaseoStore>,
    agent_id: Option<String>,
    editor: Entity<Editor>,
    pub(crate) draft: AgentChoices,
    pub(crate) draft_directory: Option<PathBuf>,
    queue: Vec<QueuedMessage>,
    next_queue_id: usize,
    images: Vec<PastedImage>,
    next_image_id: usize,
    /// Daemon attachments sent when a draft creates its agent, such as forked chat history.
    context_attachments: Vec<Value>,
    /// Loaded once because reading the key-value store on every render blocks the main thread.
    preferences: CreatePreferences,
    fork_source_title: Option<String>,
    pending_text: Option<String>,
    creating: bool,
    was_running: bool,
    model_menu: PopoverMenuHandle<ContextMenu>,
    thinking_menu: PopoverMenuHandle<ContextMenu>,
    mode_menu: PopoverMenuHandle<ContextMenu>,
    provider_menu: PopoverMenuHandle<ContextMenu>,
    dictation: Option<Dictation>,
    /// The base the user picked for a new worktree; `None` branches off `default_base`.
    worktree_base: Option<BaseRef>,
    default_base: DefaultBase,
    _subscriptions: Vec<Subscription>,
}

/// What a new worktree branches off when the user picks nothing, read from the project's
/// checkout once per directory and connection.
#[derive(Default)]
struct DefaultBase {
    loaded_for: Option<(PathBuf, u64)>,
    value: Option<BaseRef>,
    _task: Option<Task<()>>,
}

/// A ref a new worktree can branch off, as Paseo's base picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseRef {
    /// Sent to the daemon as the worktree base.
    pub ref_name: String,
    pub label: String,
    /// Where the ref lives ("origin", "local") and how a local branch diverges from origin.
    pub detail: Option<String>,
}

impl BaseRef {
    fn from_ref(ref_name: String) -> Self {
        let (label, detail) = if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
            (branch.to_owned(), Some("local".to_owned()))
        } else if let Some(remainder) = ref_name.strip_prefix("refs/remotes/") {
            match remainder.split_once('/') {
                Some((remote, branch)) => (branch.to_owned(), Some(remote.to_owned())),
                None => (remainder.to_owned(), None),
            }
        } else {
            (ref_name.clone(), None)
        };
        Self {
            ref_name,
            label,
            detail,
        }
    }
}

/// Draft commands include project skills read from the checkout, so a branch switch needs a
/// fresh list rather than the one cached for the old branch.
fn draft_command_cache_key(
    provider: &str,
    directory: Option<&Path>,
    branch: Option<&str>,
) -> String {
    format!(
        "draft:{provider}:{}:{}",
        directory
            .map(|directory| directory.to_string_lossy())
            .unwrap_or_default(),
        branch.unwrap_or_default()
    )
}

/// The base Paseo preselects: the current branch's upstream, because branching off the local
/// ref would silently carry unpushed commits into the new worktree.
pub(crate) fn default_base_ref(status: &CheckoutStatus) -> Option<BaseRef> {
    let branch = status.current_branch.as_deref()?;
    let ref_name = status
        .upstream_ref
        .clone()
        .unwrap_or_else(|| format!("refs/heads/{branch}"));
    Some(BaseRef::from_ref(ref_name))
}

/// Picker rows for the daemon's branch suggestions, following Paseo: the origin ref first, and
/// a separate local row only when it differs from origin or the divergence is unknown.
pub(crate) fn base_ref_choices(
    suggestions: &[BranchSuggestion],
    selected: Option<&BaseRef>,
) -> Vec<BaseRef> {
    let mut dated = Vec::new();
    for suggestion in suggestions {
        let date = suggestion.committer_date.unwrap_or(0);
        let name = &suggestion.name;
        if suggestion.has_local.is_none() && suggestion.has_remote.is_none() {
            dated.push((
                date,
                BaseRef {
                    ref_name: name.clone(),
                    label: name.clone(),
                    detail: None,
                },
            ));
            continue;
        }
        let has_local = suggestion.has_local == Some(true);
        let has_remote = suggestion.has_remote == Some(true);
        let divergence = suggestion
            .local_ahead
            .zip(suggestion.local_behind)
            .filter(|_| has_local && has_remote);
        let shows_both = has_local
            && has_remote
            && divergence.is_none_or(|(ahead, behind)| ahead > 0 || behind > 0);
        if has_remote {
            dated.push((
                date,
                BaseRef::from_ref(format!("refs/remotes/origin/{name}")),
            ));
        }
        if has_local && (shows_both || !has_remote) {
            let mut local = BaseRef::from_ref(format!("refs/heads/{name}"));
            if let Some((ahead, behind)) = divergence {
                let counts = [(ahead, "+"), (behind, "\u{2212}")]
                    .into_iter()
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, sign)| format!("{sign}{count}"))
                    .collect::<Vec<_>>();
                if !counts.is_empty() {
                    local.detail = Some(format!("local {}", counts.join(" ")));
                }
            }
            dated.push((date, local));
        }
    }
    dated.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
    let mut choices = dated
        .into_iter()
        .map(|(_, choice)| choice)
        .collect::<Vec<_>>();
    if let Some(selected) = selected {
        choices.retain(|choice| choice.ref_name != selected.ref_name);
        choices.insert(0, selected.clone());
    }
    choices
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DictationPhase {
    Recording,
    Transcribing,
}

/// A dictation in progress: the microphone streams to the daemon, which sends back the text.
struct Dictation {
    id: String,
    phase: DictationPhase,
    /// Dropping the recorder closes the microphone.
    recorder: Option<Recorder>,
    audio: async_channel::Sender<AudioMessage>,
    started_at: std::time::Instant,
    partial: Option<String>,
    /// Dropping the task stops streaming, which is how cancelling ends it.
    _task: Task<()>,
    _ticker: Task<()>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(
        store: Entity<PaseoStore>,
        agent_id: Option<String>,
        directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let buffer = cx.new(|cx| Buffer::local("", cx));
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let mut editor = Editor::new(
                EditorMode::AutoHeight {
                    min_lines: 2,
                    max_lines: Some(10),
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_placeholder_text(
                "Message the agent, tag @files, or use /commands and /skills",
                window,
                cx,
            );
            editor.set_show_indent_guides(false, cx);
            editor.set_show_completions_on_input(Some(true));
            editor.set_soft_wrap();
            editor.disable_mouse_wheel_zoom();
            editor
        });
        let preferences = CreatePreferences::load(cx);
        let saved_preferences = preferences.clone();
        let draft_directory = directory
            .or_else(|| preferences.directory.clone().map(PathBuf::from))
            .or_else(|| {
                store
                    .read(cx)
                    .state
                    .agents
                    .iter()
                    .max_by_key(|agent| crate::store::agent_updated_at(agent))
                    .and_then(|agent| agent.directory.clone())
            });
        let this = cx.weak_entity();
        editor.update(cx, |editor, _| {
            editor
                .set_completion_provider(Some(Rc::new(PaseoCompletionProvider { composer: this })));
        });
        let was_running = agent_id
            .as_deref()
            .and_then(|agent_id| store.read(cx).agent(agent_id))
            .is_some_and(agent_is_running);
        let observer = cx.observe(&store, |composer: &mut Self, _, cx| {
            composer.store_changed(cx);
        });
        let stream_events = cx.subscribe_in(
            &store,
            window,
            |composer: &mut Self, _, event: &StoreEvent, window, cx| {
                composer.handle_dictation_event(event, window, cx)
            },
        );
        let mut composer = Self {
            store,
            agent_id,
            editor,
            draft: AgentChoices::default(),
            draft_directory,
            queue: Vec::new(),
            next_queue_id: 0,
            images: Vec::new(),
            next_image_id: 0,
            context_attachments: Vec::new(),
            preferences: saved_preferences,
            fork_source_title: None,
            pending_text: None,
            creating: false,
            was_running,
            model_menu: PopoverMenuHandle::default(),
            thinking_menu: PopoverMenuHandle::default(),
            mode_menu: PopoverMenuHandle::default(),
            provider_menu: PopoverMenuHandle::default(),
            dictation: None,
            worktree_base: None,
            default_base: DefaultBase::default(),
            _subscriptions: vec![observer, stream_events],
        };
        composer.draft.provider = preferences.provider;
        composer.load_commands(cx);
        composer.load_default_base(cx);
        composer
    }

    pub fn set_agent(&mut self, agent_id: String, cx: &mut Context<Self>) {
        self.agent_id = Some(agent_id);
        self.was_running = true;
        self.load_commands(cx);
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let handle = self.editor.focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.editor.focus_handle(cx).contains_focused(window, cx)
    }

    pub(crate) fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text(cx)
    }

    fn agent<'a>(&self, cx: &'a App) -> Option<&'a AgentSummary> {
        self.agent_id
            .as_deref()
            .and_then(|agent_id| self.store.read(cx).agent(agent_id))
    }

    fn is_running(&self, cx: &App) -> bool {
        self.agent(cx).is_some_and(agent_is_running)
    }

    fn command_cache_key(&self, cx: &App) -> String {
        match &self.agent_id {
            Some(agent_id) => agent_id.clone(),
            None => {
                let directory = self.draft_directory.as_deref();
                let branch = directory.and_then(|directory| {
                    self.store
                        .read(cx)
                        .state
                        .workspaces
                        .values()
                        .find(|workspace| workspace.directory == directory)
                        .and_then(|workspace| workspace.current_branch.as_deref())
                });
                draft_command_cache_key(
                    self.draft.provider.as_deref().unwrap_or_default(),
                    directory,
                    branch,
                )
            }
        }
    }

    fn load_commands(&mut self, cx: &mut Context<Self>) {
        // Subagent tabs hide the composer, and the daemon has no commands for a subagent.
        if self
            .agent_id
            .as_deref()
            .and_then(paseo_client::parse_subagent_timeline_id)
            .is_some()
        {
            return;
        }
        let key = self.command_cache_key(cx);
        let agent_id = self.agent_id.clone();
        let draft = if agent_id.is_none() {
            let choices = self.choices(cx);
            match (choices.provider, self.draft_directory.clone()) {
                (Some(provider), Some(cwd)) => Some(DraftConfig {
                    provider,
                    cwd,
                    mode_id: choices.mode,
                    model: choices.model,
                    thinking_option_id: choices.thinking,
                }),
                _ => None,
            }
        } else {
            None
        };
        if agent_id.is_none() && draft.is_none() {
            return;
        }
        self.store.update(cx, |store, cx| {
            store.load_commands(key, agent_id, draft, cx);
        });
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.load_default_base(cx);
        let running = self.is_running(cx);
        // A missing agent means a disconnect or archive, not a finished turn, so the queue waits.
        let turn_finished = self.was_running
            && !running
            && self.store.read(cx).connected()
            && self.agent(cx).is_some();
        if turn_finished && !self.queue.is_empty() {
            let queued = self.queue.remove(0);
            self.send_queued(queued, None, cx);
        }
        self.was_running = running;
        if self.agent_id.is_none() && self.draft.provider.is_none() {
            let resolved = self.choices(cx).provider;
            if resolved.is_some() {
                self.draft.provider = resolved;
            }
        }
        if self.agent_id.is_none()
            && !self
                .store
                .read(cx)
                .commands
                .contains_key(&self.command_cache_key(cx))
        {
            self.load_commands(cx);
        }
    }

    /// The effective provider, model, thinking and mode, resolved against the provider
    /// snapshot so a stale preference never selects an option the daemon no longer offers.
    pub fn choices(&self, cx: &App) -> AgentChoices {
        let store = self.store.read(cx);
        if let Some(agent) = self.agent(cx) {
            let provider = agent_provider(agent).to_owned();
            let runtime = agent.extra.get("runtimeInfo");
            let model = agent
                .extra
                .get("model")
                .and_then(Value::as_str)
                .or_else(|| {
                    runtime
                        .and_then(|runtime| runtime.get("model"))
                        .and_then(Value::as_str)
                })
                .map(str::to_owned);
            let thinking = agent
                .extra
                .get("thinkingOptionId")
                .and_then(Value::as_str)
                .or_else(|| {
                    agent
                        .extra
                        .get("effectiveThinkingOptionId")
                        .and_then(Value::as_str)
                })
                .map(str::to_owned);
            return AgentChoices {
                provider: Some(provider),
                model,
                thinking,
                mode: agent
                    .extra
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
        }
        let preferences = &self.preferences;
        let ready = |provider: &&Provider| {
            provider.status == "ready"
                && provider.extra.get("enabled").and_then(Value::as_bool) != Some(false)
        };
        let provider = self
            .draft
            .provider
            .as_deref()
            .and_then(|id| store.provider(id))
            .or_else(|| store.providers.iter().find(ready))
            .or_else(|| store.providers.first());
        let Some(provider) = provider else {
            return AgentChoices::default();
        };
        let preference = preferences
            .providers
            .get(&provider.id)
            .cloned()
            .unwrap_or_default();
        let models = provider_models(provider);
        let model = resolve_choice(
            &models,
            self.draft.model.as_deref().or(preference.model.as_deref()),
            None,
        );
        let thinking = model.as_deref().and_then(|model| {
            let options = thinking_options(provider, model);
            let default_id = model_value(provider, model)
                .and_then(|value| value.get("defaultThinkingOptionId"))
                .and_then(Value::as_str);
            resolve_choice(
                &options,
                self.draft
                    .thinking
                    .as_deref()
                    .or(preference.thinking_by_model.get(model).map(String::as_str)),
                default_id,
            )
        });
        let modes = choices(provider.extra.get("modes"));
        let mode = resolve_choice(
            &modes,
            self.draft.mode.as_deref().or(preference.mode.as_deref()),
            provider.extra.get("defaultModeId").and_then(Value::as_str),
        );
        AgentChoices {
            provider: Some(provider.id.clone()),
            model,
            thinking,
            mode,
        }
    }

    pub fn mode_options(&self, cx: &App) -> Vec<Choice> {
        if let Some(agent) = self.agent(cx) {
            return choices(agent.extra.get("availableModes"));
        }
        let choices_now = self.choices(cx);
        choices_now
            .provider
            .as_deref()
            .and_then(|id| self.store.read(cx).provider(id))
            .map(|provider| choices(provider.extra.get("modes")))
            .unwrap_or_default()
    }

    fn current_provider<'a>(&self, cx: &'a App) -> Option<&'a Provider> {
        let provider_id = self.choices(cx).provider?;
        self.store.read(cx).provider(&provider_id)
    }

    pub fn select_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        match self.agent_id.clone() {
            Some(agent_id) => self.store.update(cx, |store, cx| {
                store.set_model(&agent_id, model_id.clone(), cx)
            }),
            None => {
                self.draft.model = Some(model_id.clone());
                self.draft.thinking = None;
            }
        }
        self.remember(|preference, _| preference.model = Some(model_id), cx);
        cx.notify();
    }

    pub fn select_thinking(&mut self, option_id: String, cx: &mut Context<Self>) {
        let model = self.choices(cx).model;
        match self.agent_id.clone() {
            Some(agent_id) => self.store.update(cx, |store, cx| {
                store.set_thinking(&agent_id, option_id.clone(), cx)
            }),
            None => self.draft.thinking = Some(option_id.clone()),
        }
        if let Some(model) = model {
            self.remember(
                |preference, _| {
                    preference.thinking_by_model.insert(model, option_id);
                },
                cx,
            );
        }
        cx.notify();
    }

    pub fn select_mode(&mut self, mode_id: String, cx: &mut Context<Self>) {
        match self.agent_id.clone() {
            Some(agent_id) => self.store.update(cx, |store, cx| {
                store.set_mode(&agent_id, mode_id.clone(), cx)
            }),
            None => self.draft.mode = Some(mode_id.clone()),
        }
        self.remember(|preference, _| preference.mode = Some(mode_id), cx);
        cx.notify();
    }

    pub fn select_provider(&mut self, provider_id: String, cx: &mut Context<Self>) {
        self.draft = AgentChoices {
            provider: Some(provider_id),
            ..AgentChoices::default()
        };
        self.load_commands(cx);
        cx.notify();
    }

    /// Prepares a draft that continues another agent's conversation.
    pub fn prefill_fork(
        &mut self,
        attachment: Value,
        source_title: String,
        choices: AgentChoices,
        cx: &mut Context<Self>,
    ) {
        self.context_attachments = vec![attachment];
        self.fork_source_title = Some(source_title);
        self.draft = choices;
        self.load_commands(cx);
        cx.notify();
    }

    pub fn set_draft_directory(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        if self.draft_directory.as_ref() != Some(&directory) {
            self.worktree_base = None;
        }
        self.draft_directory = Some(directory);
        self.load_commands(cx);
        self.load_default_base(cx);
        cx.notify();
    }

    fn remember(
        &mut self,
        update: impl FnOnce(&mut ProviderPreference, &mut CreatePreferences),
        cx: &App,
    ) {
        let Some(provider) = self.choices(cx).provider else {
            return;
        };
        let preferences = &mut self.preferences;
        let mut preference = preferences
            .providers
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        update(&mut preference, preferences);
        preferences.providers.insert(provider.clone(), preference);
        preferences.provider = Some(provider);
        preferences.save(cx);
    }

    fn cycle_mode(&mut self, _: &CycleMode, _window: &mut Window, cx: &mut Context<Self>) {
        let options = self.mode_options(cx);
        if options.is_empty() {
            return;
        }
        let current = self.choices(cx).mode;
        let index = current
            .and_then(|current| options.iter().position(|option| option.id == current))
            .map(|index| (index + 1) % options.len())
            .unwrap_or(0);
        if let Some(option) = options.get(index) {
            self.select_mode(option.id.clone(), cx);
        }
    }

    fn paste(&mut self, _: &editor::actions::Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let images = clipboard
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(Arc::new(image.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if images.is_empty() {
            return;
        }
        cx.stop_propagation();
        for image in images {
            self.images.push(PastedImage {
                id: self.next_image_id,
                image,
            });
            self.next_image_id += 1;
        }
        cx.notify();
    }

    fn render_context_attachments(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1p5()
            .flex_wrap()
            .children(
                self.context_attachments
                    .iter()
                    .enumerate()
                    .map(|(index, _attachment)| {
                        let title = match &self.fork_source_title {
                            Some(source) => format!("Fork of {source}"),
                            None => "Chat history".into(),
                        };
                        h_flex()
                            .id(("paseo-context-attachment", index))
                            .h(rems_from_px(26_f32))
                            .max_w(rems_from_px(360_f32))
                            .pl_2()
                            .pr_0p5()
                            .gap_1p5()
                            .rounded_md()
                            .bg(cx.theme().colors().element_background)
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Icon::new(IconName::GitBranchPlus)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(title).size(LabelSize::Small).truncate())
                            .child(
                                IconButton::new(("paseo-context-remove", index), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Remove"))
                                    .on_click(cx.listener(move |composer, _, _, cx| {
                                        if index < composer.context_attachments.len() {
                                            composer.context_attachments.remove(index);
                                        }
                                        cx.notify();
                                    })),
                            )
                    }),
            )
    }

    fn render_images(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1p5()
            .flex_wrap()
            .children(self.images.iter().map(|pasted| {
                let id = pasted.id;
                let group = SharedString::from(format!("paseo-image-{id}"));
                div()
                    .id(("paseo-image", id))
                    .group(group.clone())
                    .relative()
                    .size(rems_from_px(48_f32))
                    .rounded_md()
                    .overflow_hidden()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        gpui::img(pasted.image.clone())
                            .size_full()
                            .object_fit(gpui::ObjectFit::Cover),
                    )
                    .child(
                        div().absolute().top_0().right_0().child(
                            IconButton::new(("paseo-image-remove", id), IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text("Remove image"))
                                .visible_on_hover(group)
                                .on_click(cx.listener(move |composer, _, _, cx| {
                                    composer.images.retain(|image| image.id != id);
                                    cx.notify();
                                })),
                        ),
                    )
            }))
    }

    fn send_action(&mut self, _: &SendMessageAction, _window: &mut Window, cx: &mut Context<Self>) {
        self.submit(false, cx);
    }

    fn queue_action(&mut self, _: &QueueMessage, _window: &mut Window, cx: &mut Context<Self>) {
        self.submit(true, cx);
    }

    fn interrupt(&mut self, _: &InterruptAgent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.is_some() {
            self.cancel_dictation(cx);
            return;
        }
        if !self.is_running(cx) {
            cx.propagate();
            return;
        }
        if let Some(agent_id) = self.agent_id.clone() {
            self.store
                .update(cx, |store, cx| store.cancel(&agent_id, cx));
        }
    }

    pub fn submit(&mut self, queue: bool, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let trimmed = text.trim();
        if (trimmed.is_empty() && self.images.is_empty())
            || self.pending_text.is_some()
            || self.creating
        {
            return;
        }
        if self.agent_id.is_some() {
            match trimmed {
                "/exit" | "/quit" | "/q" => {
                    self.clear_editor(cx);
                    if let Some(agent_id) = self.agent_id.clone() {
                        self.store
                            .update(cx, |store, cx| store.archive(&agent_id, cx));
                    }
                    return;
                }
                "/clear" | "/new" => {
                    self.clear_editor(cx);
                    let directory = self.agent(cx).and_then(|agent| agent.directory.clone());
                    if let Some(agent_id) = self.agent_id.clone() {
                        self.store
                            .update(cx, |store, cx| store.archive(&agent_id, cx));
                    }
                    cx.emit(ComposerEvent::ClearRequested { directory });
                    return;
                }
                _ => {}
            }
        }
        let Some(agent_id) = self.agent_id.clone() else {
            self.create_agent(text, cx);
            return;
        };
        let running = self.is_running(cx);
        let pasted = std::mem::take(&mut self.images);
        if queue && running {
            self.queue.push(QueuedMessage {
                id: self.next_queue_id,
                text,
                images: pasted,
            });
            self.next_queue_id += 1;
            self.clear_editor(cx);
            cx.notify();
            return;
        }
        let behavior = running.then_some(ActiveTurnBehavior::Steer);
        self.pending_text = Some(text.clone());
        let message_id = uuid::Uuid::new_v4().to_string();
        let task = self.store.update(cx, |store, cx| {
            store.clear_attention(&agent_id, cx);
            remember_sent_images(store, &message_id, &pasted);
            store.send_message(
                SendMessage {
                    agent_id,
                    text,
                    message_id,
                    behavior,
                    images: attachments(&pasted),
                },
                cx,
            )
        });
        self.finish_send(task, pasted, cx);
    }

    /// Sends a queued message, putting it back at the front of the queue if the send fails.
    fn send_queued(
        &mut self,
        queued: QueuedMessage,
        behavior: Option<ActiveTurnBehavior>,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.agent_id.clone() else {
            self.queue.insert(0, queued);
            return;
        };
        let message_id = uuid::Uuid::new_v4().to_string();
        let task = self.store.update(cx, |store, cx| {
            remember_sent_images(store, &message_id, &queued.images);
            store.send_message(
                SendMessage {
                    agent_id,
                    text: queued.text.clone(),
                    message_id,
                    behavior,
                    images: attachments(&queued.images),
                },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |composer, cx| {
                if let Err(error) = result {
                    composer.queue.insert(0, queued);
                    composer.store.update(cx, |store, cx| {
                        store.state.error = Some(error.to_string());
                        cx.notify();
                    });
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn finish_send(
        &mut self,
        task: Task<Result<()>>,
        pasted: Vec<PastedImage>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |composer, cx| {
                let submitted = composer.pending_text.take();
                match result {
                    Ok(()) => {
                        if submitted.is_some_and(|submitted| submitted == composer.text(cx)) {
                            composer.clear_editor(cx);
                        }
                        cx.emit(ComposerEvent::Submitted);
                    }
                    Err(error) => {
                        composer.images.splice(0..0, pasted);
                        composer.store.update(cx, |store, cx| {
                            store.state.error = Some(error.to_string());
                            cx.notify();
                        })
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn create_agent(&mut self, text: String, cx: &mut Context<Self>) {
        let choices = self.choices(cx);
        let Some(provider) = choices.provider.clone() else {
            self.store.update(cx, |store, cx| {
                store.state.error = Some("No Paseo provider is available".into());
                cx.notify();
            });
            return;
        };
        let Some(directory) = self.draft_directory.clone() else {
            self.store.update(cx, |store, cx| {
                store.state.error = Some("Choose a project directory for the new agent".into());
                cx.notify();
            });
            return;
        };
        self.preferences.provider = Some(provider.clone());
        self.preferences.directory = directory.to_str().map(str::to_owned);
        self.preferences.save(cx);
        self.creating = true;
        let submitted = text.clone();
        let new_worktree = self.uses_new_worktree(cx);
        let pasted = std::mem::take(&mut self.images);
        let task = self.store.update(cx, |store, cx| {
            store.create_agent(
                CreateAgent {
                    provider,
                    model: choices.model,
                    directory,
                    title: None,
                    initial_prompt: Some(text),
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                    mode_id: choices.mode,
                    thinking_option_id: choices.thinking,
                    images: attachments(&pasted),
                    attachments: self.context_attachments.clone(),
                    worktree: new_worktree.then(|| WorktreeTarget {
                        new_branch: worktree_branch_name(&submitted),
                        base: self.worktree_base().map(|base| base.ref_name.clone()),
                    }),
                },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |composer, cx| {
                composer.creating = false;
                match result {
                    Ok(agent) => {
                        if composer.text(cx) == submitted {
                            composer.clear_editor(cx);
                        }
                        composer.context_attachments.clear();
                        composer.set_agent(agent.id.clone(), cx);
                        cx.emit(ComposerEvent::AgentCreated(agent.id));
                    }
                    Err(error) => {
                        composer.images.splice(0..0, pasted);
                        composer.store.update(cx, |store, cx| {
                            store.state.error = Some(error.to_string());
                            cx.notify();
                        })
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn clear_editor(&mut self, cx: &mut Context<Self>) {
        let buffer = self.editor.read(cx).buffer().read(cx).as_singleton();
        if let Some(buffer) = buffer {
            buffer.update(cx, |buffer, cx| {
                buffer.set_text("", cx);
            });
        }
    }

    fn send_queued_now(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.queue.iter().position(|queued| queued.id == id) {
            let queued = self.queue.remove(index);
            let behavior = self.is_running(cx).then_some(ActiveTurnBehavior::Steer);
            self.send_queued(queued, behavior, cx);
            cx.notify();
        }
    }

    pub fn dictation_available(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store.status == crate::store::ConnectionStatus::Connected
            && store.server_info.dictation_enabled()
    }

    fn toggle_dictation(
        &mut self,
        _: &ToggleDictation,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.dictation.as_ref().map(|dictation| dictation.phase) {
            Some(DictationPhase::Recording) => self.stop_dictation(cx),
            Some(DictationPhase::Transcribing) => {}
            None => self.start_dictation(cx),
        }
    }

    fn report_error(&self, message: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.state.error = Some(message);
            cx.notify();
        });
    }

    fn start_dictation(&mut self, cx: &mut Context<Self>) {
        if !self.dictation_available(cx) {
            let reason =
                self.store.read(cx).server_info.capabilities["voice"]["dictation"]["reason"]
                    .as_str()
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_owned);
            self.report_error(
                reason.unwrap_or_else(|| "Dictation is not available on this Paseo host".into()),
                cx,
            );
            return;
        }
        let (audio, receiver) = async_channel::unbounded();
        let (recorder, format) = match start_capture(audio.clone()) {
            Ok(capture) => capture,
            Err(error) => {
                self.report_error(format!("{error:#}"), cx);
                return;
            }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let store = self.store.clone();
        let task = cx.spawn({
            let id = id.clone();
            async move |composer, cx| {
                let result = stream_dictation(&store, &id, format, receiver, cx).await;
                if let Err(error) = composer.update(cx, |composer, cx| {
                    let current = composer
                        .dictation
                        .as_ref()
                        .is_some_and(|dictation| dictation.id == id);
                    match result {
                        Ok(true) => {}
                        Ok(false) if current => composer.dictation = None,
                        Ok(false) => {}
                        Err(error) => {
                            if current {
                                composer.dictation = None;
                            }
                            composer.report_error(format!("Dictation failed: {error:#}"), cx);
                        }
                    }
                    cx.notify();
                }) {
                    log::debug!("Composer closed during dictation: {error}");
                }
            }
        });
        let ticker = cx.spawn(async move |composer, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if composer.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        self.dictation = Some(Dictation {
            id,
            phase: DictationPhase::Recording,
            recorder: Some(recorder),
            audio,
            started_at: std::time::Instant::now(),
            partial: None,
            _task: task,
            _ticker: ticker,
        });
        cx.notify();
    }

    fn stop_dictation(&mut self, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.as_mut() {
            dictation.recorder = None;
            dictation.phase = DictationPhase::Transcribing;
            if dictation.audio.try_send(AudioMessage::Stop).is_err() {
                log::debug!("Dictation audio stream already closed");
            }
        }
        cx.notify();
    }

    fn cancel_dictation(&mut self, cx: &mut Context<Self>) {
        if let Some(dictation) = self.dictation.take() {
            if dictation.audio.try_send(AudioMessage::Stop).is_err() {
                log::debug!("Dictation audio stream already closed");
            }
            let id = dictation.id;
            self.store
                .update(cx, |store, cx| {
                    store.session_request(cx, move |session| async move {
                        session.cancel_dictation(&id).await
                    })
                })
                .detach_and_log_err(cx);
        }
        cx.notify();
    }

    fn handle_dictation_event(
        &mut self,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let StoreEvent::Stream(event) = event else {
            return;
        };
        let Some(dictation) = self.dictation.as_mut() else {
            return;
        };
        match event {
            PaseoEvent::DictationPartial { dictation_id, text }
                if *dictation_id == dictation.id =>
            {
                dictation.partial = Some(text.clone()).filter(|text| !text.trim().is_empty());
                cx.notify();
            }
            PaseoEvent::DictationFinal { dictation_id, text } if *dictation_id == dictation.id => {
                self.dictation = None;
                let text = text.trim();
                if !text.is_empty() {
                    let current = self.text(cx);
                    let separator = if current.is_empty() || current.ends_with(char::is_whitespace)
                    {
                        ""
                    } else {
                        " "
                    };
                    let combined = format!("{current}{separator}{text}");
                    self.editor.update(cx, |editor, cx| {
                        editor.set_text(combined, window, cx);
                        editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
                    });
                }
                cx.notify();
            }
            PaseoEvent::DictationFailed {
                dictation_id,
                error,
            } if *dictation_id == dictation.id => {
                self.dictation = None;
                self.report_error(format!("Dictation failed: {error}"), cx);
            }
            _ => {}
        }
    }

    fn render_dictation(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let dictation = self.dictation.as_ref()?;
        let recording = dictation.phase == DictationPhase::Recording;
        let elapsed = dictation.started_at.elapsed().as_secs();
        let status = if recording {
            format!("Listening · {}:{:02}", elapsed / 60, elapsed % 60)
        } else {
            "Transcribing…".into()
        };
        Some(
            h_flex()
                .gap_2()
                .px_1()
                .child(if recording {
                    Indicator::dot().color(Color::Error).into_any_element()
                } else {
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .with_rotate_animation(2)
                        .into_any_element()
                })
                .child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children(dictation.partial.clone().map(|partial| {
                    div().flex_1().min_w_0().child(
                        Label::new(partial)
                            .size(LabelSize::Small)
                            .italic()
                            .single_line()
                            .truncate(),
                    )
                }))
                .child(div().flex_1())
                .when(recording, |this| {
                    this.child(
                        Button::new("paseo-dictation-done", "Done")
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|composer, _, _, cx| composer.stop_dictation(cx)),
                            ),
                    )
                })
                .child(
                    Button::new("paseo-dictation-cancel", "Cancel")
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .on_click(cx.listener(|composer, _, _, cx| composer.cancel_dictation(cx))),
                )
                .into_any_element(),
        )
    }

    fn render_last_turn_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.agent_id.as_ref()?;
        let focus = self.editor.focus_handle(cx);
        Some(
            IconButton::new("paseo-review-last-turn", IconName::FileDiff)
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(move |_window, cx| {
                    Tooltip::for_action_in("Review last turn", &ReviewLastTurn, &focus, cx)
                })
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ReviewLastTurn), cx))
                .into_any_element(),
        )
    }

    fn render_dictation_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.dictation_available(cx) {
            return None;
        }
        let active = self.dictation.is_some();
        let focus = self.editor.focus_handle(cx);
        Some(
            IconButton::new(
                "paseo-dictate",
                if active {
                    IconName::MicMute
                } else {
                    IconName::Mic
                },
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::Small)
            .icon_color(if active { Color::Error } else { Color::Muted })
            .toggle_state(active)
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in(
                    if active { "Stop dictation" } else { "Dictate" },
                    &ToggleDictation,
                    &focus,
                    cx,
                )
            })
            .on_click(cx.listener(|composer, _, window, cx| {
                composer.toggle_dictation(&ToggleDictation, window, cx)
            }))
            .into_any_element(),
        )
    }

    /// Whether the connected host can create worktrees for new agents.
    pub fn can_create_worktree(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .server_info
            .has_feature("workspaceMultiplicity")
    }

    pub fn uses_new_worktree(&self, cx: &App) -> bool {
        self.preferences.new_worktree && self.can_create_worktree(cx)
    }

    pub fn set_new_worktree(&mut self, new_worktree: bool, cx: &mut Context<Self>) {
        self.preferences.new_worktree = new_worktree;
        self.preferences.save(cx);
        self.load_default_base(cx);
        cx.notify();
    }

    /// The base a new worktree branches off; `None` lets the daemon use the repository's
    /// default branch.
    pub fn worktree_base(&self) -> Option<&BaseRef> {
        self.worktree_base
            .as_ref()
            .or(self.default_base.value.as_ref())
    }

    pub fn set_worktree_base(&mut self, base: BaseRef, cx: &mut Context<Self>) {
        self.worktree_base = Some(base);
        cx.notify();
    }

    fn load_default_base(&mut self, cx: &mut Context<Self>) {
        if self.agent_id.is_some() || !self.uses_new_worktree(cx) {
            return;
        }
        let Some(directory) = self.draft_directory.clone() else {
            return;
        };
        let key = (directory.clone(), self.store.read(cx).connection_count);
        if self.default_base.loaded_for.as_ref() == Some(&key) {
            return;
        }
        self.default_base.loaded_for = Some(key.clone());
        self.default_base.value = None;
        let Some(cwd) = directory.to_str().map(str::to_owned) else {
            return;
        };
        let status = self.store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.checkout_status(&cwd).await
            })
        });
        self.default_base._task = Some(cx.spawn(async move |this, cx| {
            let base = match status.await {
                Ok(status) => default_base_ref(&status),
                Err(error) => {
                    log::debug!("Paseo could not read the worktree base: {error}");
                    None
                }
            };
            if let Err(error) = this.update(cx, |composer, cx| {
                if composer.default_base.loaded_for.as_ref() == Some(&key) {
                    composer.default_base.value = base;
                    cx.notify();
                }
            }) {
                log::debug!("Paseo composer closed: {error}");
            }
        }));
    }

    /// Puts a rewound message back for editing, unless the user already started typing.
    pub fn restore_text_if_empty(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.text(cx).trim().is_empty() {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
        self.focus(window, cx);
    }

    /// Appends context sent from an editor after anything already typed, so a draft in progress
    /// is kept, then focuses the composer. Never sends.
    pub fn insert_context(&mut self, context: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let existing = editor.text(cx);
            let separator = if existing.trim().is_empty() || existing.ends_with("\n\n") {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
            editor.insert(&format!("{separator}{context}"), window, cx);
        });
        self.focus(window, cx);
    }

    fn edit_queued(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.queue.iter().position(|queued| queued.id == id) {
            let queued = self.queue.remove(index);
            self.editor.update(cx, |editor, cx| {
                editor.set_text(queued.text, window, cx);
            });
            self.images.extend(queued.images);
            self.focus(window, cx);
            cx.notify();
        }
    }

    fn render_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: crate::chat_font_size(cx).into(),
            font_weight: settings.ui_font.weight,
            line_height: relative(1.45),
            ..Default::default()
        };
        EditorElement::new(
            &self.editor,
            EditorStyle {
                background: cx.theme().colors().surface_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                syntax: cx.theme().syntax().clone(),
                ..Default::default()
            },
        )
    }

    fn picker_chip(id: &'static str, label: String, icon: Option<IconName>) -> ui::Button {
        ui::Button::new(id, label)
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .style(ButtonStyle::Subtle)
            .when_some(icon, |button, icon| {
                button.start_icon(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            })
            .end_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let choices = self.choices(cx);
        let provider = self.current_provider(cx).cloned();
        let focus = self.editor.focus_handle(cx);
        let is_draft = self.agent_id.is_none();
        let models = provider.as_ref().map(provider_models).unwrap_or_default();
        let model_label = choices
            .model
            .as_deref()
            .map(|model| {
                models
                    .iter()
                    .find(|option| option.id == model)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| model.to_owned())
            })
            .unwrap_or_else(|| "Default model".into());
        let thinking = match (&provider, &choices.model) {
            (Some(provider), Some(model)) => thinking_options(provider, model),
            _ => Vec::new(),
        };
        let modes = self.mode_options(cx);
        let providers = store.providers.clone();

        let this = cx.weak_entity();
        let provider_picker = is_draft.then(|| {
            let provider_label = provider
                .as_ref()
                .map(|provider| {
                    provider
                        .label
                        .clone()
                        .unwrap_or_else(|| provider.id.clone())
                })
                .unwrap_or_else(|| "No provider".into());
            let this = this.clone();
            let current = choices.provider.clone();
            PopoverMenu::new("paseo-provider-picker")
                .with_handle(self.provider_menu.clone())
                .trigger_with_tooltip(
                    Self::picker_chip(
                        "paseo-provider-chip",
                        provider_label,
                        Some(crate::sidebar::provider_icon(
                            choices.provider.as_deref().unwrap_or_default(),
                        )),
                    ),
                    Tooltip::text("Agent provider"),
                )
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let this = this.clone();
                    let providers = providers.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header("Provider");
                        for provider in providers {
                            let ready = provider.status == "ready";
                            let label = provider
                                .label
                                .clone()
                                .unwrap_or_else(|| provider.id.clone());
                            let label = if ready {
                                label
                            } else {
                                format!("{label} ({})", provider.status)
                            };
                            let this = this.clone();
                            let id = provider.id.clone();
                            menu = menu.toggleable_entry(
                                label,
                                current.as_deref() == Some(provider.id.as_str()),
                                ui::IconPosition::End,
                                None,
                                move |_, cx| {
                                    if let Err(error) = this.update(cx, |composer, cx| {
                                        composer.select_provider(id.clone(), cx)
                                    }) {
                                        log::debug!("Paseo composer closed: {error}");
                                    }
                                },
                            );
                        }
                        menu
                    }))
                })
        });

        let model_picker = (!models.is_empty()).then(|| {
            let this = this.clone();
            let current = choices.model.clone();
            let focus = focus.clone();
            PopoverMenu::new("paseo-model-picker")
                .with_handle(self.model_menu.clone())
                .trigger_with_tooltip(
                    Self::picker_chip("paseo-model-chip", model_label, None),
                    move |_window, cx| {
                        Tooltip::for_action_in("Change model", &ToggleModelPicker, &focus, cx)
                    },
                )
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let this = this.clone();
                    let models = models.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header("Model");
                        for model in models {
                            let this = this.clone();
                            let id = model.id.clone();
                            menu = menu.toggleable_entry(
                                model.label.clone(),
                                current.as_deref() == Some(model.id.as_str()),
                                ui::IconPosition::End,
                                None,
                                move |_, cx| {
                                    if let Err(error) = this.update(cx, |composer, cx| {
                                        composer.select_model(id.clone(), cx)
                                    }) {
                                        log::debug!("Paseo composer closed: {error}");
                                    }
                                },
                            );
                        }
                        menu
                    }))
                })
        });

        let thinking_picker = (!thinking.is_empty()).then(|| {
            let label = choices
                .thinking
                .as_deref()
                .and_then(|id| thinking.iter().find(|option| option.id == id))
                .map(|option| option.label.clone())
                .unwrap_or_else(|| "Thinking".into());
            let this = this.clone();
            let current = choices.thinking.clone();
            let focus = focus.clone();
            PopoverMenu::new("paseo-thinking-picker")
                .with_handle(self.thinking_menu.clone())
                .trigger_with_tooltip(
                    Self::picker_chip("paseo-thinking-chip", label, Some(IconName::ToolThink)),
                    move |_window, cx| {
                        Tooltip::for_action_in(
                            "Change thinking effort",
                            &ToggleThinkingPicker,
                            &focus,
                            cx,
                        )
                    },
                )
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let this = this.clone();
                    let thinking = thinking.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header("Thinking");
                        for option in thinking {
                            let this = this.clone();
                            let id = option.id.clone();
                            menu = menu.toggleable_entry(
                                option.label.clone(),
                                current.as_deref() == Some(option.id.as_str()),
                                ui::IconPosition::End,
                                None,
                                move |_, cx| {
                                    if let Err(error) = this.update(cx, |composer, cx| {
                                        composer.select_thinking(id.clone(), cx)
                                    }) {
                                        log::debug!("Paseo composer closed: {error}");
                                    }
                                },
                            );
                        }
                        menu
                    }))
                })
        });

        let mode_picker = (!modes.is_empty()).then(|| {
            let current_mode = choices
                .mode
                .as_deref()
                .and_then(|id| modes.iter().find(|option| option.id == id));
            let label = current_mode
                .map(|option| option.label.clone())
                .unwrap_or_else(|| "Mode".into());
            let color = mode_color(current_mode.and_then(|mode| mode.color_tier.as_deref()));
            let this = this.clone();
            let current = choices.mode.clone();
            let focus = focus.clone();
            PopoverMenu::new("paseo-mode-picker")
                .with_handle(self.mode_menu.clone())
                .trigger_with_tooltip(
                    Self::picker_chip("paseo-mode-chip", label, Some(IconName::Lock))
                        .start_icon(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(color),
                        )
                        .when(color != Color::Muted, |chip| chip.color(color)),
                    move |_window, cx| Tooltip::for_action_in("Cycle mode", &CycleMode, &focus, cx),
                )
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let this = this.clone();
                    let modes = modes.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.header("Mode");
                        for mode in modes {
                            let this = this.clone();
                            let id = mode.id.clone();
                            let entry_label = match &mode.description {
                                Some(description) if !description.is_empty() => {
                                    format!("{} — {description}", mode.label)
                                }
                                _ => mode.label.clone(),
                            };
                            menu = menu.toggleable_entry(
                                entry_label,
                                current.as_deref() == Some(mode.id.as_str()),
                                ui::IconPosition::End,
                                None,
                                move |_, cx| {
                                    if let Err(error) = this.update(cx, |composer, cx| {
                                        composer.select_mode(id.clone(), cx)
                                    }) {
                                        log::debug!("Paseo composer closed: {error}");
                                    }
                                },
                            );
                        }
                        menu
                    }))
                })
        });

        h_flex()
            .gap_0p5()
            .min_w_0()
            .flex_wrap()
            .children(provider_picker)
            .children(model_picker)
            .children(thinking_picker)
            .children(mode_picker)
    }

    fn render_context_meter(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let agent = self.agent(cx)?;
        let usage = agent.extra.get("lastUsage")?;
        let used = usage
            .get("contextWindowUsedTokens")
            .and_then(Value::as_u64)?;
        let max = usage
            .get("contextWindowMaxTokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                let choices = self.choices(cx);
                let provider = self.current_provider(cx)?;
                context_window_max(provider, choices.model.as_deref()?)
            })
            .filter(|max| *max > 0)?;
        let fraction = (used as f32 / max as f32).clamp(0., 1.);
        let status = cx.theme().status();
        let color = if fraction > 0.9 {
            status.error
        } else if fraction >= 0.7 {
            status.warning
        } else {
            cx.theme().colors().text_muted
        };
        let cost = usage.get("totalCostUsd").and_then(Value::as_f64);
        let tooltip = format!(
            "Context {:.0}% · {} / {} tokens{}",
            fraction * 100.,
            format_tokens(used),
            format_tokens(max),
            cost.map(|cost| format!(" · {}", format_cost(cost)))
                .unwrap_or_default()
        );
        // CircularProgress takes pixels, so convert at the chat's rem size to scale with zoom.
        let rem_size = crate::chat_font_size(cx);
        Some(
            div()
                .id("paseo-context-meter")
                .px_1()
                .tooltip(Tooltip::text(tooltip))
                .child(
                    CircularProgress::new(
                        used as f32,
                        max as f32,
                        rems_from_px(14_f32).to_pixels(rem_size),
                        cx,
                    )
                    .stroke_width(rems_from_px(2_f32).to_pixels(rem_size))
                    .progress_color(color)
                    .bg_color(cx.theme().colors().border),
                ),
        )
    }

    fn render_send_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let running = self.is_running(cx);
        let has_text = !self.text(cx).trim().is_empty() || !self.images.is_empty();
        let busy = self.pending_text.is_some() || self.creating;
        let focus = self.editor.focus_handle(cx);
        let colors = cx.theme().colors();
        if running && !has_text {
            return div()
                .id("paseo-stop")
                .size(rems_from_px(28_f32))
                .flex_none()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().status().error)
                .cursor_pointer()
                .hover(|style| style.opacity(0.85))
                .tooltip(move |_window, cx| {
                    Tooltip::for_action_in("Stop", &InterruptAgent, &focus, cx)
                })
                .on_click(cx.listener(|composer, _, window, cx| {
                    composer.interrupt(&InterruptAgent, window, cx)
                }))
                .child(
                    div()
                        .size(rems_from_px(10_f32))
                        .rounded(rems_from_px(2_f32))
                        .bg(gpui::white()),
                )
                .into_any_element();
        }
        let enabled = has_text && !busy;
        div()
            .id("paseo-send")
            .size(rems_from_px(28_f32))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .map(|this| {
                if enabled {
                    this.bg(colors.border_focused)
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.85))
                } else {
                    this.bg(colors.element_background)
                }
            })
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in(
                    if running { "Steer (send now)" } else { "Send" },
                    &SendMessageAction,
                    &focus,
                    cx,
                )
            })
            .on_click(cx.listener(|composer, _, _, cx| composer.submit(false, cx)))
            .child(if busy {
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element()
            } else {
                Icon::new(IconName::ArrowUp)
                    .size(IconSize::Small)
                    .color(if enabled {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_queue(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex().gap_1().children(self.queue.iter().map(|queued| {
            let id = queued.id;
            h_flex()
                .id(("paseo-queued", id))
                .gap_2()
                .px_2()
                .py_1()
                .rounded_lg()
                .bg(cx.theme().colors().surface_background)
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Icon::new(IconName::ListTodo)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(queued.text.lines().next().unwrap_or_default().to_owned())
                            .size(LabelSize::Default)
                            .truncate(),
                    ),
                )
                .child(
                    IconButton::new(("paseo-queued-edit", id), IconName::Pencil)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Edit"))
                        .on_click(cx.listener(move |composer, _, window, cx| {
                            composer.edit_queued(id, window, cx)
                        })),
                )
                .child(
                    IconButton::new(("paseo-queued-send", id), IconName::ArrowUp)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Send now"))
                        .on_click(
                            cx.listener(move |composer, _, _, cx| composer.send_queued_now(id, cx)),
                        ),
                )
                .child(
                    IconButton::new(("paseo-queued-remove", id), IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Remove"))
                        .on_click(cx.listener(move |composer, _, _, cx| {
                            composer.queue.retain(|queued| queued.id != id);
                            cx.notify();
                        })),
                )
        }))
    }
}

fn remember_sent_images(store: &mut PaseoStore, message_id: &str, images: &[PastedImage]) {
    if !images.is_empty() {
        store.sent_images.insert(
            message_id.to_owned(),
            images.iter().map(|pasted| pasted.image.clone()).collect(),
        );
    }
}

fn attachments(images: &[PastedImage]) -> Vec<ImageAttachment> {
    images.iter().map(PastedImage::attachment).collect()
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.)
    } else if tokens >= 1000 {
        format!("{:.0}K", tokens as f64 / 1000.)
    } else {
        tokens.to_string()
    }
}

pub fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.is_focused(window, cx);
        let colors = cx.theme().colors();
        let running = self.is_running(cx);
        let focus = self.editor.focus_handle(cx);
        v_flex()
            .key_context("PaseoComposer")
            .capture_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::send_action))
            .on_action(cx.listener(Self::queue_action))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(Self::cycle_mode))
            .on_action(cx.listener(Self::toggle_dictation))
            .on_action(cx.listener(|composer, _: &ToggleModelPicker, window, cx| {
                composer.model_menu.toggle(window, cx)
            }))
            .on_action(
                cx.listener(|composer, _: &ToggleThinkingPicker, window, cx| {
                    composer.thinking_menu.toggle(window, cx)
                }),
            )
            .on_action(cx.listener(|composer, _: &ToggleModePicker, window, cx| {
                composer.mode_menu.toggle(window, cx)
            }))
            .on_action(
                cx.listener(|composer, _: &FocusComposer, window, cx| composer.focus(window, cx)),
            )
            .w_full()
            .gap_1p5()
            .when(!self.queue.is_empty(), |this| {
                this.child(self.render_queue(cx))
            })
            .child(
                v_flex()
                    .id("paseo-composer")
                    .w_full()
                    .gap_2()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .rounded(rems_from_px(16_f32))
                    .bg(colors.surface_background)
                    .border_1()
                    .border_color(if focused {
                        colors.border_focused.opacity(0.6)
                    } else {
                        colors.border
                    })
                    .cursor_text()
                    .on_click(cx.listener(|composer, _, window, cx| composer.focus(window, cx)))
                    .when(!self.context_attachments.is_empty(), |this| {
                        this.child(self.render_context_attachments(cx))
                    })
                    .when(!self.images.is_empty(), |this| {
                        this.child(self.render_images(cx))
                    })
                    .child(div().px_1().child(self.render_editor(cx)))
                    .children(self.render_dictation(cx))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_between()
                            .child(self.render_controls(cx))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_1()
                                    .children(self.render_context_meter(cx))
                                    .children(self.render_last_turn_button(cx))
                                    .children(self.render_dictation_button(cx))
                                    .when(running && !self.text(cx).trim().is_empty(), |this| {
                                        this.child(
                                            IconButton::new("paseo-queue", IconName::ListTodo)
                                                .shape(IconButtonShape::Square)
                                                .icon_size(IconSize::Small)
                                                .icon_color(Color::Muted)
                                                .tooltip(move |_window, cx| {
                                                    Tooltip::for_action_in(
                                                        "Queue until the agent finishes",
                                                        &QueueMessage,
                                                        &focus,
                                                        cx,
                                                    )
                                                })
                                                .on_click(cx.listener(|composer, _, _, cx| {
                                                    composer.submit(true, cx)
                                                })),
                                        )
                                    })
                                    .child(self.render_send_button(cx)),
                            ),
                    ),
            )
    }
}

pub(crate) fn client_commands(is_draft: bool) -> Vec<(&'static str, &'static str)> {
    if is_draft {
        Vec::new()
    } else {
        vec![
            ("clear", "Archive this agent and start a new one"),
            ("exit", "Archive this agent"),
        ]
    }
}

struct PaseoCompletionProvider {
    composer: WeakEntity<Composer>,
}

fn token_before_cursor(text: &str, offset: usize) -> Option<(usize, &str)> {
    let prefix = text.get(..offset)?;
    let start = prefix
        .rfind(|character: char| character.is_whitespace())
        .map(|index| index + 1)
        .unwrap_or(0);
    let token = prefix.get(start..)?;
    (token.starts_with('/') && start == 0 || token.starts_with('@')).then_some((start, token))
}

impl CompletionProvider for PaseoCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = buffer_position.to_offset(&snapshot);
        let text = snapshot.text();
        let Some((start, token)) = token_before_cursor(&text, offset) else {
            return Task::ready(Ok(Vec::new()));
        };
        let replace_range = snapshot.anchor_before(start)..buffer_position;
        let match_start = Some(snapshot.anchor_after(start + 1));
        let Some(composer) = self.composer.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };
        if token.starts_with('/') {
            let composer = composer.read(cx);
            let mut completions = client_commands(composer.agent_id.is_none())
                .into_iter()
                .map(|(name, description)| (name.to_owned(), description.to_owned(), None))
                .collect::<Vec<_>>();
            if let Some(commands) = composer
                .store
                .read(cx)
                .commands
                .get(&composer.command_cache_key(cx))
            {
                completions.extend(commands.iter().map(|command| {
                    (
                        command.name.clone(),
                        command.description.clone(),
                        command.argument_hint.clone(),
                    )
                }));
            }
            let completions = completions
                .into_iter()
                .map(|(name, description, hint)| {
                    let label = match &hint {
                        Some(hint) if !hint.is_empty() => format!("/{name} {hint}"),
                        _ => format!("/{name}"),
                    };
                    Completion {
                        replace_range: replace_range.clone(),
                        new_text: format!("/{name} "),
                        label: CodeLabel::plain(label, Some(&name)),
                        documentation: (!description.is_empty()).then(|| {
                            project::lsp_store::CompletionDocumentation::SingleLine(
                                description.into(),
                            )
                        }),
                        source: CompletionSource::Custom,
                        icon_path: Some(IconName::Terminal.path().into()),
                        icon_color: None,
                        match_start,
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: None,
                        group: None,
                    }
                })
                .collect();
            return Task::ready(Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: false,
            }]));
        }
        let query = token.trim_start_matches('@').to_owned();
        let (store, cwd) = {
            let composer = composer.read(cx);
            let cwd = composer
                .agent(cx)
                .and_then(|agent| agent.directory.clone())
                .or_else(|| composer.draft_directory.clone())
                .and_then(|directory| directory.to_str().map(str::to_owned));
            (composer.store.clone(), cwd)
        };
        let Some(cwd) = cwd else {
            return Task::ready(Ok(Vec::new()));
        };
        let suggestions = store.update(cx, |store, cx| {
            store.directory_suggestions(query, Some(cwd), true, true, cx)
        });
        cx.background_spawn(async move {
            let suggestions = suggestions.await?;
            let completions = suggestions
                .into_iter()
                .map(|suggestion| {
                    let path = suggestion.path.trim_end_matches('/').to_owned();
                    let display = if suggestion.is_directory {
                        format!("{path}/")
                    } else {
                        path
                    };
                    Completion {
                        replace_range: replace_range.clone(),
                        new_text: format!("@{display} "),
                        label: CodeLabel::plain(display.clone(), Some(&display)),
                        documentation: None,
                        source: CompletionSource::Custom,
                        icon_path: Some(
                            if suggestion.is_directory {
                                IconName::Folder
                            } else {
                                IconName::File
                            }
                            .path()
                            .into(),
                        ),
                        icon_color: None,
                        match_start,
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: None,
                        group: None,
                    }
                })
                .collect();
            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
                is_incomplete: true,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let full_text = snapshot.text();
        if text == "/" || text == "@" {
            return token_before_cursor(&full_text, offset).is_some();
        }
        token_before_cursor(&full_text, offset).is_some() && !text.chars().any(char::is_whitespace)
    }

    fn sort_completions(&self) -> bool {
        false
    }
}

/// Streams microphone audio to the daemon until stopped. Returns whether the daemon now owes a
/// transcript (false when nothing was recorded).
async fn stream_dictation(
    store: &Entity<PaseoStore>,
    dictation_id: &str,
    format: CaptureFormat,
    receiver: async_channel::Receiver<AudioMessage>,
    cx: &mut AsyncApp,
) -> Result<bool> {
    let request = |cx: &mut AsyncApp, message: DictationRequest| {
        let dictation_id = dictation_id.to_owned();
        store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                match message {
                    DictationRequest::Start => session.start_dictation(&dictation_id).await,
                    DictationRequest::Chunk(sequence, samples) => {
                        session
                            .dictation_chunk(&dictation_id, sequence, &samples)
                            .await
                    }
                    DictationRequest::Finish(final_sequence) => {
                        session
                            .finish_dictation(&dictation_id, final_sequence)
                            .await
                    }
                    DictationRequest::Cancel => session.cancel_dictation(&dictation_id).await,
                }
            })
        })
    };
    request(cx, DictationRequest::Start).await?;
    let mut encoder = Pcm16Encoder::new(&format);
    let mut samples = Vec::with_capacity(CHUNK_SAMPLES * 2);
    let mut sequence = 0u64;
    loop {
        let audio = match receiver.recv().await {
            Ok(AudioMessage::Samples(audio)) => audio,
            Ok(AudioMessage::Failed(error)) => {
                request(cx, DictationRequest::Cancel).await?;
                anyhow::bail!(error);
            }
            Ok(AudioMessage::Stop) | Err(_) => break,
        };
        encoder.push(&audio, &mut samples);
        while samples.len() >= CHUNK_SAMPLES {
            let chunk = samples.drain(..CHUNK_SAMPLES).collect::<Vec<_>>();
            request(cx, DictationRequest::Chunk(sequence, chunk)).await?;
            sequence += 1;
        }
    }
    if !samples.is_empty() {
        request(
            cx,
            DictationRequest::Chunk(sequence, std::mem::take(&mut samples)),
        )
        .await?;
        sequence += 1;
    }
    let Some(final_sequence) = sequence.checked_sub(1) else {
        request(cx, DictationRequest::Cancel).await?;
        return Ok(false);
    };
    request(cx, DictationRequest::Finish(final_sequence)).await?;
    Ok(true)
}

enum DictationRequest {
    Start,
    Chunk(u64, Vec<i16>),
    Finish(u64),
    Cancel,
}

/// A branch name for a new worktree from the first words of the prompt, with a random suffix
/// so repeated prompts never collide, like Paseo's generated worktree slugs.
pub(crate) fn worktree_branch_name(prompt: &str) -> String {
    let mut slug = String::new();
    for word in prompt
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(5)
    {
        if slug.len() + word.len() + 1 > 40 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() {
        slug.push_str("agent");
    }
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{slug}-{}", suffix.get(..4).unwrap_or("0000"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider() -> Provider {
        Provider {
            id: "claude".into(),
            label: Some("Claude".into()),
            status: "ready".into(),
            extra: json!({
                "defaultModeId": "default",
                "modes": [{"id":"default","label":"Default"},{"id":"plan","label":"Plan"}],
                "models": [
                    {"id":"sonnet","label":"Sonnet","thinkingOptions":[{"id":"low","label":"Low"},{"id":"high","label":"High","isDefault":true}]},
                    {"id":"opus","label":"Opus","isDefault":true,"aliases":["opus-latest"],"contextWindowMaxTokens":200000},
                    {"id":"old","label":"Old","isSelectable":false}
                ]
            }),
        }
    }

    #[test]
    fn choices_resolve_like_paseo_forms() {
        let provider = provider();
        let models = provider_models(&provider);
        assert_eq!(models.len(), 2);
        assert_eq!(
            resolve_choice(&models, Some("sonnet"), None).as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            resolve_choice(&models, Some("old"), None).as_deref(),
            Some("opus")
        );
        assert_eq!(resolve_choice(&models, None, None).as_deref(), Some("opus"));
        let thinking = thinking_options(&provider, "sonnet");
        assert_eq!(
            resolve_choice(&thinking, None, None).as_deref(),
            Some("high")
        );
        assert_eq!(context_window_max(&provider, "opus-latest"), Some(200_000));
        let modes = choices(provider.extra.get("modes"));
        assert_eq!(
            resolve_choice(&modes, None, Some("default")).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn completion_tokens_only_trigger_at_valid_positions() {
        assert_eq!(token_before_cursor("/rev", 4), Some((0, "/rev")));
        assert_eq!(token_before_cursor("hi /rev", 7), None);
        assert_eq!(
            token_before_cursor("look at @src/ma", 15),
            Some((8, "@src/ma"))
        );
        assert_eq!(token_before_cursor("plain", 5), None);
    }

    fn suggestion(
        name: &str,
        date: i64,
        local: Option<bool>,
        remote: Option<bool>,
        divergence: Option<(u64, u64)>,
    ) -> BranchSuggestion {
        BranchSuggestion {
            name: name.into(),
            committer_date: Some(date),
            has_local: local,
            has_remote: remote,
            local_ahead: divergence.map(|(ahead, _)| ahead),
            local_behind: divergence.map(|(_, behind)| behind),
        }
    }

    #[test]
    fn base_choices_follow_paseo_rows() {
        let suggestions = [
            suggestion("main", 30, Some(true), Some(true), Some((0, 9))),
            suggestion("synced", 20, Some(true), Some(true), Some((0, 0))),
            suggestion("feature", 40, Some(true), Some(false), None),
            suggestion("legacy", 10, None, None, None),
        ];
        let refs = |choices: Vec<BaseRef>| {
            choices
                .into_iter()
                .map(|choice| (choice.ref_name, choice.detail))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            refs(base_ref_choices(&suggestions, None)),
            vec![
                ("refs/heads/feature".into(), Some("local".into())),
                ("refs/remotes/origin/main".into(), Some("origin".into())),
                ("refs/heads/main".into(), Some("local \u{2212}9".into())),
                ("refs/remotes/origin/synced".into(), Some("origin".into())),
                ("legacy".into(), None),
            ]
        );
        let pinned = BaseRef::from_ref("refs/remotes/origin/main".into());
        let choices = base_ref_choices(&suggestions, Some(&pinned));
        assert_eq!(choices[0], pinned);
        assert_eq!(
            choices
                .iter()
                .filter(|choice| choice.ref_name == pinned.ref_name)
                .count(),
            1
        );
    }

    #[test]
    fn default_base_prefers_the_upstream() {
        let mut status = CheckoutStatus {
            current_branch: Some("zaseo".into()),
            upstream_ref: Some("refs/remotes/fork/main".into()),
            ..Default::default()
        };
        let base = default_base_ref(&status).expect("base");
        assert_eq!(
            (base.label.as_str(), base.detail.as_deref()),
            ("main", Some("fork"))
        );
        status.upstream_ref = None;
        assert_eq!(
            default_base_ref(&status).map(|base| base.ref_name),
            Some("refs/heads/zaseo".into())
        );
        status.current_branch = None;
        assert_eq!(default_base_ref(&status), None);
    }

    #[test]
    fn draft_command_cache_key_includes_the_branch() {
        let directory = Some(Path::new("/tmp/project"));
        let main = draft_command_cache_key("claude", directory, Some("main"));
        assert_ne!(
            main,
            draft_command_cache_key("claude", directory, Some("feature"))
        );
        assert_ne!(main, draft_command_cache_key("claude", directory, None));
        assert_eq!(
            main,
            draft_command_cache_key("claude", directory, Some("main"))
        );
    }

    #[test]
    fn worktree_branch_names_are_git_safe_slugs() {
        let name = worktree_branch_name("Fix the login bug in auth/session.rs, please!");
        let (slug, suffix) = name.rsplit_once('-').expect("suffix");
        assert_eq!(slug, "fix-the-login-bug-in");
        assert_eq!(suffix.len(), 4);
        assert!(worktree_branch_name("日本語").starts_with("agent-"));
    }

    #[test]
    fn token_and_cost_formatting() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_400), "12K");
        assert_eq!(format_tokens(1_250_000), "1.2M");
        assert_eq!(format_cost(0.004), "$0.0040");
        assert_eq!(format_cost(1.5), "$1.50");
    }
}
