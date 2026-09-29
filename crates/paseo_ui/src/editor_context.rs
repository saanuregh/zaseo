use std::{
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::Result;
use editor::{CodeActionProvider, Editor};
use gpui::{App, Context, Entity, Task, Window};
use language::{Buffer, BufferSnapshot, Point, ToOffset as _};
use lsp::DiagnosticSeverity;
use multi_buffer::MultiBufferRow;
use project::{CodeAction, LspAction, ProjectPath, ProjectTransaction};
use util::markdown::MarkdownCodeBlock;
use workspace::{MultiWorkspace, Workspace};
use zed_actions::paseo::{AddPathsToAgent, AddSelectionToAgent};

use crate::{AgentTab, open_agent_here, open_draft};

const FIX_ACTION_TITLE: &str = "Ask Agent to Fix";

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, action: &AddPathsToAgent, window, cx| {
            let paths = action.paths.clone();
            send_to_agent(
                workspace,
                move |directory| {
                    paths
                        .iter()
                        .map(|path| format!("@{}", mention_path(path, directory)))
                        .collect::<Vec<_>>()
                        .join(" ")
                        + " "
                },
                window,
                cx,
            );
        });
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, window, cx| {
        if let Some(window) = window
            && editor.mode().is_full()
            && editor.project().is_some()
        {
            editor.add_code_action_provider(Rc::new(DiagnosticFixProvider), window, cx);
            let editor_handle = cx.entity().downgrade();
            editor
                .register_action(move |_: &AddSelectionToAgent, window, cx| {
                    if let Some(editor) = editor_handle.upgrade() {
                        add_selection_to_agent(&editor, window, cx);
                    }
                })
                .detach();
        }
    })
    .detach();
}

/// Code to hand to an agent: where it lives and its text.
struct CodeSnippet {
    path: Option<PathBuf>,
    /// One-based, inclusive.
    rows: RangeInclusive<u32>,
    fence_tag: String,
    text: String,
}

impl CodeSnippet {
    fn new(buffer: &Entity<Buffer>, range: Range<Point>, workspace: &Workspace, cx: &App) -> Self {
        let snapshot = buffer.read(cx).snapshot();
        let fence_tag = snapshot
            .language_at(range.start)
            .map(|language| language.code_fence_block_name().to_string())
            .unwrap_or_default();
        let text = snapshot.text_for_range(range.clone()).collect::<String>();
        Self {
            path: buffer_path(buffer, workspace, cx),
            rows: snippet_rows(&range),
            fence_tag,
            text,
        }
    }

    fn reference(&self, directory: Option<&Path>) -> Option<String> {
        let path = mention_path(self.path.as_deref()?, directory);
        Some(if self.rows.start() == self.rows.end() {
            format!("@{path}:{}", self.rows.start())
        } else {
            format!("@{path}:{}-{}", self.rows.start(), self.rows.end())
        })
    }

    fn code_block(&self) -> String {
        MarkdownCodeBlock {
            tag: &self.fence_tag,
            text: self.text.trim_end_matches('\n'),
        }
        .to_string()
    }
}

/// One-based rows a range covers, leaving out a final line the range only reaches the start of.
fn snippet_rows(range: &Range<Point>) -> RangeInclusive<u32> {
    let end_row = if range.end.column == 0 && range.end.row > range.start.row {
        range.end.row - 1
    } else {
        range.end.row
    };
    range.start.row + 1..=end_row + 1
}

fn buffer_path(buffer: &Entity<Buffer>, workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let file = buffer.read(cx).file()?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    workspace
        .project()
        .read(cx)
        .absolute_path(&project_path, cx)
}

