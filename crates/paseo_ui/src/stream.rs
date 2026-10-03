use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, ElementId, FontWeight, HighlightStyle,
    Hsla, IntoElement, Pixels, SharedString, SpringAnimation, SpringConfig, StyledText, TaskExt,
    Transformation, Window, ease_out_quint, prelude::*, relative,
};
use markdown::{MarkdownFont, MarkdownStyle};
use paseo_client::{PermissionRequest, RewindMode};
use serde_json::Value;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use ui::{
    CommonAnimationExt, ContextMenu, ContextMenuEntry, CopyButton, ElevationIndex, IconButton,
    PopoverMenu, Tooltip, prelude::*,
};

use settings::Settings as _;
use theme_settings::ThemeSettings;

use crate::agent_view::{AgentView, OpenedSection, Row, RowIdentity, content_max_width};
use crate::timeline::{
    DiffLineKind, FileChange, NoticeLevel, StreamContent, StreamItem, TodoEntry, ToolCall,
    ToolDisplay, ToolKind, ToolStatus, diff_stat, edit_diff_lines, format_duration,
    format_message_time, parse_subagent_log, tool_display,
};

const MARKDOWN_BODY: u8 = 0;
const MARKDOWN_DETAIL: u8 = 1;
const DETAIL_MAX_HEIGHT: f32 = 400.;
/// Corner radius, in pixels at the chat's font size, of the chat's standalone blocks: the
/// composer, message bubbles, images and cards. Pieces inside a block use `rounded_md`.
pub(crate) const CARD_RADIUS: f32 = 12.;
/// Text size of every step line between messages: tool groups, thinking, working, turn folds and
/// the subagent track, so none reads louder than another.
pub(crate) const STEP_LABEL_SIZE: LabelSize = LabelSize::Small;

/// How long something that just appeared takes to fade in.
pub(crate) const ENTRANCE: Duration = Duration::from_millis(180);
/// A quick spring with a hint of settle, for chevrons turning as their section opens or closes.
const CHEVRON_SPRING: SpringConfig = SpringConfig::new(420., 34., 1.);
/// How long the shimmer's bright band takes to cross a working label.
const SHIMMER_SWEEP: Duration = Duration::from_millis(2000);
/// Half the width, in characters, of the shimmer's bright band.
const SHIMMER_HALF_WIDTH: f32 = 5.;

/// Fades `element` in and lifts it by `rise` when `appeared_at` is recent, and renders it still
/// otherwise. Lists only render visible rows, so an animation keyed by id alone would replay each
/// time a row scrolls back into view; gating on when it appeared plays it once.
pub(crate) fn fade_in_since<E: Styled + IntoElement + 'static>(
    element: E,
    id: impl Into<ElementId>,
    appeared_at: Option<Instant>,
    rise: Pixels,
) -> AnyElement {
    // Twice the duration, so a first frame drawn a little late still finishes its animation.
    match appeared_at.filter(|appeared_at| appeared_at.elapsed() < ENTRANCE * 2) {
        Some(_) => element
            .relative()
            .with_animation(
                id,
                Animation::new(ENTRANCE).with_easing(ease_out_quint()),
                move |element, delta| element.opacity(delta).top(rise * (1. - delta)),
            )
            .into_any_element(),
        None => element.into_any_element(),
    }
}

/// A stable id for a row's entrance animation, so a row keeps its animation when rows are
/// inserted above it.
fn entrance_key(identity: Option<RowIdentity>) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    identity.hash(&mut hasher);
    hasher.finish()
}

/// A chevron that turns by `turn` of a full rotation as its section opens. A spring starts at its
/// target when first drawn, so it only moves when the user toggles it.
pub(crate) fn rotating_chevron(
    id: impl Into<ElementId>,
    icon: IconName,
    size: IconSize,
    open: bool,
    turn: f32,
    cx: &App,
) -> impl IntoElement {
    // An svg rather than an `Icon`, whose transform setter is private to the ui crate.
    gpui::svg()
        .path(icon.path())
        .size(size.rems())
        .flex_none()
        .text_color(Color::Muted.color(cx))
        .with_spring(
            id,
            SpringAnimation::new(CHEVRON_SPRING)
                .to(if open { 1_f32 } else { 0. })
                .with_epsilon(0.01),
            move |icon, progress| {
                icon.with_transformation(Transformation::rotate(chevron_rotation(progress, turn)))
            },
        )
}

/// The chevron's angle `progress` of the way to `turn` of a full rotation. In radians rather than
/// `percentage`, which asserts 0..=1 and so rejects a spring's overshoot past its target.
fn chevron_rotation(progress: f32, turn: f32) -> gpui::Radians {
    gpui::radians(progress * turn * std::f32::consts::TAU)
}

/// A step label with a bright band sweeping across it, for work in progress. Reduced motion
/// shows the band's start, off the text, so the label reads plain.
pub(crate) fn shimmer_label(id: impl Into<ElementId>, text: SharedString, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    // From the placeholder tone, since themes often set muted text close to full text, which
    // hides the band.
    let (base, bright) = (colors.text_placeholder, colors.text);
    div()
        .text_ui_sm(cx)
        .text_color(base)
        .with_animation(
            id,
            Animation::new(SHIMMER_SWEEP).repeat(),
            move |label, delta| {
                label.child(
                    StyledText::new(text.clone())
                        .with_highlights(shimmer_highlights(&text, delta, base, bright)),
                )
            },
        )
        .into_any_element()
}

/// Colours for each character under the shimmer's band at `progress` through a sweep: brightest
/// at the band's centre, fading to `base` at its edges. The band starts and ends off the text.
fn shimmer_highlights(
    text: &str,
    progress: f32,
    base: Hsla,
    bright: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let length = text.chars().count() as f32;
    let centre = progress * (length + SHIMMER_HALF_WIDTH * 2.) - SHIMMER_HALF_WIDTH;
    text.char_indices()
        .enumerate()
        .filter_map(|(position, (start, character))| {
            let strength = 1. - (position as f32 - centre).abs() / SHIMMER_HALF_WIDTH;
            (strength > 0.).then(|| {
                let color = base.blend(bright.opacity(strength));
                (
                    start..start + character.len_utf8(),
                    HighlightStyle {
                        color: Some(color),
                        ..Default::default()
                    },
                )
            })
        })
        .collect()
}

