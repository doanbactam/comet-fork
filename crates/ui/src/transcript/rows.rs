//! Block-granularity transcript row model (pure).

use super::*;



#[derive(Clone)]
pub enum RowKind {
    User {
        /// Visible prompt (attachment-ref trailer already stripped). When the
        /// prompt carries file mentions this is the *projected* display text —
        /// chip labels in place of the raw Markdown links.
        text: SharedString,
        /// File-mention chips over `text`, in display-byte terms. Computed
        /// once per entry change in [`rows_for_entry`] (rows are cached by
        /// fingerprint), never per frame. Empty for ordinary prompts.
        mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
        /// Image refs parsed out of the message text (message-attachments.ts):
        /// thumbnails load from the owning device via ReadAttachmentChunk.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Context the prompt folded in as text, lifted back out by `badges`.
        badges: Arc<Vec<crate::badges::MessageBadge>>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message. Split per block like
    /// completed rows (only the tail blocks' versions change per commit, so
    /// the settled prefix is never respliced or re-rendered); rendered with
    /// the fade veil.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        auto_open: bool,
    },
    InputChip {
        /// First question's header (chat-view.tsx `InputChip`: the resolved
        /// chip shows it; unresolved shows "Awaiting your answer…" — which
        /// stays TRUE even across a run death: the composer keeps the panel
        /// up until the user answers, and the engine delivers a dead run's
        /// answer as a resumed turn).
        header: SharedString,
        resolved: bool,
    },
    ErrorChip {
        message: SharedString,
    },
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: SharedString,
    pub version: u64,
    /// First row of its message entry (gets the turn gap).
    pub turn_start: bool,
    pub kind: RowKind,
    /// The owning message entry — hover anywhere on the entry's rows reveals
    /// its timestamp strip (zeron chat-view.tsx `group`/`group-hover`).
    pub entry_id: SharedString,
    /// Epoch-ms for the 16px hover-timestamp strip UNDER this row: set on the
    /// LAST row of a completed entry (user rows always; assistant rows only
    /// once streaming ends — "the turn isn't at a time yet", chat-view.tsx).
    pub timestamp: Option<i64>,
    /// Text copied by the entry-level hover action. Present only on the last
    /// settled row, beside the timestamp; tools and transport-only metadata
    /// are deliberately excluded.
    pub copy_text: Option<SharedString>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM" — the exact
/// `formatTimestamp` shape (utils.ts: short month, numeric day, hour,
/// 2-digit minutes, no leading zero on the hour). Pure over an explicit
/// timezone so tests don't depend on the host's local time.
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

pub(super) fn tool_fingerprint(tools: &[ToolItem], auto_open: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for t in tools {
        let (label, detail) = tool_chip_content(&t.call);
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(&(detail.len() as u32).to_le_bytes());
        acc.push(t.is_error as u8 | (t.resolved as u8) << 1);
        // Detail payload arriving (or growing) must re-splice the row even
        // when the resolved bit didn't change.
        match t.detail.as_deref() {
            None => acc.push(0),
            Some(ToolDetail::Output {
                lines,
                truncated_by,
            }) => {
                acc.push(1);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                let bytes: usize = lines.iter().map(|l| l.len()).sum();
                acc.extend_from_slice(&(bytes as u32).to_le_bytes());
            }
            Some(ToolDetail::Thought {
                lines,
                truncated_by,
            }) => {
                // Byte-exact plus style bits: a live mend can restyle runs
                // without changing the flattened length, and the row must
                // still re-splice.
                acc.push(4);
                acc.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
                for line in lines {
                    for run in line {
                        acc.extend_from_slice(run.text.as_bytes());
                        acc.push(
                            run.style.bold as u8
                                | (run.style.italic as u8) << 1
                                | (run.style.code as u8) << 2
                                | (run.style.strikethrough as u8) << 3
                                | (run.style.link.is_some() as u8) << 4,
                        );
                    }
                    acc.push(b'\n');
                }
            }
            Some(ToolDetail::Diff { file, .. }) => {
                acc.push(2);
                acc.extend_from_slice(file.path.as_bytes());
                acc.extend_from_slice(&file.additions.to_le_bytes());
                acc.extend_from_slice(&file.deletions.to_le_bytes());
                acc.extend_from_slice(&(file.hunks.len() as u32).to_le_bytes());
            }
            Some(ToolDetail::Stats { stats }) => {
                acc.push(3);
                for stat in stats.iter() {
                    acc.extend_from_slice(stat.path.as_bytes());
                    acc.extend_from_slice(&stat.additions.to_le_bytes());
                    acc.extend_from_slice(&stat.deletions.to_le_bytes());
                }
            }
        }
        // The invocation block is pure over `call`, which the one-line hash
        // above only covers by length — hash its bytes so an in-place call
        // update (a streaming MCP input, a growing todo list) re-splices.
        if let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = t.invocation.as_deref()
        {
            for line in lines {
                acc.extend_from_slice(line.as_bytes());
            }
            acc.extend_from_slice(&(*truncated_by as u32).to_le_bytes());
        }
        // Sidecar refs arriving after the resolve tick must re-splice too —
        // they add the fetch affordance without changing the detail payload.
        acc.push(t.output_ref.is_some() as u8 | (t.diff_ref.is_some() as u8) << 1);
        // Subagent lifecycle mutates the chip in place (status flips, the
        // live tail grows) — hash it so the row re-splices on every change.
        acc.push(
            t.subagent_ref.is_some() as u8
                | match t.subagent_status {
                    None => 0,
                    Some(SubagentStatus::Running) => 1 << 1,
                    Some(SubagentStatus::Done) => 2 << 1,
                    Some(SubagentStatus::Failed) => 3 << 1,
                },
        );
        if let Some(tail) = &t.subagent_tail {
            acc.extend_from_slice(tail.as_bytes());
        }
    }
    acc.push(auto_open as u8);
    fnv1a(&acc)
}

/// Clipboard payload for an assistant/system entry: authored text parts in
/// document order, preserving Markdown while excluding tool traces and other
/// structured parts.
pub(super) fn assistant_copy_text(entry: &SessionMessageEntry) -> Option<SharedString> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            // Inspect the trimmed view only to reject empty parts. Copy the
            // original bytes so indentation-based code blocks and Markdown
            // hard-break whitespace survive the clipboard round trip.
            MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then(|| text.into())
}

