use gpui::{Action as _, Context, IntoElement, Window};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use paseo_client::{TimelineEntry, TimelinePayload};
use std::collections::BTreeSet;
use ui::{Button, Label, prelude::*};

use crate::{OpenWorkspace, PaseoView};

fn message_text(entry: &TimelineEntry) -> Option<&str> {
    match &entry.payload {
        TimelinePayload::Message(value) => value.get("text").and_then(|text| text.as_str()),
        _ => None,
    }
}

pub(super) fn render(
    view: &mut PaseoView,
    window: &mut Window,
    cx: &mut Context<PaseoView>,
) -> impl IntoElement {
    let (agents, selected_agent, providers, entries, permissions, error, has_older) = {
        let store = view.store.read(cx);
        let selected_agent = store.state.selected_agent.clone();
        let entries = store
            .state
            .timeline
            .values()
            .filter(|entry| selected_agent.as_deref() == Some(entry.agent_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let permissions = store
            .state
            .permissions
            .values()
            .filter(|request| selected_agent.as_deref() == Some(request.agent_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        (
            store.state.agents.clone(),
            selected_agent,
            store.providers.clone(),
            entries,
            permissions,
            store.state.error.clone(),
            store.has_older,
        )
    };

    let visible_markdown = entries
        .iter()
        .filter(|entry| message_text(entry).is_some())
        .map(|entry| (entry.agent_id.clone(), entry.epoch.clone(), entry.sequence))
        .collect::<BTreeSet<_>>();
    view.markdown
        .retain(|key, _| visible_markdown.contains(key));

    let mut provider_list = h_flex().gap_1();
    for provider in providers {
        let id = provider.id.clone();
        let label = provider.label.clone().unwrap_or_else(|| id.clone());
        let chosen = view.selected_provider.as_deref() == Some(id.as_str());
        provider_list = provider_list.child(
            Button::new(
                format!("paseo-provider-{id}"),
                if chosen {
                    format!("✓ {label}")
                } else {
                    label
                },
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.selected_provider = Some(id.clone());
                view.selected_model = None;
                cx.notify();
            })),
        );
    }

    let models = view
        .selected_provider
        .as_ref()
        .and_then(|provider_id| {
            view.store
                .read(cx)
                .providers
                .iter()
                .find(|provider| &provider.id == provider_id)
                .and_then(|provider| provider.extra.get("models"))
                .and_then(|models| models.as_array())
                .cloned()
        })
        .unwrap_or_default();
    let mut model_list = h_flex().gap_1();
    for model in models {
        if model.get("isSelectable").and_then(|value| value.as_bool()) == Some(false) {
            continue;
        }
        let Some(id) = model
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let label = model
            .get("label")
            .and_then(|label| label.as_str())
            .unwrap_or(&id)
            .to_owned();
        let chosen = view.selected_model.as_deref() == Some(id.as_str());
        model_list = model_list.child(
            Button::new(
                format!("paseo-model-{id}"),
                if chosen {
                    format!("✓ {label}")
                } else {
                    label
                },
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.selected_model = Some(id.clone());
                cx.notify();
            })),
        );
    }

    let mut agent_list = v_flex().gap_1();
    for agent in agents {
        let id = agent.id.clone();
        let label = agent.title.unwrap_or_else(|| id.clone());
        let selected = selected_agent.as_deref() == Some(id.as_str());
        agent_list = agent_list.child(
            Button::new(
                format!("paseo-agent-{id}"),
                format!(
                    "{}{} · {}",
                    if selected { "✓ " } else { "" },
                    label,
                    agent.status
                ),
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.store
                    .update(cx, |store, cx| store.select_agent(id.clone(), cx));
            })),
        );
    }

    let mut timeline = v_flex().gap_2();
    for entry in entries {
        let key = (entry.agent_id.clone(), entry.epoch.clone(), entry.sequence);
        let kind = match &entry.payload {
            TimelinePayload::Message(value) => value
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("Message"),
            TimelinePayload::Tool(_) => "Tool",
            TimelinePayload::Lifecycle(_) => "Event",
            TimelinePayload::Other(_) => "Activity",
        };
        if let Some(text) = message_text(&entry) {
            let markdown = if let Some((cached_text, markdown)) = view.markdown.get(&key)
                && cached_text == text
            {
                markdown.clone()
            } else {
                let markdown = cx.new(|cx| Markdown::new(text.to_owned().into(), None, None, cx));
                view.markdown
                    .insert(key, (text.to_owned(), markdown.clone()));
                markdown
            };
            timeline = timeline.child(v_flex().gap_1().child(Label::new(kind.to_owned())).child(
                MarkdownElement::new(
                    markdown,
                    MarkdownStyle::themed(MarkdownFont::Agent, window, cx),
                ),
            ));
        } else {
            let detail = match &entry.payload {
                TimelinePayload::Tool(value) => format!(
                    "{} · {}",
                    value
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("Tool"),
                    value
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                ),
                TimelinePayload::Lifecycle(value) | TimelinePayload::Other(value) => value
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Activity")
                    .to_owned(),
                TimelinePayload::Message(_) => kind.to_owned(),
            };
            timeline = timeline.child(
                v_flex()
                    .gap_1()
                    .child(Label::new(kind.to_owned()))
                    .child(Label::new(detail)),
            );
        }
    }

    let mut permission_list = v_flex().gap_1();
    for request in permissions {
        let allow_id = request.request_id.clone();
        let deny_id = request.request_id.clone();
        permission_list = permission_list.child(
            v_flex()
                .gap_1()
                .child(Label::new(request.title))
                .when_some(request.description, |this, description| {
                    this.child(Label::new(description))
                })
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(format!("paseo-allow-{allow_id}"), "Allow").on_click(
                                cx.listener(move |view, _, _, cx| {
                                    view.store.update(cx, |store, cx| {
                                        store.answer_permission(allow_id.clone(), true, cx)
                                    })
                                }),
                            ),
                        )
                        .child(
                            Button::new(format!("paseo-deny-{deny_id}"), "Deny").on_click(
                                cx.listener(move |view, _, _, cx| {
                                    view.store.update(cx, |store, cx| {
                                        store.answer_permission(deny_id.clone(), false, cx)
                                    })
                                }),
                            ),
                        ),
                ),
        );
    }

    v_flex()
        .flex_1()
        .min_h_0()
        .gap_2()
        .p_2()
        .when_some(error, |this, error| {
            this.child(Label::new(format!("Paseo error: {error}")))
        })
        .child(Label::new("New agent"))
        .child(provider_list)
        .child(model_list)
        .child(view.directory_input.clone())
        .child(
            Button::new("paseo-create-agent", "Create Agent")
                .on_click(cx.listener(|view, _, _, cx| view.create_agent(cx))),
        )
        .child(Label::new("Agents"))
        .child(agent_list)
        .when(selected_agent.is_some(), |this| {
            this.child(
                Button::new("paseo-open-workspace", "Open Workspace").on_click(cx.listener(
                    |_, _, window, cx| window.dispatch_action(OpenWorkspace.boxed_clone(), cx),
                )),
            )
        })
        .when(has_older, |this| {
            this.child(
                Button::new("paseo-load-older", "Load older messages").on_click(cx.listener(
                    |view, _, _, cx| view.store.update(cx, |store, cx| store.load_older(cx)),
                )),
            )
        })
        .child(
            v_flex()
                .id("paseo-timeline")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&view.timeline_scroll)
                .child(timeline),
        )
        .child(permission_list)
        .child(view.prompt_input.clone())
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new("paseo-send", "Send")
                        .on_click(cx.listener(|view, _, window, cx| view.send(window, cx))),
                )
                .child(
                    Button::new("paseo-cancel", "Cancel Run").on_click(cx.listener(
                        |view, _, _, cx| view.store.update(cx, |store, cx| store.cancel(cx)),
                    )),
                ),
        )
}
