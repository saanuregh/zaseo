use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, SharedString, TaskExt, Window, prelude::*,
    relative,
};
use markdown::{MarkdownFont, MarkdownStyle};
use paseo_client::{PermissionRequest, RewindMode};
use serde_json::Value;
use std::path::PathBuf;
use ui::{
    CommonAnimationExt, ContextMenu, ContextMenuEntry, CopyButton, IconButton, PopoverMenu,
    Tooltip, prelude::*,
};

use settings::Settings as _;
use theme_settings::ThemeSettings;

use crate::agent_view::{AgentView, CONTENT_MAX_WIDTH, Row};
use crate::timeline::{
    DiffLineKind, FileChange, NoticeLevel, StreamContent, StreamItem, ToolCall, ToolKind,
    ToolStatus, diff_stat, edit_diff_lines, format_duration, format_message_time,
    parse_subagent_log, tool_display, turn_text,
};

const MARKDOWN_BODY: u8 = 0;
const MARKDOWN_DETAIL: u8 = 1;
const DETAIL_MAX_HEIGHT: f32 = 400.;

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
        ToolKind::SubAgent => IconName::ZedAgent,
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
            crate::fork_agent(workspace, &agent_id, window, cx)
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

    fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
        let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        style.base_text_style.font_size = crate::chat_font_size(cx).into();
        style
    }

    pub(crate) fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let content = match row {
            Row::LoadOlder { loading } => h_flex()
                .w_full()
                .justify_center()
                .py_2()
                .child(
                    ui::Button::new(
                        "paseo-load-older",
                        if loading {
                            "Loading earlier messages…"
                        } else {
                            "Load earlier messages"
                        },
                    )
                    .label_size(LabelSize::Default)
                    .color(Color::Muted)
                    .disabled(loading)
                    .on_click(cx.listener(|view, _, _, cx| {
                        if let Some(agent_id) = view.agent_id.clone() {
                            view.store
                                .update(cx, |store, cx| store.load_older(&agent_id, cx));
                        }
                    })),
                )
                .into_any_element(),
            Row::Item {
                item,
                expanded,
                streaming,
                ..
            } => self.render_item(&item, expanded, streaming, window, cx),
            Row::TurnFooter {
                turn,
                duration_seconds,
                finished_at,
            } => self.render_turn_footer(turn, duration_seconds, finished_at, cx),
            Row::Working { since } => {
                let elapsed = since
                    .map(|since| (chrono::Utc::now() - since).num_seconds())
                    .filter(|seconds| *seconds >= 0)
                    .map(format_duration);
                h_flex()
                    .py_2()
                    .gap_2()
                    .child(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .with_rotate_animation(2),
                    )
                    .child(
                        Label::new(match elapsed {
                            Some(elapsed) => format!("Working · {elapsed}"),
                            None => "Working".into(),
                        })
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                    )
                    .when(!self.is_subagent(), |this| {
                        this.child(
                            Label::new("Esc to interrupt")
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        )
                    })
                    .into_any_element()
            }
            Row::Changes {
                files,
                expanded,
                show_all,
            } => self.render_changes(&files, &expanded, show_all, cx),
            Row::Spacer => div().h(rems_from_px(16_f32)).into_any_element(),
        };
        h_flex()
            .w_full()
            .justify_center()
            .px_4()
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems_from_px(CONTENT_MAX_WIDTH))
                    .min_w_0()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_item(
        &mut self,
        item: &StreamItem,
        expanded: bool,
        streaming: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        match &item.content {
            StreamContent::User { text, message_id } => {
                let rewind = message_id
                    .clone()
                    .filter(|_| !self.rewind_modes(cx).is_empty())
                    .map(|message_id| {
                        self.render_rewind_menu(item.key, message_id, text.clone(), cx)
                    });
                let markdown = self.markdown_for(item.key, MARKDOWN_BODY, text, cx);
                let group = SharedString::from(format!("paseo-user-{}", item.key));
                let copy_text = text.clone();
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
                    .as_ref()
                    .and_then(|message_id| self.store.read(cx).sent_images.get(message_id))
                    .cloned()
                    .unwrap_or_default();
                let show_bubble = !text.trim().is_empty() || sent_images.is_empty();
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
                                        .rounded_lg()
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
                                .px_4()
                                .py_2p5()
                                .rounded(rems_from_px(16_f32))
                                .rounded_tr(rems_from_px(4_f32))
                                .bg(colors.element_active)
                                .child(
                                    self.markdown_element(
                                        markdown,
                                        Self::markdown_style(window, cx),
                                    ),
                                ),
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
            StreamContent::Assistant { text } => {
                let markdown = self.markdown_for(item.key, MARKDOWN_BODY, text, cx);
                div()
                    .w_full()
                    .py_1p5()
                    .child(self.markdown_element(markdown, Self::markdown_style(window, cx)))
                    .into_any_element()
            }
            StreamContent::Reasoning { text } => {
                let summary = text
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| line.trim_matches('*').to_owned());
                let body = expanded.then(|| {
                    let markdown = self.markdown_for(item.key, MARKDOWN_DETAIL, text, cx);
                    let mut style = Self::markdown_style(window, cx);
                    style.base_text_style.color = colors.text_muted;
                    div()
                        .child(self.markdown_element(markdown, style))
                        .into_any_element()
                });
                self.render_expandable(
                    item.key,
                    IconName::ToolThink,
                    "Thinking".into(),
                    summary,
                    None,
                    streaming,
                    false,
                    expanded,
                    body,
                    cx,
                )
            }
            StreamContent::Tool(call) => self.render_tool(item.key, call, expanded, window, cx),
            StreamContent::Todo { items } => {
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
                                    .color(
                                        if entry.completed {
                                            Color::Success
                                        } else if entry.in_progress {
                                            Color::Accent
                                        } else {
                                            Color::Muted
                                        },
                                    ),
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
                    item.key,
                    IconName::ListTodo,
                    format!("Tasks {completed}/{}", items.len()).into(),
                    current,
                    None,
                    false,
                    false,
                    expanded,
                    body,
                    cx,
                )
            }
            StreamContent::Notice { level, message } => {
                let status = cx.theme().status();
                let (icon, color, background) = match level {
                    NoticeLevel::Info => (IconName::Info, Color::Info, status.info_background),
                    NoticeLevel::Warning => {
                        (IconName::Warning, Color::Warning, status.warning_background)
                    }
                    NoticeLevel::Error => {
                        (IconName::XCircle, Color::Error, status.error_background)
                    }
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
                            .text_color(colors.text)
                            .child(message.clone()),
                    )
                    .into_any_element()
            }
            StreamContent::Compaction {
                loading,
                pre_tokens,
            } => {
                let label = if *loading {
                    "Compacting…".to_owned()
                } else {
                    match pre_tokens {
                        Some(tokens) => format!(
                            "Context compacted ({})",
                            crate::composer::format_tokens(*tokens)
                        ),
                        None => "Context compacted".to_owned(),
                    }
                };
                h_flex()
                    .py_3()
                    .gap_2()
                    .child(div().flex_1().h_px().bg(colors.border_variant))
                    .child(
                        Icon::new(if *loading {
                            IconName::LoadCircle
                        } else {
                            IconName::Scissors
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                    .child(div().flex_1().h_px().bg(colors.border_variant))
                    .into_any_element()
            }
        }
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let group = SharedString::from(format!("paseo-row-{key}"));
        let has_body = body.is_some() || !expanded;
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
                    .id(("paseo-row", key))
                    .group(group.clone())
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .ml(rems_from_px(-8_f32))
                    .mr(rems_from_px(-8_f32))
                    .rounded_md()
                    .when(expanded, |this| {
                        this.bg(colors.surface_background).rounded_b_none()
                    })
                    .when(has_body, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(colors.ghost_element_hover))
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.toggle_expanded(key, cx)),
                            )
                    })
                    .child(div().flex_none().child(icon_element))
                    .child(
                        Label::new(label)
                            .size(LabelSize::Default)
                            .weight(FontWeight::MEDIUM)
                            .color(if expanded || running {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
                    .when_some(summary, |this, summary| {
                        this.child(
                            div().min_w_0().flex_shrink_1().child(
                                Label::new(summary)
                                    .size(LabelSize::Default)
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
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .when_some(body, |this, body| {
                this.child(
                    div()
                        .id(("paseo-row-detail", key))
                        .ml(rems_from_px(-8_f32))
                        .mr(rems_from_px(-8_f32))
                        .max_h(rems_from_px(DETAIL_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_b_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(body),
                )
            })
            .into_any_element()
    }

    fn render_tool(
        &mut self,
        key: u64,
        call: &ToolCall,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cwd = self.directory(cx);
        let mut display = tool_display(call, cwd.as_deref());
        let detail = &call.detail;
        let subagent_actions = (display.kind == ToolKind::SubAgent)
            .then(|| parse_subagent_log(detail_str(detail, "log").unwrap_or_default()).0);
        if call.status == ToolStatus::Running
            && let Some(latest) = subagent_actions.as_ref().and_then(|actions| actions.last())
        {
            let latest = latest.describe();
            display.summary = Some(match display.summary.take() {
                Some(description) => format!("{description} · {latest}"),
                None => latest,
            });
        }
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
            ToolKind::SubAgent => Some(self.render_subagent_trailing(
                key,
                call,
                subagent_actions.as_ref().map_or(0, Vec::len),
                cx,
            )),
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
        let body = expanded.then(|| self.render_tool_detail(key, call, display.kind, window, cx));
        self.render_expandable(
            key,
            tool_icon(display.kind),
            display.label.into(),
            display.summary,
            trailing,
            call.status == ToolStatus::Running,
            call.status == ToolStatus::Failed,
            expanded,
            body,
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
        key: u64,
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
                    let markdown = self.markdown_for(key, MARKDOWN_DETAIL, result, cx);
                    body = body
                        .child(self.markdown_element(markdown, Self::markdown_style(window, cx)));
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
                    let markdown = self.markdown_for(key, MARKDOWN_DETAIL, text, cx);
                    body = body
                        .child(self.markdown_element(markdown, Self::markdown_style(window, cx)));
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
                                other => serde_json::to_string_pretty(other).unwrap_or_default(),
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
            .rounded(rems_from_px(8_f32))
            .border_1()
            .border_color(colors.border_variant)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .bg(colors.element_background)
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
                            .child(
                                Icon::new(if is_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            )
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
                                }
                                view.rebuild(cx);
                            })),
                    )
                    .when(is_expanded, |this| {
                        this.child(
                            div()
                                .id(("paseo-turn-change-diff", index))
                                .max_h(rems_from_px(DETAIL_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .py_1()
                                .bg(colors.editor_background)
                                .child(render_diff_lines(file.lines.clone(), cx)),
                        )
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
        turn: usize,
        duration_seconds: Option<i64>,
        finished_at: Option<chrono::DateTime<chrono::Utc>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(range) = self.turns.get(turn).map(|turn| turn.items.clone()) else {
            return div().into_any_element();
        };
        let text = self.items.get(range).map(turn_text).unwrap_or_default();
        let has_agent_output = !text.is_empty() || duration_seconds.is_some();
        if !has_agent_output {
            return div().h(rems_from_px(4_f32)).into_any_element();
        }
        h_flex()
            .pb_2()
            .gap_1()
            .when(!text.is_empty(), |this| {
                this.child(CopyButton::new(
                    SharedString::from(format!("paseo-copy-turn-{turn}")),
                    text,
                ))
            })
            .when(!self.is_subagent(), |this| {
                this.child(
                    IconButton::new(("paseo-fork-turn", turn), IconName::GitBranchPlus)
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