/// Build the block rows of one (already continuation-joined) entry.
///
/// `parse` maps `(part_key, text)` to a block tree — the entity supplies
/// incremental parsers for live parts and a cache for complete ones; tests pass
/// a plain `parse_full`.
pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);
    let entry_id: SharedString = entry.id.clone().into();

    if entry.role == MessageRole::User {
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // Attachment refs ride the plain text (the `withAttachments`
        // transport); split them back out for the thumbnail strip.
        let parsed = crate::attachments::parse_user_message_images(&raw);
        // File mentions render as chips here too, not just in the composer.
        // The projection is pure over the text, so the raw-length row version
        // below stays a valid cache/diff key.
        // Lifted before the mention projection, so a comment body's own
        // Markdown never lands in the bubble.
        let (body, badges) = crate::badges::split(&parsed.text);
        let (text, mentions) = match crate::composer::sent_mention_display(&body) {
            Some((display, spans)) => (display, spans),
            None => (body, Vec::new()),
        };
        let copy_text = (!text.trim().is_empty()).then(|| SharedString::from(text.clone()));
        return vec![Row {
            id: entry.id.clone().into(),
            version: (raw.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                text: text.into(),
                mentions: Arc::new(mentions),
                attachments: Arc::new(parsed.attachments),
                badges: Arc::new(badges),
                pending,
            },
            entry_id,
            // User rows always carry the strip (chat-view.tsx: whenever
            // `createdAt` exists — the optimistic echo included).
            timestamp: Some(entry.created_at),
            copy_text,
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive
    // ordinary tools. Agent/spawn chips flush into their own group so they
    // never share a collapse with Reads/Runs.
    let last_part_ix = entry.parts.len().saturating_sub(1);
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let auto_open = streaming && last_ix == last_part_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix).into(),
                version: tool_fingerprint(&tools, auto_open),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    auto_open,
                },
                entry_id: entry.id.clone().into(),
                timestamp: None,
                copy_text: None,
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } => {
                let item = ToolItem {
                    call: call.clone(),
                    is_error: *is_error,
                    resolved: *resolved,
                    detail: tool_detail(output.as_deref(), diff.as_ref(), diff_stats.as_deref())
                        .map(Arc::new),
                    invocation: call_block(call).map(Arc::new),
                    output_ref: output_ref.clone().map(SharedString::from),
                    output_bytes: *output_bytes,
                    diff_ref: diff_ref.clone().map(SharedString::from),
                    subagent_ref: subagent_ref.clone().map(SharedString::from),
                    subagent_status: *subagent_status,
                    subagent_tail: subagent_tail.clone().map(SharedString::from),
                    is_thought: false,
                };
                // Agent chips don't share a fold with ordinary tools: flush
                // whenever the genus flips so each group is uniform.
                if pending_group
                    .first()
                    .is_some_and(|head| is_agent_tool(head) != is_agent_tool(&item))
                {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            // Thinking rides the SAME accordion as the tools around it
            // (user request) — a thought chip in the group, not its own row.
            MessagePart::Reasoning { id: part_id, text } => {
                if text.trim().is_empty() {
                    continue;
                }
                // Live only while it is the tail of a streaming reply — once
                // text or a tool follows, the thought is finished even though
                // the entry still streams.
                let live = streaming && part_ix == last_part_ix;
                // The same parse wiring as text parts: incremental while
                // streaming, hanging inline markers mended for display, the
                // settled cache once complete.
                let tree = parse(&format!("{}#{}", entry.id, part_id), text);
                let item = thought_item(&tree, live);
                // Thoughts join ordinary tool groups; agent (spawn-link)
                // groups stay pure, exactly like the tool genus rule.
                if pending_group.first().is_some_and(is_agent_tool) {
                    flush_group(
                        &mut rows,
                        &mut pending_group,
                        &mut group_ix,
                        group_last_part_ix,
                    );
                }
                pending_group.push(item);
                group_last_part_ix = part_ix;
            }
            other => {
                flush_group(
                    &mut rows,
                    &mut pending_group,
                    &mut group_ix,
                    group_last_part_ix,
                );
                match other {
                    MessagePart::Text { id: part_id, text } => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, text);
                        // Live and completed parts split identically — one row
                        // per top-level block, same ids, so the live→complete
                        // handoff never changes row identity. The version is a
                        // content hash of the block's bytes (LSB = streaming),
                        // so a commit only splices rows whose bytes actually
                        // changed — the settled prefix of a live reply is
                        // untouched (and its render caches stay valid).
                        for block_ix in 0..tree.blocks.len() {
                            let range = &tree.blocks[block_ix].range;
                            let end = range.end.min(text.len());
                            let bytes = text
                                .as_bytes()
                                .get(range.start.min(end)..end)
                                .unwrap_or_default();
                            let version = (fnv1a(bytes) << 1) | streaming as u64;
                            rows.push(Row {
                                id: format!("{key}.{block_ix}").into(),
                                version,
                                turn_start: false,
                                entry_id: entry_id.clone(),
                                timestamp: None,
                                copy_text: None,
                                kind: if streaming {
                                    RowKind::LiveMarkdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                } else {
                                    RowKind::Markdown {
                                        tree: tree.clone(),
                                        block_ix,
                                    }
                                },
                            });
                        }
                    }
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        // Model-generated header onto the one-line chip.
                        let header: SharedString = single_line(
                            &questions
                                .first()
                                .map(|q| q.header.clone())
                                .unwrap_or_else(|| "Question".to_string()),
                        )
                        .into();
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                header,
                                resolved: *resolved,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Error {
                        id: part_id,
                        message,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: message.len() as u64,
                            turn_start: false,
                            kind: RowKind::ErrorChip {
                                // Harness-generated; the chip is one line.
                                message: single_line(message).into(),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    // Tools and thoughts are grouped by the outer arms;
                    // nothing reaches here.
                    MessagePart::Tool { .. } | MessagePart::Reasoning { .. } => {}
                }
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    // Timestamp strip under the entry's LAST row once the turn has settled
    // (chat-view.tsx: "No timestamp hover mid-stream"). The version bit keeps
    // the diff key honest for last-row kinds whose own version wouldn't
    // change when streaming flips off (chips).
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.copy_text = assistant_copy_text(entry);
        last.version ^= 1 << 62;
    }
    rows
}

/// `ZERON_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
pub(super) fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("ZERON_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

pub(super) const FRAME_STATS_WINDOW: usize = 240;

/// `ZERON_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
pub(super) fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("ZERON_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

pub(super) fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

/// How [`parse_for_row`] produced its tree — carries the incremental parser's
/// work counters so callers (and tests) can see that per-append parse work is
/// bounded by the reparsed tail, never the whole accumulated reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live [`IncrementalParser`] advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched (render caches stay valid).
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache (no parse at all).
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring, extracted for testability: one call
/// per text part per sync. Streaming parts keep one [`IncrementalParser`] per
/// row key and advance it with the full accumulated text (`set_text` takes the
/// O(tail) append path for the prefix-extensions the doc watch delivers);
/// completed parts hit the settled cache, adopt the live parser's tree on the
/// live→complete flip (flicker-free handoff), or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            // Display tree: hanging inline markers mended so closers arriving
            // later never reflow painted text (markdown/mend.rs). Completed
            // rows below use the canonical tree — the honest settle.
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        // On the live→complete flip reuse the live parser's tree when
        // the sources match — the split rows then share the exact tree
        // the unsplit row painted, guaranteeing a flicker-free handoff.
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
pub(super) fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel. Tool groups get one larger global
/// step on either boundary so their dense chip stack has room to breathe.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return Theme::SPACE_LG;
    }
    let is_md = |k: &RowKind| matches!(k, RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. });
    let same_part_markdown = prev.is_some_and(|p| {
        is_md(&p.kind) && is_md(&row.kind) && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else if matches!(row.kind, RowKind::ToolGroup { .. })
        || prev.is_some_and(|row| matches!(row.kind, RowKind::ToolGroup { .. }))
    {
        Theme::SPACE_MD
    } else {
        Theme::SPACE_SM
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}
