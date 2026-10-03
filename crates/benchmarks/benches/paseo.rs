use std::hint::black_box;

use gpui::{
    AppContext as _, BenchAppContext, Context, Entity, IntoElement, ParentElement as _, Pixels,
    Render, Styled as _, Window, div, px,
};
use paseo_client::{AgentSummary, TimelineEntry, TimelinePayload};
use paseo_ui::{AgentView, FileEdit, PaseoPanel};
use serde_json::json;

/// Sidebar frames while agents work: rows re-render every frame a status animates.
#[gpui::bench(
    inputs = agent_counts(),
    group = "Paseo sidebar frame",
    input_name = "agents",
    sample_size = 20
)]
fn paseo_sidebar_frame(agent_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let agent_count = *agent_count;
    cx.update(|cx| paseo_ui::test_set_agents(agents(agent_count), cx));
    let mut window = cx.add_empty_window();
    let panel = window.update(|window, cx| window.replace_root(cx, PaseoPanel::test_new));
    let listed = cx.update(|cx| PaseoPanel::test_agent_order(&panel, cx).len());
    assert_eq!(listed, agent_count, "the sidebar lists every agent");
    cx.bench_renderer(panel, |_, _, cx| cx.notify());
}

/// One agent reporting an update, which moves it to the top, then the frame that shows it.
#[gpui::bench(
    inputs = agent_counts(),
    group = "Paseo sidebar agent update",
    input_name = "agents",
    sample_size = 20
)]
fn paseo_sidebar_agent_update(agent_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let agent_count = *agent_count;
    cx.update(|cx| paseo_ui::test_set_agents(agents(agent_count), cx));
    let mut window = cx.add_empty_window();
    let panel = window.update(|window, cx| window.replace_root(cx, PaseoPanel::test_new));
    let listed = cx.update(|cx| PaseoPanel::test_agent_order(&panel, cx).len());
    assert_eq!(listed, agent_count, "the sidebar lists every agent");
    let mut update = 0;
    cx.bench_renderer(panel, move |_, _, cx| {
        update += 1;
        let mut agent = agent(update % agent_count);
        agent.status = if update.is_multiple_of(2) {
            "running"
        } else {
            "idle"
        }
        .into();
        agent.extra = json!({"updatedAt": timestamp(agent_count + update)});
        paseo_ui::test_upsert_agent(agent, cx);
    });
}

/// Dragging the sidebar's edge: a new width measures every row again, rows out of view too.
#[gpui::bench(
    inputs = agent_counts(),
    group = "Paseo sidebar resize",
    input_name = "agents",
    sample_size = 20
)]
fn paseo_sidebar_resize(agent_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let agent_count = *agent_count;
    cx.update(|cx| paseo_ui::test_set_agents(agents(agent_count), cx));
    let mut window = cx.add_empty_window();
    let dock = window.update(|window, cx| {
        window.replace_root(cx, |window, cx| SidebarDock {
            panel: cx.new(|cx| PaseoPanel::test_new(window, cx)),
            width: px(300.),
        })
    });
    let panel = cx.read(|cx| dock.read(cx).panel.clone());
    let listed = cx.update(|cx| PaseoPanel::test_agent_order(&panel, cx).len());
    assert_eq!(listed, agent_count, "the sidebar lists every agent");
    cx.bench_renderer(dock, |dock, _, cx| {
        dock.width = if dock.width == px(300.) {
            px(340.)
        } else {
            px(300.)
        };
        cx.notify();
    });
}

/// The sidebar at a set width, as the dock holds it. A test window's own resize doesn't lay the
/// window out again, so the width changes here.
struct SidebarDock {
    panel: Entity<PaseoPanel>,
    width: Pixels,
}

impl Render for SidebarDock {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().h_full().w(self.width).child(self.panel.clone())
    }
}

/// Chat frames with a long conversation on screen.
#[gpui::bench(
    inputs = timeline_lengths(),
    group = "Paseo chat frame",
    input_name = "messages",
    sample_size = 20
)]
fn paseo_chat_frame(message_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let message_count = *message_count;
    cx.update(|cx| paseo_ui::test_stream_entries(conversation(message_count), cx));
    let mut window = cx.add_empty_window();
    let chat = window.update(|window, cx| {
        window.replace_root(cx, |window, cx| paseo_ui::test_chat(AGENT, window, cx))
    });
    let rows = cx.update(|cx| paseo_ui::test_chat_rows(&chat, cx));
    assert!(
        rows >= message_count,
        "the chat shows the conversation, got {rows} rows"
    );
    cx.bench_renderer(chat, |_, _, cx| cx.notify());
}