/// Raises a standalone block one level above the chat: the theme's elevated tone, a hairline
/// border and a soft shadow. Step details and code blocks sit a level lower, on the surface tone.
pub(crate) fn raised_card<E: Styled>(element: E, cx: &App) -> E {
    let colors = cx.theme().colors();
    element
        .bg(colors.elevated_surface_background)
        .border_1()
        .border_color(colors.border)
        .shadow(ElevationIndex::ElevatedSurface.shadow(cx))
}

/// How an expandable row draws: a tool group's header is always filled and has no body of its
/// own; Thinking opens into one filled box, like Paseo; other details open in a bordered box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpandableStyle {
    Bordered,
    Filled,
    ToolGroup,
}

/// A tool call's header, worked out when rows are built because rows re-render every frame
/// while an agent works, and a subagent's log can be long.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ToolSummary {
    pub(crate) display: ToolDisplay,
    pub(crate) subagent_action_count: usize,
}

/// The header of a tool call: its label, with a running subagent's latest action after its
/// summary.
pub(crate) fn tool_summary(call: &ToolCall, cwd: Option<&Path>) -> ToolSummary {
    let mut display = tool_display(call, cwd);
    let subagent_actions = (display.kind == ToolKind::SubAgent)
        .then(|| parse_subagent_log(detail_str(&call.detail, "log").unwrap_or_default()).0);
    if call.status == ToolStatus::Running
        && let Some(latest) = subagent_actions.as_ref().and_then(|actions| actions.last())
    {
        let latest = latest.describe();
        display.summary = Some(match display.summary.take() {
            Some(description) => format!("{description} · {latest}"),
            None => latest,
        });
    }
    ToolSummary {
        display,
        subagent_action_count: subagent_actions.as_ref().map_or(0, Vec::len),
    }
}

/// Items that render as a collapsed summary row with expandable details.
pub(crate) fn is_expandable(item: &StreamItem) -> bool {
    matches!(
        &item.content,
        StreamContent::Tool(_) | StreamContent::Reasoning { .. } | StreamContent::Todo { .. }
    )
}

fn tool_icon(kind: ToolKind) -> IconName {
    match kind {
        ToolKind::Shell => IconName::ToolTerminal,
        ToolKind::Read => IconName::Eye,
        ToolKind::Edit | ToolKind::Write => IconName::ToolPencil,
        ToolKind::Search => IconName::ToolSearch,
        ToolKind::Fetch => IconName::ToolWeb,
        ToolKind::SubAgent => IconName::ListTree,
        ToolKind::Plan => IconName::ListTodo,
        ToolKind::Thinking => IconName::ToolThink,
        ToolKind::Other => IconName::ToolHammer,
    }
}

fn detail_str<'a>(detail: &'a Value, key: &str) -> Option<&'a str> {
    detail
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn mono_block(text: String, cx: &App) -> AnyElement {
    div()
        .w_full()
        .font_buffer(cx)
        .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
        .text_color(cx.theme().colors().text_muted)
        .whitespace_normal()
        .child(text)
        .into_any_element()
}

fn render_diff(detail: &Value, cx: &App) -> AnyElement {
    render_diff_lines(edit_diff_lines(detail), cx)
}

pub(crate) fn render_diff_lines(lines: Vec<(DiffLineKind, String)>, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let added = cx.theme().status().created;
    let removed = cx.theme().status().deleted;
    v_flex()
        .w_full()
        .font_buffer(cx)
        .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
        .children(lines.into_iter().map(|(kind, text)| {
            let (marker, background, color) = match kind {
                DiffLineKind::Added => ("+", Some(added.opacity(0.15)), colors.text),
                DiffLineKind::Removed => ("-", Some(removed.opacity(0.12)), colors.text),
                DiffLineKind::Hunk => ("", None, colors.text_muted),
                DiffLineKind::Context => (" ", None, colors.text_muted),
            };
            h_flex()
                .w_full()
                .px_1()
                .when_some(background, |this, background| this.bg(background))
                .text_color(color)
                .child(div().w(rems_from_px(12_f32)).flex_none().child(marker))
                .child(div().flex_1().min_w_0().child(if text.is_empty() {
                    " ".to_owned()
                } else {
                    text
                }))
        }))
        .into_any_element()
}

