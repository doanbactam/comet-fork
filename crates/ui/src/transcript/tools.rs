//! Tool chip/group model and detail builders.

use std::sync::Arc;

use gpui::SharedString;
use zeron_doc::SubagentStatus;
use zeron_proto::ToolCall;

use crate::markdown::parser::{Block, BlockTree, InlineRun, InlineStyle};

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
    /// Expandable detail: a code-block of output lines, or a real diff
    /// section rendered by the changes pane's component (ACP harnesses).
    /// Precomputed here because rows are cached by fingerprint — diffing and
    /// tokenizing per paint would run on every scroll frame.
    pub detail: Option<Arc<ToolDetail>>,
    /// Expandable full-invocation block: the complete tool call (whole
    /// command / pattern / URL / input JSON) that the chip header collapses
    /// to one truncated line. Rendered above `detail` in the open card.
    /// Precomputed for the same reason as `detail`.
    pub invocation: Option<Arc<ToolDetail>>,
    /// Sidecar key of the full output (chat2-sync A3) — the doc carries only
    /// a one-line summary; expanding offers a lazy "Show full output" fetch.
    pub output_ref: Option<SharedString>,
    /// Full-output size, for the affordance label ("Show full output (12 KB)").
    pub output_bytes: Option<u64>,
    /// Sidecar key of the full diff (doc carries only per-file stats).
    pub diff_ref: Option<SharedString>,
    /// The spawned SUBAGENT's doc id — the chip IS the index (there is no
    /// listing endpoint); with it the chip offers "Open subagent".
    pub subagent_ref: Option<SharedString>,
    /// Subagent lifecycle, distinct from `resolved` (eager-done: the spawn
    /// tool's own result lands while the subagent still runs).
    pub subagent_status: Option<SubagentStatus>,
    /// One-line live tail — LEGACY docs only (new runs stopped folding it;
    /// per-delta header rewrites read as noise). Never rendered; still
    /// fingerprinted so an old doc's chips re-splice correctly.
    pub subagent_tail: Option<SharedString>,
    /// A REASONING part riding the tool group as a chip (user request: the
    /// thought process belongs inside the combined "Ran N commands"
    /// accordion, opening/closing with the same tween). Synthesized in
    /// [`rows_for_entry`] — never comes from a doc tool part. The thought
    /// text is the `detail`; `resolved == false` means it is still
    /// streaming (the chip then defaults open).
    pub is_thought: bool,
}

/// Subagent spawn chips — [`ToolCall::is_subagent_spawn`], the shared genus
/// every driver decodes its spawn tool into. These stay out of the
/// collapsible "Called N tools" wrap so a running subagent is visible
/// without opening the fold.
pub(super) fn is_agent_call(call: &ToolCall) -> bool {
    call.is_subagent_spawn()
}

/// The chip's GENUS is the call itself, never the ref: docs written before
/// the claude-driver fix carry stray `subagent_ref`s on ordinary Run chips
/// (a background shell's `task_notification` was mis-tagged as subagent
/// traffic), and honoring the ref alone turned those Runs into spawn chips
/// that opened empty, never-created subagent docs.
pub(super) fn is_agent_tool(item: &ToolItem) -> bool {
    is_agent_call(&item.call)
}

/// A chip renders as the spawn LINK (whole-card click → subagent tab) only
/// when an agent call has actually been bound to its doc.
pub(super) fn is_spawn_link(item: &ToolItem) -> bool {
    is_agent_call(&item.call) && item.subagent_ref.is_some()
}

/// Ordinary tool groups fold behind a summary header; agent/spawn chips
/// render as their own always-open row.
pub(super) fn tool_group_collapses(tools: &[ToolItem]) -> bool {
    tools.iter().any(|t| !is_agent_tool(t))
}

/// Column budget for soft-wrapping thought text into detail lines. The
/// detail body is preformatted (no element wrapping), so the wrap happens
/// here — conservative enough to fit the card at typical transcript widths.
pub(super) const THOUGHT_WRAP_COLS: usize = 96;