/// One streamed chunk of the agent's reply, then the frame that shows it.
#[gpui::bench(
    inputs = timeline_lengths(),
    group = "Paseo chat streamed chunk",
    input_name = "messages",
    sample_size = 20
)]
fn paseo_chat_streamed_chunk(message_count: &usize, cx: &mut BenchAppContext) {
    init_context(cx);
    let message_count = *message_count;
    cx.update(|cx| paseo_ui::test_stream_entries(conversation(message_count), cx));
    let mut window = cx.add_empty_window();
    let chat = window.update(|window, cx| {
        window.replace_root(cx, |window, cx| paseo_ui::test_chat(AGENT, window, cx))
    });
    let rows = cx.update(|cx| paseo_ui::test_chat_rows(&chat, cx));
    assert!(
        rows >= message_count,
        "the chat shows the conversation, got {rows} rows"
    );
    let mut sequence = message_count as u64;
    cx.bench_renderer(chat, move |_: &mut AgentView, _, cx| {
        sequence += 1;
        paseo_ui::test_stream_entries(
            vec![entry(
                sequence,
                json!({"type": "assistant_message", "text": "and the next few words "}),
            )],
            cx,
        );
    });
}

/// Finding where an agent's edits sit in a file, which the edits overlay redoes on every change.
#[gpui::bench(
    inputs = file_lengths(),
    group = "Paseo agent edits location",
    input_name = "lines",
    sample_size = 20
)]
fn paseo_locate_agent_edits(line_count: &usize, cx: &mut BenchAppContext) {
    let text = (0..*line_count)
        .map(|line| format!("    let value_{line} = compute({line});\n"))
        .collect::<String>();
    let edits = (0..20)
        .map(|index| {
            let line = index * line_count / 20;
            (
                index,
                FileEdit {
                    old_text: format!("    let value_{line} = 0;\n"),
                    new_text: format!("    let value_{line} = compute({line});\n"),
                    line_hint: Some(line),
                    whole_file: false,
                },
            )
        })
        .collect::<Vec<_>>();
    cx.bench_iter(|_| {
        black_box(paseo_ui::reverse_edits_tracking(
            black_box(&text),
            black_box(&edits),
        ));
    });
}

const AGENT: &str = "agent";

fn init_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        paseo_ui::test_init(cx);
    });
}

/// Agents across a few projects, every tenth one working so its row animates.
fn agents(count: usize) -> Vec<AgentSummary> {
    (0..count).map(agent).collect()
}

fn agent(index: usize) -> AgentSummary {
    let mut agent = paseo_ui::test_agent(
        &format!("agent-{index}"),
        &format!("Fix the login flow, part {index}"),
        if index.is_multiple_of(10) {
            "running"
        } else {
            "idle"
        },
    );
    agent.project = Some(json!({"projectName": format!("project-{}", index % 5)}));
    agent.extra = json!({"updatedAt": timestamp(index)});
    agent
}

/// Alternating user messages and replies of a few sentences.
fn conversation(message_count: usize) -> Vec<TimelineEntry> {
    (0..message_count)
        .map(|index| {
            let kind = if index.is_multiple_of(2) {
                "user_message"
            } else {
                "assistant_message"
            };
            entry(
                index as u64 + 1,
                json!({"type": kind, "text": format!(
                    "Message {index}: the change moves the login check before the redirect, \
                     so a stale session no longer loops. Tests cover both paths."
                )}),
            )
        })
        .collect()
}

fn entry(sequence: u64, payload: serde_json::Value) -> TimelineEntry {
    TimelineEntry {
        agent_id: AGENT.into(),
        epoch: "epoch".into(),
        sequence,
        timestamp: timestamp(sequence as usize),
        payload: TimelinePayload::Message(payload),
        extra: json!({}),
    }
}

/// A distinct time for each `seconds`, later for larger values.
fn timestamp(seconds: usize) -> String {
    format!(
        "2026-10-{:02}T{:02}:{:02}:{:02}Z",
        1 + seconds / 86_400 % 28,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn agent_counts() -> Vec<usize> {
    let mut counts = vec![10, 100, 500];
    if std::env::var("ZED_BENCH_HUGE").is_ok() {
        counts.push(2000);
    }
    counts
}

fn timeline_lengths() -> Vec<usize> {
    let mut lengths = vec![100, 1000];
    if std::env::var("ZED_BENCH_HUGE").is_ok() {
        lengths.push(10_000);
    }
    lengths
}

fn file_lengths() -> Vec<usize> {
    let mut lengths = vec![1000, 10_000];
    if std::env::var("ZED_BENCH_HUGE").is_ok() {
        lengths.push(100_000);
    }
    lengths
}

gpui::bench_group!(
    benches,
    paseo_sidebar_frame,
    paseo_sidebar_agent_update,
    paseo_sidebar_resize,
    paseo_chat_frame,
    paseo_chat_streamed_chunk,
    paseo_locate_agent_edits
);
gpui::bench_main!(benches);