/// How an agent should read a path: relative to its directory when inside it, else absolute.
fn mention_path(path: &Path, agent_directory: Option<&Path>) -> String {
    match agent_directory.and_then(|directory| path.strip_prefix(directory).ok()) {
        Some(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Some(relative) => relative.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

fn selection_context(snippets: &[CodeSnippet], directory: Option<&Path>) -> String {
    let mut context = String::new();
    for snippet in snippets {
        if let Some(reference) = snippet.reference(directory) {
            context.push_str(&reference);
            context.push('\n');
        }
        context.push_str(&snippet.code_block());
    }
    context
}

struct Problem {
    severity: DiagnosticSeverity,
    /// One-based.
    row: u32,
    message: String,
    source: Option<String>,
}

fn problem_line(problem: &Problem) -> String {
    let severity = match problem.severity {
        DiagnosticSeverity::ERROR => "error",
        DiagnosticSeverity::WARNING => "warning",
        DiagnosticSeverity::INFORMATION => "info",
        _ => "hint",
    };
    let message = problem.message.lines().collect::<Vec<_>>().join(" ");
    match &problem.source {
        Some(source) => format!("- {severity} on line {}: {message} ({source})", problem.row),
        None => format!("- {severity} on line {}: {message}", problem.row),
    }
}

fn diagnostic_context(
    problems: &[Problem],
    snippet: &CodeSnippet,
    directory: Option<&Path>,
) -> String {
    let subject = if problems.len() == 1 {
        "this problem"
    } else {
        "these problems"
    };
    let location = snippet
        .reference(directory)
        .map(|reference| format!(" in {reference}"))
        .unwrap_or_default();
    let mut context = format!("Fix {subject}{location}:\n");
    for problem in problems {
        context.push_str(&problem_line(problem));
        context.push('\n');
    }
    context.push_str(&snippet.code_block());
    context
}

fn add_selection_to_agent(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let ranges = selected_code_ranges(editor, cx);
    workspace.update(cx, |workspace, cx| {
        let snippets = ranges
            .into_iter()
            .map(|(buffer, range)| CodeSnippet::new(&buffer, range, workspace, cx))
            .collect::<Vec<_>>();
        if snippets.is_empty() {
            return;
        }
        send_to_agent(
            workspace,
            move |directory| selection_context(&snippets, directory),
            window,
            cx,
        );
    });
}

/// Each selection, or the cursor's line when nothing is selected, as ranges in the buffers
/// it shows. A selection across several excerpts gives one range per excerpt, so lines hidden
/// between excerpts are never sent.
fn selected_code_ranges(
    editor: &Entity<Editor>,
    cx: &mut App,
) -> Vec<(Entity<Buffer>, Range<Point>)> {
    editor.update(cx, |editor, cx| {
        let selections = editor.selections.all_adjusted(&editor.display_snapshot(cx));
        let multi_buffer = editor.buffer().read(cx);
        let snapshot = multi_buffer.snapshot(cx);
        let mut ranges: Vec<(Entity<Buffer>, Range<Point>)> = Vec::new();
        for selection in selections {
            let range = if selection.is_empty() {
                let row = selection.head().row;
                Point::new(row, 0)..Point::new(row, snapshot.line_len(MultiBufferRow(row)))
            } else {
                selection.start..selection.end
            };
            for (buffer_snapshot, buffer_range, _) in snapshot.range_to_buffer_ranges(range) {
                let Some(buffer) = multi_buffer.buffer(buffer_snapshot.remote_id()) else {
                    continue;
                };
                let start = buffer_snapshot.offset_to_point(buffer_range.start.0);
                let end = buffer_snapshot.offset_to_point(buffer_range.end.0);
                let already_sent = ranges
                    .iter()
                    .any(|(sent, sent_range)| sent == &buffer && *sent_range == (start..end));
                if start != end && !already_sent {
                    ranges.push((buffer, start..end));
                }
            }
        }
        ranges
    })
}

/// Puts context into the composer of the agent used last, or a new draft's when there is none,
/// without switching projects: the user stays next to the code they sent.
fn send_to_agent(
    workspace: &mut Workspace,
    build_context: impl FnOnce(Option<&Path>) -> String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tab = match target(workspace, cx) {
        Target::Tab(tab) => {
            workspace.activate_item(&tab, true, true, window, cx);
            tab
        }
        Target::Agent(agent_id) => open_agent_here(workspace, &agent_id, true, window, cx),
        Target::Draft => open_draft(workspace, None, window, cx),
    };
    let view = tab.read(cx).view().clone();
    let (directory, composer) = {
        let view = view.read(cx);
        let directory = view
            .directory(cx)
            .or_else(|| view.composer.read(cx).draft_directory.clone());
        (directory, view.composer.clone())
    };
    let context = build_context(directory.as_deref());
    composer.update(cx, |composer, cx| {
        composer.insert_context(&context, window, cx)
    });
}

enum Target {
    Tab(Entity<AgentTab>),
    Agent(String),
    Draft,
}

/// The agent tab activated most recently in this workspace, else the agent last focused in any
/// Paseo view. Focus alone misses tabs opened from the sidebar that were never clicked into.
/// A subagent tab is read-only, so its parent agent takes the text.
fn target(workspace: &Workspace, cx: &App) -> Target {
    let agent_id = match recent_agent_tab(workspace, cx) {
        Some(tab) => match tab.read(cx).agent_id(cx) {
            Some(agent_id) if paseo_client::parse_subagent_timeline_id(&agent_id).is_some() => {
                agent_id
            }
            _ => return Target::Tab(tab),
        },
        None => match crate::store(cx).read(cx).focused_agent.clone() {
            Some(agent_id) => agent_id,
            None => return Target::Draft,
        },
    };
    let agent_id = match paseo_client::parse_subagent_timeline_id(&agent_id) {
        Some((parent_agent_id, _)) => parent_agent_id.to_owned(),
        None => agent_id,
    };
    if crate::store(cx).read(cx).agent(&agent_id).is_some() {
        Target::Agent(agent_id)
    } else {
        Target::Draft
    }
}

fn recent_agent_tab(workspace: &Workspace, cx: &App) -> Option<Entity<AgentTab>> {
    workspace
        .panes()
        .iter()
        .flat_map(|pane| {
            let pane = pane.read(cx);
            pane.activation_history()
                .iter()
                .filter_map(|entry| {
                    let tab = pane
                        .items()
                        .find(|item| item.item_id() == entry.entity_id)?
                        .downcast::<AgentTab>()?;
                    Some((entry.timestamp, tab))
                })
                .collect::<Vec<_>>()
        })
        .max_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, tab)| tab)
}

fn fixable_problems(
    snapshot: &BufferSnapshot,
    range: Range<usize>,
) -> Vec<(Range<Point>, Problem)> {
    snapshot
        .diagnostics_in_range::<_, Point>(range, false)
        .filter(|entry| is_fixable(entry.diagnostic))
        .map(|entry| {
            let problem = Problem {
                severity: entry.diagnostic.severity,
                row: entry.range.start.row + 1,
                message: entry.diagnostic.message.to_string(),
                source: diagnostic_source(entry.diagnostic),
            };
            (entry.range, problem)
        })
        .collect()
}

/// Errors and warnings only: hints and notes are often hidden by the editor, and aren't
/// problems to fix.
fn is_fixable(diagnostic: &language::Diagnostic) -> bool {
    diagnostic.is_primary
        && (diagnostic.severity == DiagnosticSeverity::ERROR
            || diagnostic.severity == DiagnosticSeverity::WARNING)
}

fn diagnostic_source(diagnostic: &language::Diagnostic) -> Option<String> {
    let code = diagnostic.code.as_ref().map(|code| match code {
        lsp::NumberOrString::Number(number) => number.to_string(),
        lsp::NumberOrString::String(text) => text.clone(),
    });
    match (diagnostic.source.clone(), code) {
        (Some(source), Some(code)) => Some(format!("{source} {code}")),
        (source, code) => source.or(code),
    }
}

/// Offers "Ask Agent to Fix" in the code actions menu when the cursor is on a diagnostic.
struct DiagnosticFixProvider;

impl CodeActionProvider for DiagnosticFixProvider {
    fn id(&self) -> Arc<str> {
        "paseo_diagnostic_fix".into()
    }

    fn code_actions(
        &self,
        buffer: &Entity<Buffer>,
        range: Range<text::Anchor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offsets = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
        let has_problem = snapshot
            .diagnostics_in_range::<_, usize>(offsets, false)
            .any(|entry| is_fixable(entry.diagnostic));
        if !has_problem {
            return Task::ready(Ok(Vec::new()));
        }
        Task::ready(Ok(vec![CodeAction {
            server_id: lsp::LanguageServerId(usize::MAX),
            range,
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title: FIX_ACTION_TITLE.into(),
                ..Default::default()
            })),
            resolved: true,
        }]))
    }

    fn apply_code_action(
        &self,
        buffer: Entity<Buffer>,
        action: CodeAction,
        _push_to_history: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<ProjectTransaction>> {
        // Deferred because the editor applying the action, and possibly the window's root, are
        // mid-update, and opening the agent tab reads both.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = window
                .root::<MultiWorkspace>()
                .flatten()
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
            else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                ask_agent_to_fix(workspace, &buffer, action.range, window, cx)
            });
        });
        Task::ready(Ok(ProjectTransaction::default()))
    }
}