fn render_search(detail: &Value, cx: &App) -> AnyElement {
    let mut sections = v_flex().gap_1();
    let counts = [
        detail
            .get("numFiles")
            .and_then(Value::as_u64)
            .map(|files| format!("{files} files")),
        detail
            .get("numMatches")
            .and_then(Value::as_u64)
            .map(|matches| format!("{matches} matches")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !counts.is_empty() {
        sections = sections.child(
            Label::new(counts.join(" · "))
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
    }
    if let Some(results) = detail.get("webResults").and_then(Value::as_array) {
        sections = sections.children(results.iter().filter_map(|result| {
            let title = result.get("title").and_then(Value::as_str)?.to_owned();
            let url = result.get("url").and_then(Value::as_str)?.to_owned();
            Some(
                v_flex()
                    .child(Label::new(title).size(LabelSize::Default))
                    .child(Label::new(url).size(LabelSize::Small).color(Color::Accent)),
            )
        }));
    }
    if let Some(paths) = detail.get("filePaths").and_then(Value::as_array) {
        let paths = paths
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if !paths.is_empty() {
            sections = sections.child(mono_block(paths, cx));
        }
    }
    if let Some(content) = detail_str(detail, "content") {
        sections = sections.child(mono_block(content.to_owned(), cx));
    }
    sections.into_any_element()
}

impl AgentView {
    pub(crate) fn fork(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(agent_id), Some(workspace)) = (
            self.agent_id.clone(),
            self.workspace
                .as_ref()
                .and_then(|workspace| workspace.upgrade()),
        ) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            crate::fork_agent(workspace, self.store.clone(), &agent_id, window, cx)
        });
    }

    /// Rewind modes the agent's provider supports, in Paseo's menu order.
    pub(crate) fn rewind_modes(&self, cx: &App) -> Vec<RewindMode> {
        let Some(agent) = self.agent(cx) else {
            return Vec::new();
        };
        let capabilities = &agent.extra["capabilities"];
        [
            ("supportsRewindConversation", RewindMode::Conversation),
            ("supportsRewindFiles", RewindMode::Files),
            ("supportsRewindBoth", RewindMode::Both),
        ]
        .into_iter()
        .filter(|(capability, _)| capabilities[*capability] == true)
        .map(|(_, mode)| mode)
        .collect()
    }

    fn render_rewind_menu(
        &self,
        key: u64,
        message_id: String,
        text: String,
        cx: &Context<Self>,
    ) -> AnyElement {
        let view = cx.weak_entity();
        let modes = self.rewind_modes(cx);
        PopoverMenu::new(("paseo-rewind", key))
            .trigger_with_tooltip(
                IconButton::new(("paseo-rewind-trigger", key), IconName::Undo)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted),
                Tooltip::text("Rewind to this message"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let view = view.clone();
                let message_id = message_id.clone();
                let text = text.clone();
                let modes = modes.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("This action cannot be undone").separator();
                    for mode in modes {
                        let (label, icon) = match mode {
                            RewindMode::Conversation => ("Rewind conversation", IconName::Chat),
                            RewindMode::Files => ("Rewind files", IconName::File),
                            RewindMode::Both => ("Rewind conversation and files", IconName::Undo),
                        };
                        let view = view.clone();
                        let message_id = message_id.clone();
                        let text = text.clone();
                        menu = menu.item(ContextMenuEntry::new(label).icon(icon).handler(
                            move |window, cx| {
                                if let Err(error) = view.update(cx, |view, cx| {
                                    view.rewind(message_id.clone(), mode, text.clone(), window, cx)
                                }) {
                                    log::debug!("Paseo agent view closed: {error}");
                                }
                            },
                        ));
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn rewind(
        &mut self,
        message_id: String,
        mode: RewindMode,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.agent_id.clone() else {
            return;
        };
        let task = self.store.update(cx, |store, cx| {
            store.session_request(cx, move |session| async move {
                session.rewind(&agent_id, &message_id, mode).await
            })
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            view.update_in(cx, |view, window, cx| match result {
                Ok(()) if mode != RewindMode::Files => {
                    view.composer.update(cx, |composer, cx| {
                        composer.restore_text_if_empty(text, window, cx)
                    });
                }
                Ok(()) => {}
                Err(error) => view.show_error(error, cx),
            })
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn show_error(&self, error: anyhow::Error, cx: &mut App) {
        match self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        {
            Some(workspace) => {
                workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))
            }
            None => log::error!("Paseo request failed: {error:#}"),
        }
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace
                .open_abs_path(
                    path,
                    workspace::OpenOptions {
                        visible: Some(workspace::OpenVisible::None),
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    }

    /// The chat's markdown style from this render, or a new one outside a render.
    fn chat_markdown_style(&self, window: &Window, cx: &App) -> MarkdownStyle {
        self.markdown_style
            .clone()
            .unwrap_or_else(|| Self::build_markdown_style(window, cx))
    }

    pub(crate) fn build_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
        let font_size = crate::chat_font_size(cx);
        let chat = &crate::PaseoSettings::get_global(cx).chat;
        let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        style.base_text_style.font_size = font_size.into();
        if let Some(font_family) = chat.font_family.clone() {
            style.base_text_style.font_family = font_family;
        }
        // Prose reads best at about one and a half lines, with paragraphs set apart by more
        // than a line gap; the markdown defaults are tighter for tooltips and hovers.
        style.base_text_style.line_height = relative(chat.line_height);
        style.paragraph_line_height = relative(chat.line_height);
        style.paragraph_spacing = font_size * 0.7;
        style.list_spacing = font_size * 0.35;
        // Code reads as code from its font and tint, like Zed's agent panel. Sized in rems of
        // the chat, it follows the chat font size and zoom instead of the editor's size.
        let colors = cx.theme().colors();
        style.inline_code.color = Some(colors.text);
        style.inline_code.background_color = Some(colors.editor_foreground.opacity(0.08));
        style.inline_code.font_size = Some(rems(0.9).into());
        // A symbol span stays styled as code; it is a link only to its click.
        style.link_callback = Some(std::rc::Rc::new(|url, _| {
            url.starts_with(crate::agent_view::SYMBOL_LINK_SCHEME)
                .then(gpui::TextStyleRefinement::default)
        }));
        style.code_block = style.code_block.rounded_md().bg(colors.surface_background);
        style
    }

    pub(crate) fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.rows.clone();
        let Some(row) = rows.get(index) else {
            return div().into_any_element();
        };
        let identity = row.identity();
        let content = match row {
            Row::LoadOlder { loading } => h_flex()
                .w_full()
                .justify_center()
                .py_2()
                .child(
                    ui::Button::new(
                        "paseo-load-older",
                        if *loading {
                            "Loading earlier messages…"
                        } else {
                            "Load earlier messages"
                        },
                    )
                    .label_size(LabelSize::Default)
                    .color(Color::Muted)
                    .disabled(*loading)
                    .on_click(cx.listener(|view, _, _, cx| view.load_older(cx))),
                )
                .into_any_element(),
            Row::Item {
                item,
                tool,
                expanded,
                streaming,
            } => self.render_item(item, tool.as_deref(), *expanded, *streaming, window, cx),
            Row::ToolGroup {
                key,
                label,
                running,
                failed,
                expanded,
            } => self.render_expandable(
                *key,
                IconName::ToolHammer,
                label.clone().into(),
                None,
                None,
                *running,
                *failed,
                *expanded,
                None,
                ExpandableStyle::ToolGroup,
                cx,
            ),
            Row::TurnFooter {
                first_item_key,
                has_text,
                duration_seconds,
                finished_at,
            } => self.render_turn_footer(
                *first_item_key,
                *has_text,
                *duration_seconds,
                *finished_at,
                cx,
            ),
            Row::Working { since, spinner } => self.render_working(*since, *spinner, cx),
            Row::Changes {
                files,
                expanded,
                show_all,
            } => self.render_changes(files, expanded, *show_all, cx),
            Row::Spacer => div().h(rems_from_px(16_f32)).into_any_element(),
            Row::TurnFold {
                key,
                duration_seconds,
                expanded,
            } => self.render_turn_fold(*key, *duration_seconds, *expanded, cx),
        };
        let appeared_at =
            identity.and_then(|identity| self.row_appeared_at.get(&identity).copied());
        fade_in_since(
            h_flex()
                .debug_selector(|| format!("paseo-chat-row-{index}"))
                .w_full()
                .justify_center()
                .px_4()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(content_max_width(cx))
                        .min_w_0()
                        .child(content),
                ),
            ("paseo-row-entrance", entrance_key(identity)),
            appeared_at,
            px(4.),
        )
    }

    fn render_working(
        &self,
        since: Option<chrono::DateTime<chrono::Utc>>,
        spinner: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let elapsed = since
            .map(|since| (chrono::Utc::now() - since).num_seconds())
            .filter(|seconds| *seconds >= 0)
            .map(format_duration);
        h_flex()
            .py_1p5()
            .gap_1p5()
            .when(spinner, |this| {
                this.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
            })
            .child(shimmer_label(
                "paseo-working-shimmer",
                match elapsed {
                    Some(elapsed) => format!("Working · {elapsed}").into(),
                    None => "Working".into(),
                },
                cx,
            ))
            .when(!self.is_subagent(), |this| {
                this.child(
                    Label::new("Esc to interrupt")
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                )
            })
            .into_any_element()
    }

    /// A finished turn's "Worked for" line: a click shows or hides the steps before its answer.
    fn render_turn_fold(
        &mut self,
        key: u64,
        duration_seconds: Option<i64>,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match duration_seconds {
            Some(seconds) => format!("Worked for {}", format_duration(seconds)),
            None => "Worked".to_owned(),
        };
        v_flex()
            .pt_2()
            .pb_1()
            .mb_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .id(("paseo-turn-fold", key))
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_turn(key, cx)))
                    .child(Label::new(label).size(STEP_LABEL_SIZE).color(Color::Muted))
                    .child(rotating_chevron(
                        ("paseo-turn-fold-chevron", key),
                        IconName::ChevronDown,
                        IconSize::Small,
                        expanded,
                        0.5,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_item(
        &mut self,
        item: &Rc<StreamItem>,
        tool: Option<&ToolSummary>,
        expanded: bool,
        streaming: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &item.content {
            StreamContent::User { text, message_id } => {
                self.render_user_message(item, text, message_id.as_deref(), window, cx)
            }
            StreamContent::Assistant { text } => {
                let markdown = self.markdown_for(item, MARKDOWN_BODY, text, cx);
                div()
                    .w_full()
                    .py_1p5()
                    .child(self.markdown_element(
                        markdown,
                        self.chat_markdown_style(window, cx),
                        cx,
                    ))
                    .into_any_element()
            }
            StreamContent::Reasoning { text } => {
                let body = expanded.then(|| {
                    let markdown = self.markdown_for(item, MARKDOWN_DETAIL, text, cx);
                    let mut style = self.chat_markdown_style(window, cx);
                    style.base_text_style.color = cx.theme().colors().text_muted;
                    div()
                        .child(self.markdown_element(markdown, style, cx))
                        .into_any_element()
                });
                self.render_expandable(
                    item.key,
                    IconName::ToolThink,
                    "Thinking".into(),
                    None,
                    None,
                    streaming,
                    false,
                    expanded,
                    body,
                    ExpandableStyle::Filled,
                    cx,
                )
            }
            StreamContent::Tool(call) => self.render_tool(item, call, tool, expanded, window, cx),
            StreamContent::Todo { items } => self.render_todo(item.key, items, expanded, cx),
            StreamContent::Notice { level, message } => render_notice(*level, message, cx),
            StreamContent::Compaction {
                loading,
                pre_tokens,
            } => render_compaction(*loading, *pre_tokens, cx),
        }
    }

    fn render_user_message(
        &mut self,
        item: &Rc<StreamItem>,
        text: &str,
        message_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let rewind = message_id
            .filter(|_| !self.rewind_modes(cx).is_empty())
            .map(|message_id| {
                self.render_rewind_menu(item.key, message_id.to_owned(), text.to_owned(), cx)
            });
        let markdown = self.markdown_for(item, MARKDOWN_BODY, text, cx);
        let group = SharedString::from(format!("paseo-user-{}", item.key));
        let copy_text = text.to_owned();
        let timestamp = item
            .timestamp
            .map(|timestamp| {
                format_message_time(
                    timestamp.with_timezone(&chrono::Local),
                    chrono::Local::now(),
                )
            })
            .unwrap_or_default();
        let sent_images = message_id
            .and_then(|message_id| self.store.read(cx).sent_images.get(message_id))
            .cloned()
            .unwrap_or_default();
        let show_bubble = !text.trim().is_empty() || sent_images.is_empty();
        let fixed_width = needs_fixed_bubble_width(text);
        v_flex()
            .id(("paseo-user-message", item.key))
            .group(group.clone())
            .w_full()
            .items_end()
            .pt_3()
            .gap_0p5()
            .when(!sent_images.is_empty(), |column| {
                column.child(
                    h_flex()
                        .gap_1p5()
                        .flex_wrap()
                        .justify_end()
                        .max_w(relative(0.85))
                        .children(sent_images.into_iter().map(|image| {
                            div()
                                .rounded(rems_from_px(CARD_RADIUS))
                                .overflow_hidden()
                                .border_1()
                                .border_color(colors.border)
                                .child(
                                    gpui::img(image)
                                        .max_w(rems_from_px(240_f32))
                                        .max_h(rems_from_px(180_f32))
                                        .object_fit(gpui::ObjectFit::Contain),
                                )
                        })),
                )
            })
            .when(show_bubble, |column| {
                column.child(
                    div()
                        .min_w_0()
                        .max_w(relative(0.85))
                        .when(fixed_width, |bubble| bubble.w(relative(0.85)))
                        .px_4()
                        .py_2p5()
                        .rounded(rems_from_px(CARD_RADIUS))
                        .rounded_tr(rems_from_px(4_f32))
                        .map(|bubble| raised_card(bubble, cx))
                        .child(self.markdown_element(
                            markdown,
                            self.chat_markdown_style(window, cx),
                            cx,
                        )),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .h(rems_from_px(20_f32))
                    .visible_on_hover(group)
                    .child(
                        Label::new(timestamp)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(rewind)
                    .child(CopyButton::new(
                        SharedString::from(format!("paseo-copy-user-{}", item.key)),
                        copy_text,
                    )),
            )
            .into_any_element()
    }

    fn render_todo(
        &mut self,
        key: u64,
        items: &[TodoEntry],
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let completed = items.iter().filter(|entry| entry.completed).count();
        let current = items
            .iter()
            .find(|entry| entry.in_progress)
            .or_else(|| items.iter().find(|entry| !entry.completed))
            .map(|entry| entry.text.clone());
        let body = expanded.then(|| {
            v_flex()
                .gap_1()
                .children(items.iter().map(|entry| {
                    h_flex()
                        .gap_2()
                        .child(
                            Icon::new(if entry.completed {
                                IconName::TodoComplete
                            } else if entry.in_progress {
                                IconName::TodoProgress
                            } else {
                                IconName::TodoPending
                            })
                            .size(IconSize::Small)
                            .color(if entry.completed {
                                Color::Success
                            } else if entry.in_progress {
                                Color::Accent
                            } else {
                                Color::Muted
                            }),
                        )
                        .child(
                            Label::new(entry.text.clone())
                                .size(LabelSize::Default)
                                .color(if entry.completed {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .when(entry.completed, |label| label.strikethrough()),
                        )
                }))
                .into_any_element()
        });
        self.render_expandable(
            key,
            IconName::ListTodo,
            format!("Tasks {completed}/{}", items.len()).into(),
            current,
            None,
            false,
            false,
            expanded,
            body,
            ExpandableStyle::Bordered,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_expandable(
        &mut self,
        key: u64,
        icon: IconName,
        label: SharedString,
        summary: Option<String>,
        trailing: Option<AnyElement>,
        running: bool,
        failed: bool,
        expanded: bool,
        body: Option<AnyElement>,
        style: ExpandableStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let is_group = style == ExpandableStyle::ToolGroup;
        let group = SharedString::from(if is_group {
            format!("paseo-tool-group-{key}")
        } else {
            format!("paseo-row-{key}")
        });
        let has_body = is_group || body.is_some() || !expanded;
        let icon_element = if running {
            Icon::new(IconName::LoadCircle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .with_rotate_animation(2)
                .into_any_element()
        } else {
            Icon::new(icon)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        v_flex()
            .w_full()
            .py_0p5()
            .child(
                h_flex()
                    .id((
                        if is_group {
                            "paseo-tool-group"
                        } else {
                            "paseo-row"
                        },
                        key,
                    ))
                    .group(group.clone())
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .ml(rems_from_px(-8_f32))
                    .mr(rems_from_px(-8_f32))
                    .rounded_md()
                    .when(expanded && !is_group, |this| {
                        this.bg(colors.surface_background).rounded_b_none()
                    })
                    .when(has_body, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(colors.ghost_element_hover))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if is_group {
                                    view.toggle_group(key, cx)
                                } else {
                                    view.toggle_expanded(key, cx)
                                }
                            }))
                    })
                    .child(div().flex_none().child(icon_element))
                    .child(if running {
                        shimmer_label(
                            (
                                if is_group {
                                    "paseo-tool-group-shimmer"
                                } else {
                                    "paseo-row-shimmer"
                                },
                                key,
                            ),
                            label,
                            cx,
                        )
                    } else {
                        // A tool group is a quiet line between the agent's messages, as in T3 Code.
                        Label::new(label)
                            .size(STEP_LABEL_SIZE)
                            .color(if expanded {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .into_any_element()
                    })
                    .when_some(summary, |this, summary| {
                        this.child(
                            div().min_w_0().flex_shrink_1().child(
                                Label::new(summary)
                                    .size(STEP_LABEL_SIZE)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                    })
                    .children(trailing)
                    .when(failed, |this| {
                        this.child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .when(!expanded, |this| this.visible_on_hover(group))
                            .child(rotating_chevron(
                                (
                                    if is_group {
                                        "paseo-tool-group-chevron"
                                    } else {
                                        "paseo-row-chevron"
                                    },
                                    key,
                                ),
                                IconName::ChevronDown,
                                IconSize::Small,
                                expanded,
                                0.5,
                                cx,
                            )),
                    ),
            )
            .when_some(body, |this, body| {
                let opened_at = self.opened_at.get(&OpenedSection::Step(key)).copied();
                this.child(fade_in_since(
                    div()
                        .id(("paseo-row-detail", key))
                        .ml(rems_from_px(-8_f32))
                        .mr(rems_from_px(-8_f32))
                        .max_h(rems_from_px(DETAIL_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_b_md()
                        .map(|this| match style {
                            ExpandableStyle::Filled => this.bg(colors.surface_background),
                            _ => this.border_1().border_color(colors.border_variant),
                        })
                        .child(body),
                    ("paseo-row-detail-fade", key),
                    opened_at,
                    px(0.),
                ))
            })
            .into_any_element()
    }

    fn render_tool(
        &mut self,
        item: &Rc<StreamItem>,
        call: &ToolCall,
        tool: Option<&ToolSummary>,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = item.key;
        let summary;
        let tool = match tool {
            Some(tool) => tool,
            None => {
                summary = tool_summary(call, self.directory(cx).as_deref());
                &summary
            }
        };
        let display = &tool.display;
        let detail = &call.detail;
        let trailing = match display.kind {
            ToolKind::Edit => detail_str(detail, "unifiedDiff")
                .map(diff_stat)
                .or_else(|| {
                    let old = detail_str(detail, "oldString").map(|text| text.lines().count());
                    let new = detail_str(detail, "newString").map(|text| text.lines().count());
                    (old.is_some() || new.is_some()).then(|| (new.unwrap_or(0), old.unwrap_or(0)))
                })
                .map(|(added, removed)| {
                    h_flex()
                        .gap_1()
                        .flex_none()
                        .child(
                            Label::new(format!("+{added}"))
                                .size(LabelSize::Small)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("-{removed}"))
                                .size(LabelSize::Small)
                                .color(Color::Deleted),
                        )
                        .into_any_element()
                }),
            ToolKind::Shell => detail
                .get("exitCode")
                .and_then(Value::as_i64)
                .filter(|code| *code != 0)
                .map(|code| {
                    Label::new(format!("exit {code}"))
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .into_any_element()
                }),
            ToolKind::SubAgent => {
                Some(self.render_subagent_trailing(key, call, tool.subagent_action_count, cx))
            }
            _ => None,
        };
        let open_file = matches!(
            display.kind,
            ToolKind::Read | ToolKind::Edit | ToolKind::Write
        )
        .then(|| detail_str(detail, "filePath"))
        .flatten()
        .filter(|path| paseo_client::is_absolute_workspace_path(path))
        .filter(|_| self.store.read(cx).is_local_host())
        .map(PathBuf::from);
        let trailing = match (trailing, open_file) {
            (trailing, Some(path)) => Some(
                h_flex()
                    .gap_1()
                    .children(trailing)
                    .child(
                        IconButton::new(("paseo-open-file", key), IconName::ArrowUpRight)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Open in editor"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                view.open_file(path.clone(), window, cx);
                            })),
                    )
                    .into_any_element(),
            ),
            (trailing, None) => trailing,
        };
        let body = expanded.then(|| self.render_tool_detail(item, call, display.kind, window, cx));
        self.render_expandable(
            key,
            tool_icon(display.kind),
            display.label.clone().into(),
            display.summary.clone(),
            trailing,
            call.status == ToolStatus::Running,
            call.status == ToolStatus::Failed,
            expanded,
            body,
            ExpandableStyle::Bordered,
            cx,
        )
    }

    /// The action count, and an Open button once the daemon lists the subagent this call started.
    fn render_subagent_trailing(
        &self,
        key: u64,
        call: &ToolCall,
        action_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Inside a subagent tab, nested subagents are listed under the top-level agent.
        let parent_agent_id = self.agent_id.as_deref().map(|timeline_id| {
            paseo_client::parse_subagent_timeline_id(timeline_id)
                .map_or(timeline_id, |(parent_agent_id, _)| parent_agent_id)
                .to_owned()
        });
        let subagent_id = parent_agent_id.as_deref().and_then(|parent_agent_id| {
            self.store
                .read(cx)
                .state
                .subagents_for(parent_agent_id)
                .iter()
                .find(|subagent| subagent.tool_call_id.as_deref() == Some(call.call_id.as_str()))
                .map(|subagent| subagent.id.clone())
        });
        h_flex()
            .gap_1()
            .flex_none()
            .when(action_count > 0, |this| {
                this.child(
                    Label::new(if action_count == 1 {
                        "1 action".to_owned()
                    } else {
                        format!("{action_count} actions")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when_some(
                parent_agent_id.zip(subagent_id),
                |this, (parent_agent_id, subagent_id)| {
                    this.child(
                        IconButton::new(("paseo-open-subagent", key), IconName::ArrowUpRight)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Open subagent conversation"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                view.open_subagent(&parent_agent_id, &subagent_id, window, cx);
                            })),
                    )
                },
            )
            .into_any_element()
    }

    fn render_tool_detail(
        &mut self,
        item: &Rc<StreamItem>,
        call: &ToolCall,
        kind: ToolKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = &call.detail;
        let mut body = v_flex().w_full().gap_2();
        match kind {
            ToolKind::Shell => {
                let command = detail_str(detail, "command").unwrap_or_default();
                let output = detail_str(detail, "output")
                    .or_else(|| detail_str(detail, "log"))
                    .unwrap_or_default();
                let text = if output.is_empty() {
                    format!("$ {command}")
                } else {
                    format!("$ {command}\n\n{}", output.trim_end())
                };
                body = body.child(mono_block(text, cx));
            }
            ToolKind::Edit | ToolKind::Write => {
                if detail_str(detail, "unifiedDiff").is_some()
                    || detail_str(detail, "oldString").is_some()
                    || detail_str(detail, "newString").is_some()
                    || (kind == ToolKind::Write && detail_str(detail, "content").is_some())
                {
                    body = body.child(render_diff(detail, cx));
                }
            }
            ToolKind::Read => {
                if let Some(content) = detail_str(detail, "content") {
                    body = body.child(mono_block(content.to_owned(), cx));
                }
            }
            ToolKind::Search => body = body.child(render_search(detail, cx)),
            ToolKind::Fetch => {
                if let Some(url) = detail_str(detail, "url") {
                    body = body.child(
                        Label::new(url.to_owned())
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    );
                }
                if let Some(result) = detail_str(detail, "result") {
                    let markdown = self.markdown_for(item, MARKDOWN_DETAIL, result, cx);
                    body = body.child(self.markdown_element(
                        markdown,
                        self.chat_markdown_style(window, cx),
                        cx,
                    ));
                }
            }
            ToolKind::SubAgent => {
                if let Some(description) = detail_str(detail, "description") {
                    body = body.child(Label::new(description.to_owned()).size(LabelSize::Default));
                }
                if let Some(session) = detail_str(detail, "childSessionId") {
                    body = body.child(
                        Label::new(format!("session {session}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
                let (actions, remaining) =
                    parse_subagent_log(detail_str(detail, "log").unwrap_or_default());
                if !actions.is_empty() {
                    body = body.child(v_flex().gap_0p5().children(
                        actions.into_iter().enumerate().map(|(index, action)| {
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    Label::new(format!("{}.", index + 1))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(action.tool_label())
                                        .size(LabelSize::Default)
                                        .weight(FontWeight::MEDIUM),
                                )
                                .children(action.summary.map(|summary| {
                                    Label::new(summary)
                                        .size(LabelSize::Default)
                                        .color(Color::Muted)
                                        .truncate()
                                }))
                        }),
                    ));
                }
                if !remaining.trim().is_empty() {
                    body = body.child(mono_block(remaining, cx));
                }
            }
            ToolKind::Plan => {
                if let Some(text) = detail_str(detail, "text") {
                    let markdown = self.markdown_for(item, MARKDOWN_DETAIL, text, cx);
                    body = body.child(self.markdown_element(
                        markdown,
                        self.chat_markdown_style(window, cx),
                        cx,
                    ));
                }
            }
            ToolKind::Thinking | ToolKind::Other => {
                if let Some(text) = detail_str(detail, "text") {
                    body = body.child(mono_block(text.to_owned(), cx));
                } else {
                    for (label, value) in [
                        ("Input", detail.get("input")),
                        ("Output", detail.get("output")),
                    ] {
                        if let Some(value) = value.filter(|value| !value.is_null()) {
                            let text = match value {
                                Value::String(text) => text.clone(),
                                other => {
                                    serde_json::to_string_pretty(other).unwrap_or_else(|error| {
                                        log::warn!("Paseo could not format tool {label}: {error}");
                                        String::new()
                                    })
                                }
                            };
                            body = body
                                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                                .child(mono_block(text, cx));
                        }
                    }
                }
            }
        }
        if let Some(error) = &call.error {
            body = body.child(
                div()
                    .font_buffer(cx)
                    .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
                    .text_color(cx.theme().status().error)
                    .child(error.clone()),
            );
        }
        body.into_any_element()
    }

    fn render_changes(
        &self,
        files: &[FileChange],
        expanded: &[bool],
        show_all: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const COLLAPSED_FILES: usize = 3;
        let colors = cx.theme().colors().clone();
        let cwd = self.directory(cx);
        let local = self.store.read(cx).is_local_host();
        let additions: usize = files.iter().map(|file| file.additions).sum();
        let deletions: usize = files.iter().map(|file| file.deletions).sum();
        let visible = if show_all {
            files.len()
        } else {
            files.len().min(COLLAPSED_FILES)
        };
        let stat = |additions: usize, deletions: usize| {
            h_flex()
                .gap_1()
                .child(
                    Label::new(format!("+{additions}"))
                        .size(LabelSize::Small)
                        .color(Color::Created),
                )
                .child(
                    Label::new(format!("-{deletions}"))
                        .size(LabelSize::Small)
                        .color(Color::Deleted),
                )
        };
        v_flex()
            .id("paseo-turn-changes")
            .w_full()
            .my_1()
            .rounded(rems_from_px(CARD_RADIUS))
            .map(|card| raised_card(card, cx))
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .child(
                        Icon::new(IconName::FileDiff)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(if files.len() == 1 {
                            "1 file changed".to_owned()
                        } else {
                            format!("{} files changed", files.len())
                        })
                        .size(LabelSize::Default),
                    )
                    .child(stat(additions, deletions))
                    .child(div().flex_1())
                    .child(
                        ui::Button::new("paseo-review-turn", "Review")
                            .label_size(LabelSize::Default)
                            .tooltip(Tooltip::text("Open the last turn as a diff"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.mark_focused(cx);
                                window.dispatch_action(Box::new(crate::ReviewLastTurn), cx);
                            })),
                    ),
            )
            .children(files.iter().take(visible).enumerate().map(|(index, file)| {
                let is_expanded = expanded.get(index).copied().unwrap_or(false);
                let path = file.path.clone();
                let display = crate::timeline::relative_path(&file.path, cwd.as_deref());
                let absolute = PathBuf::from(&file.path);
                let opened_at = self
                    .opened_at
                    .get(&OpenedSection::Change(file.path.clone()))
                    .copied();
                v_flex()
                    .w_full()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        h_flex()
                            .id(("paseo-turn-change", index))
                            .group("paseo-turn-change-row")
                            .px_3()
                            .h(rems_from_px(28_f32))
                            .gap_1p5()
                            .cursor_pointer()
                            .hover(|this| this.bg(colors.ghost_element_hover))
                            .child(rotating_chevron(
                                ("paseo-turn-change-chevron", index),
                                IconName::ChevronRight,
                                IconSize::Small,
                                is_expanded,
                                0.25,
                                cx,
                            ))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(display)
                                        .size(LabelSize::Default)
                                        .single_line()
                                        .truncate(),
                                ),
                            )
                            .when(local, |this| {
                                this.child(
                                    IconButton::new(
                                        ("paseo-open-change", index),
                                        IconName::ArrowUpRight,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .icon_color(Color::Muted)
                                    .visible_on_hover("paseo-turn-change-row")
                                    .tooltip(Tooltip::text("Open file"))
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            cx.stop_propagation();
                                            view.open_file(absolute.clone(), window, cx);
                                        },
                                    )),
                                )
                            })
                            .child(stat(file.additions, file.deletions))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.expanded_changes.remove(&path) {
                                    view.expanded_changes.insert(path.clone());
                                    view.mark_opened(OpenedSection::Change(path.clone()));
                                }
                                view.rebuild(cx);
                            })),
                    )
                    .when(is_expanded, |this| {
                        this.child(fade_in_since(
                            div()
                                .id(("paseo-turn-change-diff", index))
                                .max_h(rems_from_px(DETAIL_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .py_1()
                                .bg(colors.editor_background)
                                .child(render_diff_lines(file.lines.clone(), cx)),
                            ("paseo-turn-change-diff-fade", index),
                            opened_at,
                            px(0.),
                        ))
                    })
            }))
            .when(files.len() > COLLAPSED_FILES, |this| {
                let hidden = files.len() - COLLAPSED_FILES;
                this.child(
                    h_flex()
                        .id("paseo-turn-changes-more")
                        .px_3()
                        .h(rems_from_px(26_f32))
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.ghost_element_hover))
                        .child(
                            Label::new(if show_all {
                                "Show fewer".to_owned()
                            } else {
                                format!("Show {hidden} more")
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.show_all_changes = !view.show_all_changes;
                            view.rebuild(cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn render_turn_footer(
        &mut self,
        first_item_key: Option<u64>,
        has_text: bool,
        duration_seconds: Option<i64>,
        finished_at: Option<chrono::DateTime<chrono::Utc>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_agent_output = has_text || duration_seconds.is_some();
        if !has_agent_output {
            return div().h(rems_from_px(4_f32)).into_any_element();
        }
        let id = first_item_key.unwrap_or_default();
        let copy_text = self.turn_copy_text(first_item_key).filter(|_| has_text);
        h_flex()
            .pb_2()
            .gap_1()
            .when_some(copy_text, |this, text| {
                this.child(CopyButton::new(
                    SharedString::from(format!("paseo-copy-turn-{id}")),
                    text,
                ))
            })
            .when(!self.is_subagent(), |this| {
                this.child(
                    IconButton::new(("paseo-fork-turn", id), IconName::GitBranchPlus)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Fork in a new tab"))
                        .on_click(cx.listener(|view, _, window, cx| view.fork(window, cx))),
                )
            })
            .when_some(duration_seconds, |this, seconds| {
                this.child(
                    Label::new(format!("Worked for {}", format_duration(seconds)))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(finished_at, |this, finished_at| {
                let finished = format_message_time(
                    finished_at.with_timezone(&chrono::Local),
                    chrono::Local::now(),
                );
                this.child(
                    Label::new(if duration_seconds.is_some() {
                        format!("· {finished}")
                    } else {
                        format!("Finished {finished}")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .into_any_element()
    }
}

/// A short preview of what a permission request would do.
pub(crate) fn permission_preview(request: &PermissionRequest, cx: &App) -> Option<AnyElement> {
    let extra = &request.extra;
    if let Some(plan) = extra
        .get("metadata")
        .and_then(|metadata| metadata.get("planText"))
        .or_else(|| extra.get("input").and_then(|input| input.get("plan")))
        .and_then(Value::as_str)
    {
        return Some(mono_block(plan.to_owned(), cx));
    }
    let detail = extra.get("detail")?;
    match detail.get("type").and_then(Value::as_str) {
        Some("shell") => {
            detail_str(detail, "command").map(|command| mono_block(format!("$ {command}"), cx))
        }
        Some("edit") | Some("write") => Some(
            v_flex()
                .gap_1()
                .when_some(detail_str(detail, "filePath"), |this, path| {
                    this.child(
                        Label::new(path.to_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .child(render_diff(detail, cx))
                .into_any_element(),
        ),
        Some(_) => {
            let text = detail_str(detail, "filePath")
                .or_else(|| detail_str(detail, "url"))
                .or_else(|| detail_str(detail, "query"))
                .map(str::to_owned)
                .or_else(|| {
                    extra
                        .get("input")
                        .filter(|input| !input.is_null())
                        .and_then(|input| serde_json::to_string_pretty(input).ok())
                })?;
            Some(mono_block(text, cx))
        }
        None => None,
    }
}

fn render_notice(level: NoticeLevel, message: &str, cx: &App) -> AnyElement {
    let status = cx.theme().status();
    let (icon, color, background) = match level {
        NoticeLevel::Info => (IconName::Info, Color::Info, status.info_background),
        NoticeLevel::Warning => (IconName::Warning, Color::Warning, status.warning_background),
        NoticeLevel::Error => (IconName::XCircle, Color::Error, status.error_background),
    };
    h_flex()
        .my_1()
        .px_3()
        .py_2p5()
        .gap_2()
        .items_start()
        .rounded_md()
        .bg(background)
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_ui(cx)
                .text_color(cx.theme().colors().text)
                .child(message.to_owned()),
        )
        .into_any_element()
}

fn render_compaction(loading: bool, pre_tokens: Option<u64>, cx: &App) -> AnyElement {
    let border = cx.theme().colors().border_variant;
    let label = if loading {
        "Compacting…".to_owned()
    } else {
        match pre_tokens {
            Some(tokens) => format!(
                "Context compacted ({})",
                crate::composer::format_tokens(tokens)
            ),
            None => "Context compacted".to_owned(),
        }
    };
    h_flex()
        .py_3()
        .gap_2()
        .child(div().flex_1().h_px().bg(border))
        .child(
            Icon::new(if loading {
                IconName::LoadCircle
            } else {
                IconName::Scissors
            })
            .size(IconSize::Small)
            .color(Color::Muted),
        )
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(div().flex_1().h_px().bg(border))
        .into_any_element()
}

/// Markdown lists lay out at zero width until given one (see `push_markdown_list_item`), and
/// tables and code blocks fill their container, so a bubble sized to its content would squeeze
/// them to the width of its plain lines.
fn needs_fixed_bubble_width(text: &str) -> bool {
    text.lines().map(str::trim_start).any(|line| {
        let after_number = line.trim_start_matches(|character: char| character.is_ascii_digit());
        let numbered = after_number.len() < line.len()
            && (after_number.starts_with(". ") || after_number.starts_with(") "));
        numbered
            || ["- ", "* ", "+ ", "```", "~~~", "|"]
                .iter()
                .any(|marker| line.starts_with(marker))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chevron_rotation_accepts_spring_overshoot() {
        // A spring passes its target before settling; `percentage` panics outside 0..=1.
        assert!(chevron_rotation(1.08, 0.5).0 > std::f32::consts::PI);
        assert!(chevron_rotation(-0.05, 0.25).0 < 0.);
        assert_eq!(chevron_rotation(1., 0.25).0, std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn shimmer_band_brightens_the_characters_under_it() {
        let base = gpui::hsla(0., 0., 0.4, 1.);
        let bright = gpui::hsla(0., 0., 0.9, 1.);
        let text = "Working · 3m";
        let length = text.chars().count() as f32;
        let band_on =
            |position: f32| (position + SHIMMER_HALF_WIDTH) / (length + SHIMMER_HALF_WIDTH * 2.);

        assert!(shimmer_highlights(text, 0., base, bright).is_empty());
        assert!(shimmer_highlights(text, 1., base, bright).is_empty());

        let highlights = shimmer_highlights(text, band_on(2.), base, bright);
        let lightness = |byte: usize| {
            highlights
                .iter()
                .find(|(range, _)| range.start == byte)
                .and_then(|(_, style)| style.color)
                .map(|color| color.l)
        };
        let centre = lightness(2).expect("the band's centre is highlighted");
        assert!((centre - bright.l).abs() < 0.01);
        assert!(lightness(4).expect("inside the band") < centre);
        assert!(lightness(0).is_some());
        // "·" is two bytes; ranges follow characters, not bytes.
        assert!(highlights.iter().all(
            |(range, _)| text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
        ));
        assert!(highlights.iter().all(|(range, _)| range.start < 8));
    }

    #[test]
    fn block_markdown_gets_a_fixed_width_bubble() {
        assert!(!needs_fixed_bubble_width("couple of issues"));
        assert!(!needs_fixed_bubble_width("fix 2.5 things\nand -dash words"));
        assert!(needs_fixed_bubble_width(
            "couple of issues\n1. the side bar is buggy\n2. archived error"
        ));
        assert!(needs_fixed_bubble_width("notes:\n  - nested bullet"));
        assert!(needs_fixed_bubble_width("see\n* star bullet"));
        assert!(needs_fixed_bubble_width("run\n```\ncargo test\n```"));
        assert!(needs_fixed_bubble_width("| a | b |\n|---|---|"));
        assert!(needs_fixed_bubble_width("3) paren list"));
    }
}