/// Flatten a thought's parsed markdown into wrapped, STYLED detail lines —
/// inline markers render as real styling (bold/italic/code/links) instead of
/// literal `**` glyphs; blocks flatten structurally (headings bold, list
/// bullets, quote bars, verbatim code lines). Every line is one fixed-height
/// row, so the detail height stays analytic (lines × [`OUTPUT_LINE_HEIGHT`])
/// and the group's fold tween keeps working without measurement.
pub(super) fn thought_lines(tree: &BlockTree) -> Vec<Vec<InlineRun>> {
    let mut out: Vec<Vec<InlineRun>> = Vec::new();
    for top in &tree.blocks {
        if !out.is_empty() {
            // One blank separator row between top-level blocks (the old
            // plain-text wrap kept paragraph gaps the same way).
            out.push(Vec::new());
        }
        thought_block_lines(&top.block, 0, &mut out);
    }
    while out
        .last()
        .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
    {
        out.pop();
    }
    out
}

/// The slot-0 indent run every emitted thought line opens with (possibly
/// empty). List/quote handlers rewrite it in place to plant markers/bars, so
/// it must exist even at zero indent.
pub(super) fn indent_run(indent: usize) -> Vec<InlineRun> {
    vec![InlineRun {
        text: " ".repeat(indent),
        style: InlineStyle::default(),
    }]
}

/// Append text to a line's run list, merging into the tail run when styles
/// match (keeps run counts small for the shaper).
pub(super) fn push_styled(line: &mut Vec<InlineRun>, text: &str, style: &InlineStyle) {
    if text.is_empty() {
        return;
    }
    match line.last_mut() {
        Some(last) if last.style == *style => last.text.push_str(text),
        _ => line.push(InlineRun {
            text: text.to_owned(),
            style: style.clone(),
        }),
    }
}

/// Close a wrapped line: the slot-0 indent run in front (see [`indent_run`]).
pub(super) fn finish_line(indent: usize, mut line: Vec<InlineRun>) -> Vec<InlineRun> {
    let mut full = indent_run(indent);
    full.append(&mut line);
    full
}

/// Word-wrap styled runs at the thought column budget. Char-counted like
/// every detail wrap — block heights must stay analytic — with words glued
/// across style boundaries (`**bold**tail` wraps as one unit), separator
/// spaces riding the preceding run, and pathological overlong tokens
/// hard-split at the budget. Hard breaks (`\n` runs) split into
/// separately-wrapped segments.
pub(super) fn wrap_styled_runs(runs: &[InlineRun], indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    let budget = THOUGHT_WRAP_COLS.saturating_sub(indent).max(16);
    let mut segments: Vec<Vec<InlineRun>> = vec![Vec::new()];
    for run in runs {
        for (ix, piece) in run.text.split('\n').enumerate() {
            if ix > 0 {
                segments.push(Vec::new());
            }
            if !piece.is_empty() {
                segments.last_mut().unwrap().push(InlineRun {
                    text: piece.to_owned(),
                    style: run.style.clone(),
                });
            }
        }
    }
    for segment in segments {
        // Tokens: maximal non-whitespace piece lists, glued across run
        // boundaries so a word split by styling never wraps mid-word.
        let mut tokens: Vec<Vec<InlineRun>> = Vec::new();
        let mut in_token = false;
        for run in &segment {
            let text = run.text.as_str();
            let mut pos = 0;
            while pos < text.len() {
                let rest = &text[pos..];
                let ws = rest.chars().next().is_some_and(char::is_whitespace);
                let end = rest
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace() != ws)
                    .map_or(text.len(), |(i, _)| pos + i);
                if ws {
                    in_token = false;
                } else {
                    if !in_token {
                        tokens.push(Vec::new());
                        in_token = true;
                    }
                    push_styled(tokens.last_mut().unwrap(), &text[pos..end], &run.style);
                }
                pos = end;
            }
        }
        let mut line: Vec<InlineRun> = Vec::new();
        let mut len = 0usize;
        for token in tokens {
            let tok_len: usize = token.iter().map(|r| r.text.chars().count()).sum();
            if tok_len > budget {
                // Hard-split a pathological token at the budget.
                if len > 0 {
                    out.push(finish_line(indent, std::mem::take(&mut line)));
                    len = 0;
                }
                for piece in token {
                    let mut chars = piece.text.chars();
                    loop {
                        let chunk: String = chars.by_ref().take(budget - len).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        len += chunk.chars().count();
                        push_styled(&mut line, &chunk, &piece.style);
                        if len == budget {
                            out.push(finish_line(indent, std::mem::take(&mut line)));
                            len = 0;
                        }
                    }
                }
                continue;
            }
            if len > 0 && len + 1 + tok_len > budget {
                out.push(finish_line(indent, std::mem::take(&mut line)));
                len = 0;
            }
            if len > 0 {
                if let Some(last) = line.last_mut() {
                    last.text.push(' ');
                }
                len += 1;
            }
            for piece in token {
                push_styled(&mut line, &piece.text, &piece.style);
            }
            len += tok_len;
        }
        if len > 0 {
            out.push(finish_line(indent, line));
        }
    }
}