fn ask_agent_to_fix(
    workspace: &mut Workspace,
    buffer: &Entity<Buffer>,
    range: Range<text::Anchor>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let snapshot = buffer.read(cx).snapshot();
    let offsets = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
    let problems_with_ranges = fixable_problems(&snapshot, offsets);
    let (Some(first_row), Some(last_row)) = (
        problems_with_ranges
            .iter()
            .map(|(range, _)| range.start.row)
            .min(),
        problems_with_ranges
            .iter()
            .map(|(range, _)| *snippet_rows(range).end() - 1)
            .max(),
    ) else {
        return;
    };
    let lines = Point::new(first_row, 0)..Point::new(last_row, snapshot.line_len(last_row));
    let snippet = CodeSnippet::new(buffer, lines, workspace, cx);
    let problems = problems_with_ranges
        .into_iter()
        .map(|(_, problem)| problem)
        .collect::<Vec<_>>();
    send_to_agent(
        workspace,
        move |directory| diagnostic_context(&problems, &snippet, directory),
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(rows: RangeInclusive<u32>, text: &str) -> CodeSnippet {
        CodeSnippet {
            path: Some(PathBuf::from("/work/app/src/main.rs")),
            rows,
            fence_tag: "rust".to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn mention_path_is_relative_inside_the_agent_directory() {
        let path = Path::new("/work/app/src/main.rs");
        assert_eq!(
            mention_path(path, Some(Path::new("/work/app"))),
            "src/main.rs"
        );
        assert_eq!(
            mention_path(path, Some(Path::new("/other"))),
            "/work/app/src/main.rs"
        );
        assert_eq!(mention_path(path, None), "/work/app/src/main.rs");
        assert_eq!(
            mention_path(Path::new("/work/app"), Some(Path::new("/work/app"))),
            "."
        );
    }

    #[test]
    fn snippet_rows_leave_out_a_line_the_selection_only_touches() {
        assert_eq!(snippet_rows(&(Point::new(3, 2)..Point::new(3, 8))), 4..=4);
        assert_eq!(snippet_rows(&(Point::new(3, 0)..Point::new(6, 0))), 4..=6);
        assert_eq!(snippet_rows(&(Point::new(3, 0)..Point::new(6, 1))), 4..=7);
    }

    #[test]
    fn selection_context_references_the_lines_and_quotes_the_code() {
        let context = selection_context(
            &[snippet(12..=13, "fn main() {\n    run();\n")],
            Some(Path::new("/work/app")),
        );
        assert_eq!(
            context,
            "@src/main.rs:12-13\n```rust\nfn main() {\n    run();\n```\n"
        );
    }

    #[test]
    fn selection_context_lengthens_the_fence_around_backticks() {
        let context = selection_context(&[snippet(4..=4, "let fence = \"```\";")], None);
        assert_eq!(
            context,
            "@/work/app/src/main.rs:4\n````rust\nlet fence = \"```\";\n````\n"
        );
    }

    #[test]
    fn diagnostic_context_lists_each_problem_above_the_code() {
        let problems = [
            Problem {
                severity: DiagnosticSeverity::ERROR,
                row: 12,
                message: "mismatched types\nexpected `u32`".to_owned(),
                source: Some("rustc E0308".to_owned()),
            },
            Problem {
                severity: DiagnosticSeverity::WARNING,
                row: 13,
                message: "unused variable".to_owned(),
                source: None,
            },
        ];
        let context = diagnostic_context(
            &problems,
            &snippet(12..=13, "let a: u32 = \"\";\nlet b = 1;"),
            Some(Path::new("/work/app")),
        );
        assert_eq!(
            context,
            "Fix these problems in @src/main.rs:12-13:\n\
             - error on line 12: mismatched types expected `u32` (rustc E0308)\n\
             - warning on line 13: unused variable\n\
             ```rust\nlet a: u32 = \"\";\nlet b = 1;\n```\n"
        );
    }
}