/// One markdown block into thought detail lines, `indent` spaces deep.
pub(super) fn thought_block_lines(block: &Block, indent: usize, out: &mut Vec<Vec<InlineRun>>) {
    match block {
        Block::Paragraph { runs } => wrap_styled_runs(runs, indent, out),
        Block::Heading { runs, .. } => {
            // Headings keep the detail's single type size — bold is the cue
            // (an 18px line box can't host display sizes).
            let bold: Vec<InlineRun> = runs
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r.style.bold = true;
                    r
                })
                .collect();
            wrap_styled_runs(&bold, indent, out);
        }
        Block::CodeBlock { code, .. } => {
            let style = InlineStyle {
                code: true,
                ..InlineStyle::default()
            };
            for line in code.lines() {
                for chunk in wrap_cols(line, THOUGHT_WRAP_COLS.saturating_sub(indent).max(16)) {
                    let mut row = indent_run(indent);
                    if !chunk.is_empty() {
                        row.push(InlineRun {
                            text: chunk.to_string(),
                            style: style.clone(),
                        });
                    }
                    out.push(row);
                }
            }
        }
        Block::List {
            ordered_start,
            items,
        } => {
            // Tight rendering: no blank rows inside a list.
            for (ix, item) in items.iter().enumerate() {
                let marker = match ordered_start {
                    Some(start) => format!("{}. ", start + ix as u64),
                    None => "• ".to_string(),
                };
                let inner = indent + marker.chars().count();
                let mark = out.len();
                for child in item {
                    thought_block_lines(child, inner, out);
                }
                if out.len() == mark {
                    // An empty item still shows its marker.
                    out.push(indent_run(inner));
                }
                // The item's first line trades its indent spaces for the
                // marker (the slot-0 run is always the indent).
                if let Some(first) = out[mark].first_mut() {
                    first.text = format!("{}{marker}", " ".repeat(indent));
                }
            }
        }
        Block::BlockQuote { children } => {
            let mark = out.len();
            for (ix, child) in children.iter().enumerate() {
                if ix > 0 {
                    out.push(Vec::new());
                }
                thought_block_lines(child, indent + 2, out);
            }
            // Trade the two quote-indent spaces for the bar on every quoted
            // line — replace, not overwrite: nested list handlers already
            // planted markers after their own deeper indent.
            for line in &mut out[mark..] {
                if let Some(first) = line.first_mut()
                    && first.text.len() >= indent + 2
                {
                    first.text.replace_range(indent..indent + 2, "│ ");
                }
            }
        }
        Block::Table { header, rows, .. } => {
            // A thought is a record, not a layout surface: cells joined with
            // a dot separator, header bold — no column machinery.
            let join = |cells: &[Vec<InlineRun>], bold: bool| -> Vec<InlineRun> {
                let mut line: Vec<InlineRun> = Vec::new();
                for (ix, cell) in cells.iter().enumerate() {
                    if ix > 0 {
                        push_styled(&mut line, " · ", &InlineStyle::default());
                    }
                    for r in cell {
                        let mut r = r.clone();
                        r.style.bold |= bold;
                        line.push(r);
                    }
                }
                line
            };
            wrap_styled_runs(&join(header, true), indent, out);
            for row in rows {
                wrap_styled_runs(&join(row, false), indent, out);
            }
        }
        Block::Rule => {
            let mut row = indent_run(indent);
            row.push(InlineRun {
                text: "———".into(),
                style: InlineStyle::default(),
            });
            out.push(row);
        }
    }
}

/// A reasoning part as a tool-group chip: "Thought process" header over the
/// thought's markdown flattened into styled detail lines (analytic height —
/// the group's fold tween needs it; see [`thought_lines`]). Capped like tool
/// outputs, with the counted tail. `live` = the part is still streaming
/// (chip defaults open).
pub(super) fn thought_item(tree: &BlockTree, live: bool) -> ToolItem {
    let mut lines = thought_lines(tree);
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    if truncated_by > 0 {
        // Keep the TAIL while streaming (the fresh thinking is the signal);
        // settled thoughts keep the head like tool outputs do.
        if live {
            lines.drain(..truncated_by);
            // The cut can land on a block separator — drop the orphan blank.
            while lines
                .first()
                .is_some_and(|l| l.iter().all(|r| r.text.trim().is_empty()))
            {
                lines.remove(0);
            }
        } else {
            lines.truncate(OUTPUT_DETAIL_MAX_LINES);
        }
    }
    ToolItem {
        call: ToolCall::Unknown {
            name: "Thought process".into(),
            input: None,
        },
        is_error: false,
        resolved: !live,
        detail: (!lines.is_empty()).then(|| {
            Arc::new(ToolDetail::Thought {
                lines,
                truncated_by,
            })
        }),
        invocation: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
        is_thought: true,
    }
}

/// A chip's expandable detail payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDetail {
    /// Command/tool output as a code block: verbatim lines (indentation
    /// intact), capped at [`OUTPUT_DETAIL_MAX_LINES`] with a counted tail.
    Output {
        lines: Vec<SharedString>,
        truncated_by: usize,
    },
    /// A thought's markdown, pre-flattened into wrapped STYLED lines — one
    /// fixed-height row each, so the height stays analytic like `Output`
    /// while inline markers render as real styling ([`thought_lines`]).
    Thought {
        lines: Vec<Vec<InlineRun>>,
        truncated_by: usize,
    },
    /// A file diff, in the changes pane's model: hunks with 3 lines of
    /// context, dual line numbers, and (for recognized languages) syntax
    /// tokens — rendered by `changes::render_file_body`.
    Diff {
        file: Arc<crate::changes::FileDiff>,
        old_text: Option<Arc<str>>,
        new_text: Option<Arc<str>>,
    },
    /// Per-file `+N −N` stat rows — what the thin doc keeps of an edit
    /// (chat2-sync A1). The full diff upgrades this to [`ToolDetail::Diff`]
    /// via the sidecar fetch.
    Stats {
        stats: Arc<Vec<zeron_doc::ToolDiffStat>>,
    },
}

/// Max verbatim output lines per chip before the counted tail row.
pub const OUTPUT_DETAIL_MAX_LINES: usize = 24;

/// Max diff lines an inline tool-diff detail renders — the detail is one
/// stacked element inside its transcript row, so it must stay bounded
/// (~600 lines ≈ 12.6k px, several screens of context before the cut).
pub const DIFF_DETAIL_MAX_LINES: usize = 600;

/// Per-line height of an output detail block (diff blocks use the changes
/// pane's own [`crate::changes::DIFF_LINE_HEIGHT`]).
pub const OUTPUT_LINE_HEIGHT: f32 = 18.0;

/// Vertical padding of an output detail body (py(6) × 2).
pub(super) const OUTPUT_BODY_PAD: f32 = 12.0;

/// The hairline between an expanded chip's header row and its detail body.
pub(super) const DETAIL_SEPARATOR: f32 = 1.0;

/// Build a tool part's expandable detail. A diff wins over raw output (it is
/// the more structured record of the same action); post-strip docs carry diff
/// STATS instead of inline diff text, which win the same way.
pub fn tool_detail(
    output: Option<&str>,
    diff: Option<&zeron_proto::ToolDiff>,
    diff_stats: Option<&[zeron_doc::ToolDiffStat]>,
) -> Option<ToolDetail> {
    if let Some(diff) = diff {
        let mut file = diff_to_file(diff);
        if file.hunks.is_empty() {
            return None;
        }
        // A transcript diff renders as one stacked element inside its row —
        // cap it so a whole-file rewrite (or fetched full-diff blob) can't
        // build tens of thousands of elements per frame. The changes pane
        // has no such cap; it virtualizes per line.
        crate::changes::truncate_file_lines(&mut file, DIFF_DETAIL_MAX_LINES);
        return Some(ToolDetail::Diff {
            file: Arc::new(file),
            old_text: diff.old_text.as_deref().map(Arc::from),
            new_text: Some(Arc::from(diff.new_text.as_str())),
        });
    }
    if let Some(stats) = diff_stats.filter(|s| !s.is_empty()) {
        return Some(ToolDetail::Stats {
            stats: Arc::new(stats.to_vec()),
        });
    }
    let output = output?;
    let mut lines: Vec<SharedString> = output
        .lines()
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    // Trim trailing blank output lines so the block hugs its content.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Columns at which an invocation line soft-wraps into continuation lines.
/// The wrap is char-counted, not measured — block heights must be analytic —
/// so the budget is sized to fit the narrowest useful transcript pane.
pub const CALL_WRAP_COLS: usize = 80;

/// Soft-wrap one raw line into [`CALL_WRAP_COLS`]-char chunks so a long
/// single-line command stays fully readable instead of ellipsizing.
pub(super) fn wrap_cols(line: &str, cols: usize) -> Vec<SharedString> {
    if line.chars().count() <= cols {
        return vec![SharedString::from(line.to_owned())];
    }
    line.chars()
        .collect::<Vec<_>>()
        .chunks(cols)
        .map(|chunk| SharedString::from(chunk.iter().collect::<String>()))
        .collect()
}

/// Build a chip's full-invocation block — the complete tool call the header
/// truncates to one line: the whole command, pattern, or URL, todo items one
/// per line, MCP/unknown input as pretty-printed JSON. Reuses the output
/// code-block payload so rendering and height stay one implementation.
pub fn call_block(call: &ToolCall) -> Option<ToolDetail> {
    let text: String = match call {
        ToolCall::Exec { command } => command.clone(),
        ToolCall::ReadFile { path } => path.clone(),
        ToolCall::WriteFile { path, content } => match content {
            Some(content) => format!("{path}\n{content}"),
            None => path.clone(),
        },
        ToolCall::EditFile { path, .. } => path.clone(),
        ToolCall::ApplyPatch { path } => path.clone().unwrap_or_else(|| "workspace".into()),
        ToolCall::Search { pattern, path } => match path {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.clone(),
        },
        ToolCall::Glob { pattern } => pattern.clone(),
        ToolCall::WebFetch { url, prompt } => match prompt {
            Some(prompt) => format!("{url}\n{prompt}"),
            None => url.clone(),
        },
        ToolCall::WebSearch { query } => query.clone(),
        ToolCall::Todo { items } => items
            .iter()
            .map(|i| format!("{} {}", if i.done { "[x]" } else { "[ ]" }, i.text))
            .collect::<Vec<_>>()
            .join("\n"),
        ToolCall::Mcp {
            server,
            tool,
            input,
        } => match input {
            Some(input) => format!(
                "{server} · {tool}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => format!("{server} · {tool}"),
        },
        ToolCall::Unknown { name, input } => match input {
            Some(input) => format!(
                "{name}\n{}",
                serde_json::to_string_pretty(input).unwrap_or_default()
            ),
            None => name.clone(),
        },
    };
    let mut lines: Vec<SharedString> = text
        .lines()
        .flat_map(|l| wrap_cols(l, CALL_WRAP_COLS))
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    let truncated_by = lines.len().saturating_sub(OUTPUT_DETAIL_MAX_LINES);
    lines.truncate(OUTPUT_DETAIL_MAX_LINES);
    Some(ToolDetail::Output {
        lines,
        truncated_by,
    })
}

/// Reduce an inline [`zeron_proto::ToolDiff`] to the changes pane's
/// [`crate::changes::FileDiff`]: hunks grouped with 3 context lines, dual
/// 1-based line numbers, unified-diff hunk headers, and add/del counts.
pub fn diff_to_file(diff: &zeron_proto::ToolDiff) -> crate::changes::FileDiff {
    use crate::changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
    let old = diff.old_text.as_deref().unwrap_or("");
    let text_diff = similar::TextDiff::from_lines(old, &diff.new_text);
    let mut hunks = Vec::new();
    let (mut additions, mut deletions) = (0u32, 0u32);
    let mut max_line = 0u32;
    for group in text_diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let header = format!(
            "@@ -{},{} +{},{} @@",
            old_range.start + 1,
            old_range.len(),
            new_range.start + 1,
            new_range.len(),
        );
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let kind = match change.tag() {
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        LineKind::Del
                    }
                    similar::ChangeTag::Insert => {
                        additions += 1;
                        LineKind::Add
                    }
                    similar::ChangeTag::Equal => LineKind::Context,
                };
                let old_no = change.old_index().map(|n| n as u32 + 1);
                let new_no = change.new_index().map(|n| n as u32 + 1);
                max_line = max_line.max(old_no.unwrap_or(0)).max(new_no.unwrap_or(0));
                lines.push(DiffLine {
                    kind,
                    old_no,
                    new_no,
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
        hunks.push(Hunk { header, lines });
    }
    FileDiff {
        path: diff.path.clone(),
        old_path: None,
        status: if diff.old_text.is_none() {
            FileStatus::Added
        } else {
            FileStatus::Modified
        },
        binary: false,
        notices: Vec::new(),
        hunks,
        additions,
        deletions,
        max_line,
    }
}
