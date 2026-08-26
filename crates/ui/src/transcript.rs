//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/research/mugen-pretext.md §3):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups
//!   (agent/spawn chips split out so they never collapse) and input/error chips;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; the first send in an empty chat anchors the prompt at
//! the viewport top and hands off to the same glide when the reply overflows.
//! While that anchor holds, wheel/touch is clamped rather than obeyed — the
//! whole turn is already visible, so there is nothing to scroll to.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, Bounds, ClipboardItem, Context, Entity, ListAlignment, ListOffset,
    ListScrollEvent, ListState, MouseButton, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels,
    Point, SharedString, StyledImage as _, StyledText, Subscription, Task, TextRun, Window, canvas,
    div, img, list, prelude::*, px, quad,
};

use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use zeron_proto::ToolCall;

use crate::markdown::parser::{Block, BlockTree, IncrementalParser, InlineRun, parse_full};
use crate::markdown::render::{self, RenderCache, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::syntax_cache::{DocumentHighlightKey, SyntaxHighlightCache};
use crate::theme::Theme;
use zeron_syntax::LanguageId as Lang;

mod stick;
mod tools;
mod rows;
mod sidecar;
mod view;
pub use stick::{
    StickSpring, AT_BOTTOM_PX, GLIDE_MAX_VIEWPORTS, SPRING_CHASE_MAX_LEAD, SPRING_DAMPING,
    SPRING_FRAME_MS, SPRING_GROWTH_EMA, SPRING_MASS, SPRING_MAX_CATCHUP_FRAMES,
    SPRING_SETTLE_GRACE_MS, SPRING_STIFFNESS,
};
pub(crate) use stick::OWN_SEND_TOP_INSET_PX;
pub use sidecar::BLOB_AFFORDANCE_HEIGHT;
pub use rows::{
    diff_rows, format_timestamp, parse_for_row, rows_for_entry, top_gap_for, ParseOutcome, Row,
    RowKind,
};
pub use tools::{
    call_block, diff_to_file, tool_detail, ToolDetail, ToolItem, CALL_WRAP_COLS,
    DIFF_DETAIL_MAX_LINES, OUTPUT_DETAIL_MAX_LINES, OUTPUT_LINE_HEIGHT,
};
use rows::*;
use sidecar::*;
use stick::{
    own_turn_reservation, should_anchor_live_stream, OWN_SEND_GLIDE_RETAIN, OWN_SEND_GLIDE_SNAP_PX,
    OWN_SEND_SCROLL_SLACK_PX,
};
use tools::{
    is_agent_call, is_agent_tool, is_spawn_link, thought_item, thought_lines, tool_group_collapses,
    DETAIL_SEPARATOR, OUTPUT_BODY_PAD, THOUGHT_WRAP_COLS,
};

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
pub const OVERDRAW_PX: f32 = 320.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Bound session-local viewport memory independently of total chat history.
const MAX_SAVED_VIEWPORTS: usize = 256;
/// Text-selection edge scrolling runs only during a drag. A 24 ms cadence is
/// smooth enough to track text while avoiding a permanent animation-frame loop
/// on low-end devices.
const SELECTION_SCROLL_TICK_MS: u64 = 24;
const SELECTION_SCROLL_EDGE_PX: f32 = 36.0;
const SELECTION_SCROLL_MAX_STEP_PX: f32 = 24.0;
/// Transcript column max width (zeron 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// Tool chip row height / gap — analytic, so fold heights need no measurement.
/// A row is the guide rail + a 30px chip card centered in it (zeron
/// tool-chip.tsx: `TOOL_CHIP_HEIGHT = 38`, card `h-[30px]`); rows stack with no
/// gap so the rail reads continuous.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
/// Inner height of the chip header: [`CHIP_CARD_HEIGHT`] is the card's
/// border-box (explicit `h` in gpui includes the 1px border), so a 30px
/// header inside a 30px bordered card clips 2px off the bottom and every
/// glyph/icon reads high (user report).
const CHIP_HEADER_HEIGHT: f32 = CHIP_CARD_HEIGHT - 2.0;

/// Signed list scroll step for a pointer near a viewport edge.
///
/// GPUI list offsets increase toward the document bottom. The quadratic ramp
/// keeps entry into the edge zone gentle and reaches full speed at the edge.
fn selection_scroll_step(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
    let height = f32::from(bounds.size.height);
    if height <= 0.0 {
        return 0.0;
    }
    let edge = SELECTION_SCROLL_EDGE_PX.min(height / 3.0);
    if edge <= 0.0 {
        return 0.0;
    }
    let y = f32::from(position.y);
    let top = f32::from(bounds.top());
    let bottom = f32::from(bounds.bottom());
    let scaled = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        SELECTION_SCROLL_MAX_STEP_PX * t * t
    };
    if y < top + edge {
        -scaled(top + edge - y)
    } else if y > bottom - edge {
        scaled(y - (bottom - edge))
    } else {
        0.0
    }
}
const CHIPS_TOP_PAD: f32 = 2.0;
/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// User-bubble attachment thumbnails (user-attachments.tsx): 112×80 thumbs in
/// a FIXED-height strip (load-state flips never shift the virtualizer).
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools
        .iter()
        .filter(|t| !t.is_thought)
        .map(|t| (t.call.clone(), t.is_error))
        .collect();
    let thoughts = tools.iter().filter(|t| t.is_thought).count();
    // The shared summary answers "used 0 tools" for an empty set — a
    // thought-only group must not inherit that.
    let base = if pairs.is_empty() {
        String::new()
    } else {
        zeron_proto::view::tool_group_summary(&pairs)
    };
    // Thought chips ride the group (they are UI-synthesized, so the shared
    // view summary never sees them): name them on the collapsed line.
    match (base.is_empty(), thoughts) {
        (_, 0) => base,
        (true, 1) => "Thought process".into(),
        (true, n) => format!("Thought {n} times"),
        (false, 1) => format!("Thought · {base}"),
        (false, n) => format!("Thought {n} times · {base}"),
    }
}

// `single_line` and the per-kind chip label/detail are shared with the terminal
// viewport (`zeron_proto::view`): a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use zeron_proto::view::{single_line, tool_chip_content};

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

/// Analytic height an open detail adds to its chip's card (separator + body)
/// — output blocks by line count, diff blocks via the changes pane's own
/// [`crate::changes::body_height`]. The chip's own [`CHIP_HEIGHT`] is already
/// counted by [`chips_height`].
pub fn detail_height(detail: &ToolDetail) -> f32 {
    let body = match detail {
        ToolDetail::Output {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => {
            let rows = lines.len() + usize::from(*truncated_by > 0);
            rows as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD
        }
        ToolDetail::Diff { file, .. } => crate::changes::body_height(file),
        ToolDetail::Stats { stats } => stats.len() as f32 * OUTPUT_LINE_HEIGHT + OUTPUT_BODY_PAD,
    };
    DETAIL_SEPARATOR + body
}

/// Rotating flavour vocabulary (20 words / 7s, seeded per chat).
pub const FLAVOUR_WORDS: [&str; 20] = [
    "Thinking",
    "Pondering",
    "Scheming",
    "Brewing",
    "Weaving",
    "Tinkering",
    "Musing",
    "Composing",
    "Sifting",
    "Untangling",
    "Distilling",
    "Sketching",
    "Plotting",
    "Riffing",
    "Combobulating",
    "Percolating",
    "Marinating",
    "Noodling",
    "Puzzling",
    "Conjuring",
];
pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The flavour word for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// The working trailer's "Sending…" bridge: true while an in-flight send is
/// fresher than the session row's turn start — the row still carries the
/// PREVIOUS turn (or none), so a timer would count the send round-trip and
/// restart when the turn actually begins.
pub fn sending_bridge(
    send_started: Option<chrono::DateTime<chrono::Utc>>,
    turn_started: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    match (send_started, turn_started) {
        (Some(send), Some(turn)) => turn <= send,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

struct HighlightEntry {
    key: DocumentHighlightKey,
    document: Option<Weak<zeron_syntax::HighlightedDocument>>,
    _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
    cache: SyntaxHighlightCache,
}

impl HighlightStore {
    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<zeron_syntax::HighlightedDocument>> {
        let slot_key = (row_id.clone(), block_ix);
        let document_key = DocumentHighlightKey::new(lang, code);
        if let Some(entry) = self.entries.get(&slot_key)
            && entry.key == document_key
        {
            let document = entry.document.as_ref()?;
            if let Some(document) = document.upgrade() {
                return Some(document);
            }
        }
        if let Some(document) = self.cache.get(&document_key) {
            self.entries.insert(
                slot_key,
                HighlightEntry {
                    key: document_key,
                    document: Some(Arc::downgrade(&document)),
                    _task: None,
                },
            );
            return Some(document);
        }
        let code = code.to_string();
        let source_bytes = code.len();
        let task = cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let document = cx
                .background_executor()
                .spawn(async move {
                    zeron_syntax::highlight(zeron_syntax::HighlightRequest {
                        source: &code,
                        path: None,
                        fence_tag: Some(match lang {
                            Lang::Rust => "rust",
                            Lang::JavaScript => "javascript",
                            Lang::Jsx => "jsx",
                            Lang::TypeScript => "typescript",
                            Lang::Tsx => "tsx",
                            Lang::Python => "python",
                            Lang::Go => "go",
                            Lang::Json => "json",
                            Lang::Jsonc => "jsonc",
                            Lang::Bash => "bash",
                            Lang::Toml => "toml",
                            Lang::Markdown => "markdown",
                            Lang::Html => "html",
                            Lang::Css => "css",
                            Lang::Yaml => "yaml",
                            Lang::C => "c",
                            Lang::Cpp => "cpp",
                            Lang::CSharp => "csharp",
                            Lang::Java => "java",
                            Lang::Kotlin => "kotlin",
                            Lang::Swift => "swift",
                            Lang::Ruby => "ruby",
                            Lang::Php => "php",
                            Lang::Sql => "sql",
                            Lang::Lua => "lua",
                            Lang::Dockerfile => "dockerfile",
                            Lang::Nix => "nix",
                            Lang::Make => "make",
                        }),
                    })
                    .ok()
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(document) = document {
                    let document = Arc::new(document);
                    let retained = transcript
                        .highlights
                        .cache
                        .insert(document_key, document.clone());
                    if let Some(entry) = transcript.highlights.entries.get_mut(&slot_key)
                        && entry.key == document_key
                    {
                        tracing::debug!(
                            language = ?lang,
                            source_bytes,
                            spans = document.lines.iter().map(Vec::len).sum::<usize>(),
                            elapsed_us = started.elapsed().as_micros() as u64,
                            "syntax highlight ready"
                        );
                        entry.document = retained.then(|| Arc::downgrade(&document));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                key: document_key,
                document: None,
                _task: Some(task),
            },
        );
        None
    }
}

// ---------------------------------------------------------------------------
// Transcript entity
// ---------------------------------------------------------------------------

struct CachedRows {
    fingerprint: u64,
    rows: Vec<Row>,
}

#[derive(Default, Clone, Copy)]
struct FoldState {
    /// User pin (click); `None` follows the auto-open rule.
    open: Option<bool>,
    /// Bumped per toggle — keys the 200ms height tween.
    epoch: usize,
    /// Height at the moment of the toggle (the tween's start). The destination
    /// is always the *current* target height, so content growth after a toggle
    /// snaps instead of replaying a stale tween.
    from: f32,
    /// When the toggle happened. The tween is armed only for a short window
    /// after the click: gpui replays an element's animation on REMOUNT, and a
    /// virtualized row scrolling back into view is a remount — an armed-forever
    /// tween made every once-collapsed group flash open→closed on each
    /// reappearance (user report).
    toggled_at: Option<Instant>,
}

/// Layout state for the most recent locally-sent turn (notes-app parity):
/// EVERY send reserves the space below the prompt for the reply — a trailing
/// runway pad sized `usable − turn height`, i.e. a min-height for the turn,
/// shrinking 1:1 as the reply streams so the held layout never moves. The
/// entry is an eased glide onto the prompt; landed, the hold re-asserts the
/// prompt's position absolutely after every layout (the bottom spring can't
/// hold here: parking at exact distance 0 re-glues gpui's list, which then
/// hard-tracks the pad's stale bottom on every commit — rig-traced). Wheel
/// input releases the hold, leaving the reservation as plain scrollable
/// space. The anchor retires once the reply overflows the reservation (pad
/// ~0, height-neutral). Chat switches snapshot its runway with the viewport
/// and restore it released, so revisiting never resumes hidden auto-follow.
#[derive(Clone, Debug)]
struct OwnTurnAnchor {
    chat_id: String,
    message_id: SharedString,
    /// Current reservation pad on the last row (`usable − turn_height`).
    runway: f32,
    /// The step still owns the viewport (glide → hold). Any wheel/touch
    /// input releases it — the reservation stays behind as plain scrollable
    /// space, and the ordinary escape/restick rules apply from then on.
    held: bool,
    /// The entry glide has landed; the hold now re-asserts the prompt's
    /// position absolutely after every layout (glue- and lag-proof — the
    /// exact mechanism the shipped first-send anchor used).
    positioned: bool,
    /// A fresh send may install the anchor one notification before its echo.
    /// Once the prompt has appeared, its later disappearance is terminal
    /// (failed echo or removed entry) and the runway must retire.
    seen_prompt: bool,
}

impl OwnTurnAnchor {
    fn released_for_restore(mut self) -> Self {
        self.held = false;
        self.positioned = false;
        self.seen_prompt = true;
        self
    }

    fn observe_prompt(&mut self, exists: bool) -> bool {
        if exists {
            self.seen_prompt = true;
        }
        exists || !self.seen_prompt
    }
}

/// A stable per-chat viewport anchor. Row identity is preferred over its old
/// index because async replay can insert or remove rows while a chat is away.
#[derive(Clone, Debug)]
struct ViewportAnchor {
    row_id: SharedString,
    entry_id: SharedString,
    fallback_ix: usize,
    offset_in_row: Pixels,
}

impl ViewportAnchor {
    fn capture(rows: &[Row], scroll_top: ListOffset) -> Option<Self> {
        let fallback_ix = scroll_top.item_ix.min(rows.len().checked_sub(1)?);
        let row = &rows[fallback_ix];
        Some(Self {
            row_id: row.id.clone(),
            entry_id: row.entry_id.clone(),
            fallback_ix,
            offset_in_row: scroll_top.offset_in_item,
        })
    }

    fn resolve_exact(&self, rows: &[Row]) -> Option<ListOffset> {
        let item_ix = rows.iter().position(|row| row.id == self.row_id)?;
        Some(ListOffset {
            item_ix,
            offset_in_item: self.offset_in_row,
        })
    }

    fn resolve(&self, rows: &[Row]) -> Option<ListOffset> {
        if let Some(offset) = self.resolve_exact(rows) {
            return Some(offset);
        }

        // A row can disappear when a streaming block is reshaped. Stay in the
        // same message entry, choosing the surviving row nearest the old
        // location; the intra-row offset is no longer meaningful in that case.
        let item_ix = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry_id == self.entry_id)
            .min_by_key(|(ix, _)| ix.abs_diff(self.fallback_ix))
            .map(|(ix, _)| ix)
            .unwrap_or_else(|| self.fallback_ix.min(rows.len().saturating_sub(1)));
        (!rows.is_empty()).then_some(ListOffset {
            item_ix,
            offset_in_item: px(0.0),
        })
    }
}

/// Session-local viewport state. Chats that were following their tail keep
/// following it; only user-owned viewports restore a concrete row anchor.
#[derive(Clone, Debug)]
enum SavedViewport {
    FollowTail,
    Anchored {
        anchor: ViewportAnchor,
        distance_from_bottom: f32,
        /// Preserve the runway that made a short active turn scrollable.
        /// Navigation releases its automatic hold, so revisiting restores the
        /// viewport without immediately following new output to the bottom.
        own_turn: Option<OwnTurnAnchor>,
    },
}

struct RestoredViewport {
    offset: ListOffset,
    distance_from_bottom: f32,
    own_turn: Option<OwnTurnAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewportFinalizeToken {
    generation: u64,
    layout_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptReplayState {
    Pending,
    Empty,
    Populated,
}

impl TranscriptReplayState {
    fn authoritative_empty(self) -> bool {
        self == Self::Empty
    }

    fn allows_fallback(self) -> bool {
        self == Self::Populated
    }
}

impl ViewportFinalizeToken {
    fn still_current(self, generation: u64) -> bool {
        self.generation == generation
    }

    fn layout_settled(self, layout_revision: u64) -> bool {
        self.layout_revision == layout_revision
    }
}

impl SavedViewport {
    fn capture(
        rows: &[Row],
        scroll_top: ListOffset,
        pinned: bool,
        distance_from_bottom: f32,
        own_turn: Option<&OwnTurnAnchor>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        if pinned {
            return Some(Self::FollowTail);
        }
        Some(Self::Anchored {
            anchor: ViewportAnchor::capture(rows, scroll_top)?,
            distance_from_bottom,
            own_turn: own_turn.cloned(),
        })
    }

    /// Before the opening reset arrives, rows may contain only optimistic
    /// echoes. In that gap an exact row is safe, but entry/index fallbacks
    /// would mistake an unrelated echo for the authoritative transcript.
    fn resolve(&self, rows: &[Row], allow_fallback: bool) -> Option<RestoredViewport> {
        let Self::Anchored {
            anchor,
            distance_from_bottom,
            own_turn,
        } = self
        else {
            return None;
        };
        let offset = if allow_fallback {
            anchor.resolve(rows)?
        } else {
            anchor.resolve_exact(rows)?
        };
        let own_turn = own_turn
            .clone()
            .filter(|turn| {
                rows.iter()
                    .any(|row| row.turn_start && row.entry_id == turn.message_id)
            })
            .map(OwnTurnAnchor::released_for_restore);
        Some(RestoredViewport {
            offset,
            distance_from_bottom: *distance_from_bottom,
            own_turn,
        })
    }
}

#[derive(Default)]
struct SavedViewportCache {
    by_chat: HashMap<String, SavedViewport>,
    recency: VecDeque<String>,
}

impl SavedViewportCache {
    fn insert(&mut self, chat_id: String, viewport: SavedViewport) {
        if self.by_chat.contains_key(&chat_id) {
            self.recency.retain(|candidate| candidate != &chat_id);
        }
        self.recency.push_back(chat_id.clone());
        self.by_chat.insert(chat_id, viewport);
        while self.by_chat.len() > MAX_SAVED_VIEWPORTS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.by_chat.remove(&evicted);
        }
    }

    fn get_cloned_and_touch(&mut self, chat_id: &str) -> Option<SavedViewport> {
        let viewport = self.by_chat.get(chat_id).cloned()?;
        self.recency.retain(|candidate| candidate != chat_id);
        self.recency.push_back(chat_id.to_string());
        Some(viewport)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_chat.len()
    }
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<String>,
    /// `Some(doc_id)` pins this instance to a SUBAGENT doc: rows come from
    /// `AppState::sub_transcript(doc_id)` instead of the selected chat, and
    /// the instance is READ-ONLY — no echoes, no own-turn hold, and no global
    /// attachment protection (that set is shared with the primary transcript
    /// and overwritten wholesale).
    doc_override: Option<String>,
    /// Whether an override instance watches a LIVE doc (`for_doc(follow)`):
    /// only then may the working trailer render — a frozen snapshot must
    /// never spin, whatever its entries claim.
    doc_live: bool,
    /// Memory-only viewport state for primary chats visited in this window.
    /// A transcript instance is shared across tabs, so the active ListState is
    /// reset on every attach and cannot retain these positions by itself.
    saved_viewports: SavedViewportCache,
    /// An anchored viewport waiting for the selected chat's async replay.
    pending_viewport: Option<SavedViewport>,
    /// Generation of the selected chat, guarding post-layout restoration
    /// callbacks across rapid A→B→A navigation.
    viewport_generation: u64,
    /// A restored item anchor needs one post-layout refresh of distance-based
    /// UI state; programmatic list scrolling never invokes `handle_scroll`.
    viewport_finalize_pending: bool,
    viewport_finalize_scheduled: bool,
    /// Bumped whenever sync or own-turn logic invalidates measured rows. The
    /// post-restore finalizer waits until one layout completes without another
    /// invalidation, avoiding a stale jump-button decision.
    viewport_layout_revision: u64,
    /// One-shot "open at the latest content" for UNPINNED (frozen) override
    /// instances: rows land ASYNC after the tab opens (watch replay / blob
    /// fetch), so the end-scroll fires on the first non-empty sync, then
    /// never again — landing at the end and FOLLOWING it are different
    /// states, and the user owns the viewport from there. Pinned instances
    /// don't need it (the pin branch already opens at the end).
    land_end_pending: bool,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Detail folds (output/diff) per chip, keyed `"{row_id}#d{ix}"` — full
    /// [`FoldState`]s so detail bodies tween open/closed exactly like the
    /// group fold. Render-local like `folds` — never part of the row
    /// fingerprint.
    tool_details: HashMap<SharedString, FoldState>,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    /// Last UI typography generation reflected in `list` item measurements.
    /// Family and size changes can alter prose wrapping without changing row
    /// identity, so the virtual list must explicitly discard cached heights.
    typography_generation: u32,
    highlights: HighlightStore,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, after an own-send first overflows, and
    /// on the jump button.
    pinned: bool,
    /// A locally-sent prompt currently held near the viewport top while its
    /// reply grows into the empty space below it.
    own_turn: Option<OwnTurnAnchor>,
    /// A layout-affecting change needs one post-layout own-turn measurement.
    own_turn_kick: bool,
    /// One own-turn `on_next_frame` callback in flight at most.
    own_turn_scheduled: bool,
    /// Wall-clock of the previous entry-glide tick (`None` = not gliding).
    own_turn_last_tick: Option<Instant>,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// Last pointer sample while markdown selection owns a left-button drag.
    selection_drag_position: Option<Point<Pixels>>,
    /// One-shot timer rescheduled only while the pointer remains in an edge
    /// zone. Dropping it on mouse-up stops all selection scroll work.
    selection_scroll_task: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Height of the shell's composer/status/terminal stack overlaying the
    /// transcript's bottom (measured last frame): the last row pads past it
    /// so pinned content rests above the glass chrome it scrolls under.
    bottom_clearance: f32,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer — reveals the entry's timestamp
    /// strip (zeron chat-view.tsx `group-hover`; the rows report hover
    /// themselves). Keyed by ROW so a row→row move within one entry can't
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Entry whose hover action is showing transient copied-check feedback.
    copied_message: Option<SharedString>,
    copied_message_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// Focused while the lightbox is open so Escape reaches it.
    attachment_preview_focus: gpui::FocusHandle,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(String, String), Task<()>>,
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(String, String), Task<()>>,
    /// Sidecar blob fetches keyed by doc ref (`chatId/partId[.diff]`,
    /// chat2-sync A3). `Ready` holds the UPGRADED detail, built once on
    /// arrival — render swaps it in per chip; rows never rebuild for it.
    /// Deliberately NOT cleared on chat switch: refs are chat-qualified and a
    /// fetched blob stays valid.
    blob_details: HashMap<SharedString, BlobFetch>,
    /// Monotonic fetch order per blob ref: when a tool has BOTH a diff and
    /// an output blob fetched, the chip shows the one requested most
    /// recently (click "Show full output" after a diff → see the output).
    blob_fetch_order: HashMap<SharedString, u64>,
    blob_fetch_counter: u64,
    /// Last `AppState::transcript_rev` processed by `sync`. When unchanged
    /// on the next notify, and no chat attach edge is pending, `sync` exits
    /// early — skipping the deep transcript clone and row diff.
    synced_rev: u64,
    _observe: Subscription,
}

/// Shell-facing events (the transcript itself hosts no surfaces).
#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    /// A spawn chip's "Open subagent" affordance: open the subagent's
    /// transcript as a right-pane tab. `chat_id` is the doc the chip lives
    /// in (the frozen blob is keyed `{chat_id}/{doc_id}`); `frozen` means
    /// the subagent finished — try the blob before watching the doc.
    OpenSubagent {
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
    },
}

impl gpui::EventEmitter<TranscriptEvent> for Transcript {}


/// A sent message's text with its file-mention chips. The same recipe as the
/// markdown renderer's inline code (`flat_text_element`): chip ranges shape in
/// the mono font at the spectrum's `code_text`, [`StyledText`] supplies wrapped glyph
/// geometry through its layout handle, and a canvas paints the rounded
/// `code_wash` *beneath* the glyphs — so chips wrap, clip, and scroll exactly
/// like the text they decorate.
///
/// Per-frame cost while an assistant message streams below: shaping hits
/// gpui's line-layout cache (identical text + runs ⇒ reuse) and the underlay
/// repaints O(chips) quads — no layout work, no re-projection (spans were
/// computed once in [`rows_for_entry`]).
/// The user bubble's text: runs split at mention-chip boundaries (one plain
/// run when there are none), with the same selection machinery as rendered
/// markdown — the element registers into the frame's document-ordered
/// registry, so drags select, span into adjacent rows, and Cmd+C copies.
fn user_bubble_text(
    row_id: &SharedString,
    text: SharedString,
    mentions: Arc<Vec<crate::composer::SentMentionSpan>>,
    theme: &Theme,
) -> AnyElement {
    // Split runs at chip boundaries (spans are in order): body text keeps the
    // sans font, chips read as inline code. Size/line-height flow from the
    // bubble's div like every text child.
    let body_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_sans.clone()),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let chip_run = |len: usize| TextRun {
        len,
        font: gpui::font(theme.font_mono.clone()),
        color: theme.code_text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::with_capacity(mentions.len() * 2 + 1);
    let mut at = 0;
    for span in mentions.iter() {
        if at < span.range.start {
            runs.push(body_run(span.range.start - at));
        }
        runs.push(chip_run(span.range.len()));
        at = span.range.end;
    }
    if at < text.len() {
        runs.push(body_run(text.len() - at));
    }
    let styled = StyledText::new(text.clone()).with_runs(runs);
    let layout = styled.layout().clone();
    let wash = theme.code_wash;
    let sel_key: std::sync::Arc<str> = format!("{row_id}:u").into();
    let sel_theme = theme.clone();
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            for span in mentions.iter() {
                for rect in render::range_rects(&layout, &span.range, 0.0, 2.0) {
                    window.paint_quad(quad(
                        rect,
                        px(5.0),
                        wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            render::paint_text_selection(window, &sel_key, &text, &layout, &sel_theme);
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(styled)
        .into_any_element()
}

/// The transcript ErrorChip — a port of zeron chat-view.tsx `ErrorChip`
/// (34px-minimum row, `rounded-[10px] border border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message at `text-foreground/80` — a subtle
/// red-tinted wash, never a bare red-stroke box. Unlike the web port, the
/// message WRAPS instead of truncating: startup-crash errors carry the
/// agent's exit status and stderr, and a one-line ellipsis was exactly what
/// made zeronsh/comet#95 undiagnosable from the screenshot.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .min_h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .py(px(7.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked — the
/// interactive controls live in the composer (chat-view.tsx `InputChip`):
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// A small glyph standing in for the tool's icon (zeron uses an icon set; a
/// quiet monochrome character keeps the tile without shipping SVGs).
/// The glyph for a tool call (zeron tool-chip.tsx `toolIcon`, Solar set).
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::COMMAND,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::DOCUMENT,
        ToolCall::WriteFile { .. } => crate::icons::DOCUMENT_ADD,
        ToolCall::EditFile { .. } => crate::icons::PEN,
        ToolCall::Search { .. } => crate::icons::MAGNIFER,
        ToolCall::Glob { .. } => crate::icons::FOLDER_WITH_FILES,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::GLOBAL,
        ToolCall::Todo { .. } => crate::icons::CHECKLIST,
        call if is_agent_call(call) => crate::icons::BOT,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::WIDGET,
    }
}

/// The body of an expanded chip card, under the header's separator. Diffs
/// render through the changes pane's section body — the real component, with
/// hunk headers, dual line-number gutters, accent bars, row washes, and
/// syntax runs — so an inline tool diff is indistinguishable from the
/// checkout diff sidebar. Output renders as a code block: verbatim mono
/// lines, indentation intact, counted-tail truncation.
fn detail_body(
    detail: &ToolDetail,
    diff_highlights: Option<Arc<crate::changes::DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let body = div().w_full().min_w_0().flex().flex_col().overflow_hidden();
    match detail {
        // No comment layer: an inline tool diff is a record of what the
        // agent already did, not a review surface.
        ToolDetail::Diff { file, .. } => body
            .child(crate::changes::render_file_body_with_syntax(
                file,
                diff_highlights,
                theme,
            ))
            .into_any_element(),
        ToolDetail::Stats { stats } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(stats.iter().map(|stat| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_color(theme.text.opacity(0.85))
                            .child(SharedString::from(stat.path.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.success)
                            .child(SharedString::from(format!("+{}", stat.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.danger)
                            .child(SharedString::from(format!("−{}", stat.deletions))),
                    )
            }))
            .into_any_element(),
        ToolDetail::Output {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .children(lines.iter().map(|line| {
                div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .text_color(theme.text.opacity(0.85))
                    .child(div().w_full().min_w_0().truncate().child(line.clone()))
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
        ToolDetail::Thought {
            lines,
            truncated_by,
        } => body
            .py(px(6.0))
            .text_size(px(12.0))
            .children(lines.iter().map(|line| {
                let row = div()
                    .h(px(OUTPUT_LINE_HEIGHT))
                    .w_full()
                    .min_w_0()
                    .px(px(12.0))
                    .flex()
                    .items_center();
                let Some((text, runs)) = thought_line_text(line, theme) else {
                    return row; // blank separator row
                };
                row.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .child(StyledText::new(text).with_runs(runs)),
                )
            }))
            .when(*truncated_by > 0, |block| {
                block.child(more_lines_row(*truncated_by, theme))
            })
            .into_any_element(),
    }
}

/// The counted-tail row under a truncated Output/Thought detail.
fn more_lines_row(truncated_by: usize, theme: &Theme) -> gpui::Div {
    div()
        .h(px(OUTPUT_LINE_HEIGHT))
        .px(px(12.0))
        .flex()
        .items_center()
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(format!("… {truncated_by} more lines")))
}

/// Shape one flattened thought line into gpui text runs — the detail-body
/// palette: muted foreground prose, semibold for bold, violet mono for code,
/// underlined links (NOT clickable — a thought is a record, not a surface).
fn thought_line_text(line: &[InlineRun], theme: &Theme) -> Option<(SharedString, Vec<TextRun>)> {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    for run in line {
        if run.text.is_empty() {
            continue;
        }
        let mut f = if run.style.code {
            gpui::font(theme.font_mono.clone())
        } else {
            gpui::font(theme.font_sans.clone())
        };
        if run.style.bold {
            f.weight = gpui::FontWeight::SEMIBOLD;
        }
        if run.style.italic {
            f.style = gpui::FontStyle::Italic;
        }
        runs.push(TextRun {
            len: run.text.len(),
            font: f,
            color: if run.style.code {
                render::inline_code_text(theme)
            } else {
                theme.text.opacity(0.85)
            },
            background_color: None,
            underline: run.style.link.is_some().then_some(gpui::UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
        text.push_str(&run.text);
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((text.into(), runs))
}

/// The trailing tile on a chip header, when it has one.
enum ChipTrail {
    /// Expand/collapse chevron — flipped while the detail body is open.
    Chevron { open: bool },
    /// Top-right "opens elsewhere" arrow — the spawn chip's link to its
    /// subagent tab.
    OpenArrow,
}

/// The chip's content row: icon tile + label + detail line (+ trailing tile
/// when the chip expands or links out). Shared between the plain chip, the
/// header of an expandable chip card, and the spawn link chip.
///
/// Spawn chips carry their subagent's lifecycle VISUALLY, in the chip's own
/// language: while running the mini working spinner (the sidebar's) pulses
/// at the right of the ordinary static detail; done is the ordinary quiet
/// chip; failed takes the danger tint — no status words, no live text (a
/// header rewriting itself per stream delta read as noise — user report).
fn chip_header_row(
    tool: &ToolItem,
    trail: Option<ChipTrail>,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    let (label, detail) = if tool.is_thought {
        ("Thought process", String::new())
    } else {
        tool_chip_content(&tool.call)
    };
    let running = tool.subagent_ref.is_some()
        && matches!(tool.subagent_status, Some(SubagentStatus::Running));
    let failed = tool.is_error
        || (tool.subagent_ref.is_some()
            && matches!(tool.subagent_status, Some(SubagentStatus::Failed)));
    let tint = if failed {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .h(px(CHIP_HEADER_HEIGHT))
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .text_size(px(12.0))
        .line_height(px(18.0))
        .child(
            // Icon tile (`size-[18px] rounded-[5px] bg-white/[0.08]`,
            // icon size-3).
            div()
                .size(px(18.0))
                .flex_none()
                .rounded(px(5.0))
                .bg(crate::theme::ink(0.08))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(if tool.is_thought {
                        crate::icons::CHAT_ROUND_LINE
                    } else {
                        tool_icon_path(&tool.call)
                    })
                    .size(px(12.0))
                    .text_color(theme.text_muted),
                ),
        )
        .child(
            div()
                .flex_none()
                .h(px(18.0))
                .flex()
                .items_center()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tint)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(18.0))
                .flex()
                .items_center()
                .truncate()
                .text_color(if failed {
                    theme.danger
                } else {
                    theme.text.opacity(0.85)
                })
                .child(SharedString::from(detail)),
        )
        .when_some(tool.call.subagent_model(), |row, model| {
            // Which model the child runs on, when the spawn named one.
            //
            // In the trailing slot rather than suffixed onto the detail: the
            // detail is the truncating slot, and the model is exactly what a
            // reader scanning a fan-out of spawns wants left once the
            // descriptions are cut.
            //
            // Bare faint text, NOT a filled pill: the tiles either side of it
            // are AFFORDANCES (the spinner means running, the arrow opens the
            // subagent), so giving a passive label the same chrome made the
            // trailing edge read as three buttons — the loudest thing in the
            // row was the one thing you cannot click.
            row.child(
                div()
                    .flex_none()
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(model.to_owned())),
            )
        })
        .when(running, |row| {
            // The sidebar working-row spinner, in the chip's trailing slot —
            // paint-local (fixed footprint), so it never moves the layout.
            row.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                format!(
                    "subagent-chip-{}",
                    tool.subagent_ref.as_deref().unwrap_or_default()
                ),
                2.0,
                theme.glyph,
                view,
                cx,
            )))
        })
        .when_some(trail, |row, trail| {
            // Trailing tile matching the group header's: a chevron for the
            // output/diff accordion, or the open-arrow for spawn chips.
            let tile = div()
                .size(px(18.0))
                .flex_none()
                .rounded(px(5.0))
                .bg(crate::theme::ink(0.06))
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted.opacity(0.8));
            row.child(match trail {
                ChipTrail::Chevron { open } => tile
                    .text_size(px(10.0))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
                ChipTrail::OpenArrow => tile.child(
                    crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.8)),
                ),
            })
        })
}

/// The header row of an expandable chip card.
fn chip_header(
    tool: &ToolItem,
    open: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> gpui::Div {
    chip_header_row(tool, Some(ChipTrail::Chevron { open }), theme, view, cx)
}

/// Max chars a subagent tab title keeps. The strip chip is fixed-width and
/// truncates visually, but the derived title also rides drag ghosts and any
/// future pickers — cap it at the source.
const SUBAGENT_TITLE_MAX: usize = 40;

/// First line of `text`, trimmed, capped at `max` chars with an ellipsis.
fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// Drop a leading "Agent"/"Task" genus (with its `:` and spacing) from a
/// spawn-title candidate. Only a real word boundary strips — "Taskmaster"
/// keeps its name. A bare "Agent"/"Task" strips to "" (no context at all).
fn strip_spawn_prefix(text: &str) -> &str {
    let t = text.trim();
    for prefix in ["agent", "task"] {
        if t.len() >= prefix.len()
            && t.is_char_boundary(prefix.len())
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &t[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    t
}

/// Tab title for a spawn chip's subagent surface: the BARE task description
/// ("verify the marker pipeline"). The chip keeps the tool's fuller name —
/// a fixed-width tab spent on "Agent: " never shows the task, so the genus
/// is stripped here and the call input's description/prompt fields back up
/// a bare name (older docs); "Subagent" only as the last resort.
fn subagent_tab_title(call: &ToolCall) -> SharedString {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Subagent".into(),
    };
    let candidates = [
        Some(name),
        input.and_then(|i| i.get("description")?.as_str()),
        input.and_then(|i| i.get("prompt")?.as_str()),
    ];
    for text in candidates.into_iter().flatten() {
        if let Some(title) = title_line(strip_spawn_prefix(text), SUBAGENT_TITLE_MAX) {
            return title.into();
        }
    }
    "Subagent".into()
}

/// A plain (non-expandable) chip: bordered card, plus the group guide rail
/// when the chip lives under a collapsible header.
fn tool_chip(
    tool: &ToolItem,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .when(rail, |el| el.ml(px(12.0)))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::hairline(0.07))
                .bg(crate::theme::ink(0.03))
                .child(chip_header_row(tool, None, theme, view, cx)),
        )
        .into_any_element()
}

/// A spawn chip: same card as [`tool_chip`], but the WHOLE card is the
/// "open the subagent tab" click (open-arrow tile in the trailing slot).
/// No accordion — an inline body would only repeat the subagent's own
/// transcript. The group guide rail is omitted for agent-only rows (no
/// collapse header for it to hang from).
fn subagent_chip(
    tool: &ToolItem,
    id: SharedString,
    on_open: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    rail: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when(rail, |row| {
            row.child(
                div()
                    .ml(px(12.0))
                    .h_full()
                    .w(px(1.0))
                    .flex_none()
                    .bg(crate::theme::ink(0.08)),
            )
        })
        .child(
            div()
                .id(id)
                .when(rail, |el| el.ml(px(12.0)))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::hairline(0.07))
                .bg(crate::theme::ink(0.03))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.05)))
                .on_click(on_open)
                .child(chip_header_row(
                    tool,
                    Some(ChipTrail::OpenArrow),
                    theme,
                    view,
                    cx,
                )),
        )
        .into_any_element()
}

fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error,
            resolved,
            subagent_ref,
            subagent_status,
            subagent_tail,
            ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
            // Subagent lifecycle mutates a COMPLETED entry in place (eager-
            // done: the spawn resolves while the subagent runs on) and
            // `byte_len` above doesn't cover these fields — hash them or the
            // cached rows never refresh on status/tail changes.
            acc.push(
                subagent_ref.is_some() as u8
                    | match subagent_status {
                        None => 0,
                        Some(SubagentStatus::Running) => 1 << 1,
                        Some(SubagentStatus::Done) => 2 << 1,
                        Some(SubagentStatus::Failed) => 3 << 1,
                    },
            );
            if let Some(tail) = subagent_tail {
                acc.extend_from_slice(tail.as_bytes());
            }
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
    }
    fnv1a(&acc)
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let typography_generation = crate::typography::generation(cx);
        if self.typography_generation != typography_generation {
            self.typography_generation = typography_generation;
            // `refresh_windows` re-lays out visible rows, but ListState keeps
            // measured heights for virtualized rows outside the viewport.
            // Mark every row unmeasured while retaining height hints and a
            // proportional scroll anchor; GPUI will refresh each measurement
            // as the row enters its layout range.
            self.list.remeasure();
        }
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::attachments::flush_evicted(Some(window), cx);
        // Own-turn driver: measurements are only authoritative after layout,
        // so reservation sizing, the send glide, and the outgrown-handoff
        // each advance at most once per requested frame. Scheduled on every
        // frame while an anchor is live (not just on kicks) so viewport
        // resizes and streaming growth re-derive the reservation; the step
        // only notifies on change, so a settled hold schedules no next frame.
        if (self.own_turn.is_some() || self.own_turn_kick) && !self.own_turn_scheduled {
            self.own_turn_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.own_turn_scheduled = false;
                        this.step_own_turn(cx);
                    })
                    .ok();
            });
        }
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        // Programmatic `scroll_to` does not invoke the list's user-scroll
        // handler. Refresh distance-derived state once layout has measured the
        // replay, guarded so a stale A callback cannot mutate B (or a newer A).
        if self.viewport_finalize_pending && !self.viewport_finalize_scheduled {
            self.viewport_finalize_scheduled = true;
            let token = ViewportFinalizeToken {
                generation: self.viewport_generation,
                layout_revision: self.viewport_layout_revision,
            };
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.viewport_finalize_scheduled = false;
                        if !token.still_current(this.viewport_generation) {
                            if this.viewport_finalize_pending {
                                cx.notify();
                            }
                            return;
                        }
                        let distance = this.distance_from_bottom();
                        this.last_scroll_distance = distance;
                        this.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX
                            && !this.pinned
                            && !this.own_turn.as_ref().is_some_and(|turn| turn.held);
                        if token.layout_settled(this.viewport_layout_revision) {
                            this.viewport_finalize_pending = false;
                        }
                        cx.notify();
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        let list_el = list(self.list.clone(), cx.processor(Self::render_row))
            .size_full()
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
        let content: AnyElement = if self.doc_override.is_some() {
            // The primary transcript's fade lives on the SHELL's outlet
            // wrapper (it spans the titlebar/composer chrome); an override
            // instance owns its own — top edge only (nothing overlays the
            // pane's bottom), gated on real overflow so a short top-anchored
            // transcript shows no fade. Gated here rather than at paint via
            // a ScrollHandle (the list isn't one); scrolls re-render this
            // entity, so the flag can't go stale.
            let scrolled_under_top = {
                let max = f32::from(self.list.max_offset_for_scrollbar().y);
                max - self.distance_from_bottom() > 1.0
            };
            crate::edge_fade::edge_faded(
                Theme::TRANSCRIPT_FADE_BAND,
                scrolled_under_top,
                false,
                list_el,
            )
            .into_any_element()
        } else {
            list_el.into_any_element()
        };
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::on_selection_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_selection_mouse_up))
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            .child(content)
            .child(rail);
        // Full-size viewer for a clicked user-bubble thumbnail
        // (AttachmentPreviewDialog: bare lightbox, click closes).
        if let Some(preview) = self.attachment_preview.clone() {
            let weak = cx.weak_entity();
            return root.child(crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                &self.attachment_preview_focus,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.attachment_preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::MessagePart;

    #[test]
    fn selection_scroll_ramps_at_viewport_edges() {
        let bounds = Bounds::new(
            gpui::point(px(10.0), px(20.0)),
            gpui::size(px(300.0), px(200.0)),
        );
        assert_eq!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(120.0))),
            0.0
        );
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(20.0))) < 0.0);
        assert!(selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0))) > 0.0);
        assert!(
            selection_scroll_step(bounds, gpui::point(px(20.0), px(220.0)))
                > selection_scroll_step(bounds, gpui::point(px(20.0), px(200.0)))
        );
    }

    // ---- streaming parse wiring (the transcript side, not the parser) ----

    #[test]
    fn live_row_parse_work_is_bounded_per_commit() {
        // Drive the EXACT wiring `rows_for` uses (`parse_for_row`) with the
        // prefix-extending commit snapshots the doc watch delivers, and prove
        // the per-commit parse work stays O(reparsed tail): a full-reparse
        // wiring would feed ~N/2 × final_len bytes through the parser across N
        // commits; the incremental path stays within a small multiple of the
        // final length regardless of N.
        let mut live_parsers = HashMap::new();
        let mut tree_cache = HashMap::new();
        let paragraph = "A paragraph of streaming prose that keeps arriving.\n\n";
        let commits = 120usize;
        let mut text = String::new();
        let mut total_parsed = 0usize;
        for i in 0..commits {
            // Each commit appends ~half a paragraph (crosses block boundaries).
            let chunk = &paragraph[..paragraph.len() / 2];
            text.push_str(if i % 2 == 0 {
                chunk
            } else {
                &paragraph[paragraph.len() / 2..]
            });
            let (tree, outcome) =
                parse_for_row(true, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
            assert!(!tree.blocks.is_empty());
            let ParseOutcome::Incremental {
                parsed_bytes,
                stable_prefix_blocks,
            } = outcome
            else {
                panic!("streaming commit must take the incremental path");
            };
            total_parsed += parsed_bytes;
            // Per commit: never a full reparse once the doc has grown past the
            // tail window (last two complete blocks + the partial trailing
            // one + the delta ≤ 3 paragraphs here).
            assert!(
                parsed_bytes <= 3 * paragraph.len(),
                "commit {i}: parsed {parsed_bytes} bytes — not bounded by the tail window"
            );
            // The stable prefix grows with the doc — settled blocks are never
            // re-touched (this is what keeps render caches valid).
            assert!(stable_prefix_blocks + 2 >= tree.blocks.len().saturating_sub(1));
        }
        // Across the whole stream: work is commits × O(tail), an order of
        // magnitude under the ~commits × len/2 a full-reparse wiring costs.
        let final_len = text.len();
        let full_reparse_cost = commits * final_len / 2;
        assert!(total_parsed <= commits * 3 * paragraph.len());
        assert!(
            total_parsed * 10 < full_reparse_cost,
            "total parsed {total_parsed} vs full-reparse ~{full_reparse_cost}"
        );

        // Live→complete handoff: the completed part adopts the live parser's
        // exact tree without parsing a single byte.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Handoff);
        // And the settled cache serves repeats with no work at all.
        let (_, outcome) = parse_for_row(false, "e1#p1", &text, &mut live_parsers, &mut tree_cache);
        assert_eq!(outcome, ParseOutcome::Cached);
    }

    // ---- stick-to-bottom spring ----

    #[test]
    fn spring_converges_to_a_fixed_target() {
        let mut spring = StickSpring::new();
        let target = 400.0;
        let mut pos = 0.0;
        let mut frames = 0;
        while pos < target && frames < 600 {
            pos = spring.step(pos, target, 1.0);
            frames += 1;
        }
        assert_eq!(pos, target, "spring must land exactly on the target");
        assert!(
            frames < 300,
            "400px should converge within 5s of frames, took {frames}"
        );
        // Once landed it stays landed (and idles out).
        for _ in 0..120 {
            pos = spring.step(pos, target, 1.0);
            assert_eq!(pos, target);
        }
        assert!(spring.is_idle(), "no residual motion at rest");
    }

    #[test]
    fn spring_never_overshoots_or_oscillates() {
        let mut spring = StickSpring::new();
        let target = 250.0;
        let mut pos = 0.0;
        let mut last = pos;
        for _ in 0..600 {
            pos = spring.step(pos, target, 1.0);
            assert!(pos <= target, "overshoot: {pos} > {target}");
            assert!(
                pos >= last - 1e-3,
                "oscillation: position moved backwards {last} -> {pos}"
            );
            last = pos;
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn spring_feed_forward_tracks_constant_growth() {
        // Target grows 2px/frame (≈120px/s — a typical stream). After warmup
        // the EMA feed-forward must carry the viewport at the same rate with a
        // bounded, stable lag — a glide, not 0,0,0,Npx steps.
        let growth = 2.0;
        let mut spring = StickSpring::new();
        let mut target = 600.0;
        let mut pos = 600.0;
        let mut deltas: Vec<f32> = Vec::new();
        for frame in 0..400 {
            target += growth;
            let next = spring.step(pos, target, 1.0);
            if frame >= 200 {
                deltas.push(next - pos);
            }
            pos = next;
        }
        // Steady state: per-frame movement ≈ growth rate…
        let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
        assert!(
            (mean - growth).abs() < 0.2,
            "steady-state speed {mean} should track growth {growth}"
        );
        // …with no stepping (every frame moves, none jumps).
        for d in &deltas {
            assert!(*d > 0.0, "viewport stalled mid-stream");
            assert!(*d < growth * 3.0, "viewport jumped: {d}px in one frame");
        }
        // The EMA growth estimate itself has locked on.
        assert!((spring.target_vel() - growth).abs() < 0.3);
        // Lag stays bounded by the chase lead.
        assert!(target - pos <= SPRING_CHASE_MAX_LEAD + growth);
    }

    #[test]
    fn spring_feed_forward_resets_when_target_shrinks() {
        let mut spring = StickSpring::new();
        let mut pos = 0.0;
        for i in 1..=50 {
            pos = spring.step(pos, 100.0 + i as f32 * 4.0, 1.0);
        }
        assert!(spring.target_vel() > 1.0);
        // A collapse (target shrinks by more than 1px) drops the estimate.
        spring.step(pos.min(120.0), 120.0, 1.0);
        assert_eq!(spring.target_vel(), 0.0);
    }

    #[test]
    fn spring_catchup_frames_glide_instead_of_teleporting() {
        // A 5-frame hitch advances roughly as far as 5 single steps would —
        // sub-stepped, still clamped at the target.
        let target = 300.0;
        let mut a = StickSpring::new();
        let mut pos_a = 0.0;
        for _ in 0..5 {
            pos_a = a.step(pos_a, target, 1.0);
        }
        let mut b = StickSpring::new();
        let pos_b = b.step(0.0, target, 5.0);
        assert!((pos_a - pos_b).abs() < 1.0, "{pos_a} vs {pos_b}");
        assert!(pos_b <= target);
    }

    #[test]
    fn restick_is_direction_aware() {
        // Scrolling away from the bottom never resticks, even inside the band
        // (a 20px wheel notch from the pinned bottom must break the pin).
        assert!(!Transcript::should_restick(20.0, 0.0));
        assert!(!Transcript::should_restick(69.0, 30.0));
        // Returning toward the bottom resticks once inside the 70px band…
        assert!(Transcript::should_restick(69.0, 120.0));
        assert!(Transcript::should_restick(0.0, 30.0));
        // …but not while still outside it.
        assert!(!Transcript::should_restick(200.0, 300.0));
        // No movement — leave the pin alone.
        assert!(!Transcript::should_restick(50.0, 50.0));
    }

    #[test]
    fn only_a_stream_at_the_bottom_gets_a_hard_end_anchor() {
        assert!(should_anchor_live_stream(true, 0.0, true));
        assert!(should_anchor_live_stream(true, AT_BOTTOM_PX, true));

        // A user who has moved away from the end keeps control of the
        // viewport, even if the transcript is still streaming.
        assert!(!should_anchor_live_stream(true, AT_BOTTOM_PX + 0.1, true));
        assert!(!should_anchor_live_stream(false, 0.0, true));

        // Ordinary transcript updates retain the existing spring behavior.
        assert!(!should_anchor_live_stream(true, 0.0, false));
    }

    #[test]
    fn own_turn_reservation_is_a_min_height_for_the_turn() {
        let usable = 700.0;
        // A short turn reserves the rest of the usable viewport below it.
        assert_eq!(own_turn_reservation(usable, 100.0), 600.0);
        // Growth consumes the reservation 1:1 — total held height is stable.
        assert_eq!(own_turn_reservation(usable, 450.0), 250.0);
        // At/past the fill line nothing is reserved (bottom spring takes
        // over with no height jump).
        assert_eq!(own_turn_reservation(usable, 700.0), 0.0);
        assert_eq!(own_turn_reservation(usable, 1_200.0), 0.0);
    }

    fn viewport_row(id: &str, entry_id: &str) -> Row {
        Row {
            id: id.into(),
            version: 0,
            turn_start: true,
            kind: RowKind::ErrorChip {
                message: SharedString::default(),
            },
            entry_id: entry_id.into(),
            timestamp: None,
            copy_text: None,
        }
    }

    #[test]
    fn viewport_anchor_tracks_a_stable_row_across_replay() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 1,
                offset_in_item: px(23.0),
            },
        )
        .expect("visible row");

        let replay = vec![
            viewport_row("new", "entry-new"),
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("c", "entry-c"),
        ];
        let restored = anchor.resolve(&replay).expect("restored row");
        assert_eq!(restored.item_ix, 2);
        assert_eq!(restored.offset_in_item, px(23.0));
    }

    #[test]
    fn viewport_anchor_has_entry_and_index_fallbacks() {
        let rows = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("old-block", "entry-c"),
        ];
        let anchor = ViewportAnchor::capture(
            &rows,
            ListOffset {
                item_ix: 2,
                offset_in_item: px(31.0),
            },
        )
        .expect("visible row");

        let reshaped = vec![
            viewport_row("a", "entry-a"),
            viewport_row("b", "entry-b"),
            viewport_row("inserted", "entry-new"),
            viewport_row("new-block", "entry-c"),
        ];
        let same_entry = anchor.resolve(&reshaped).expect("entry fallback");
        assert_eq!(same_entry.item_ix, 3);
        assert_eq!(same_entry.offset_in_item, px(0.0));

        let entry_removed = vec![viewport_row("a", "entry-a"), viewport_row("b", "entry-b")];
        let clamped = anchor.resolve(&entry_removed).expect("index fallback");
        assert_eq!(clamped.item_ix, 1);
        assert_eq!(clamped.offset_in_item, px(0.0));
    }

    #[test]
    fn optimistic_echo_cannot_consume_a_historical_viewport_before_replay() {
        let history = vec![viewport_row("historical", "historical-entry")];
        let saved = SavedViewport::capture(&history, ListOffset::default(), false, 480.0, None)
            .expect("historical viewport");
        let echo_only = vec![viewport_row("echo", "echo-entry")];

        assert!(
            saved.resolve(&echo_only, false).is_none(),
            "an unrelated echo is not an authoritative index fallback"
        );
        assert_eq!(
            saved
                .resolve(&echo_only, true)
                .expect("populated replay may use an index fallback")
                .offset
                .item_ix,
            0
        );
        assert!(TranscriptReplayState::Empty.authoritative_empty());
        assert!(!TranscriptReplayState::Empty.allows_fallback());
        assert!(!TranscriptReplayState::Pending.allows_fallback());
        assert!(TranscriptReplayState::Populated.allows_fallback());

        let echo_viewport =
            SavedViewport::capture(&echo_only, ListOffset::default(), false, 0.0, None)
                .expect("echo viewport");
        assert!(
            echo_viewport.resolve(&echo_only, false).is_some(),
            "the exact optimistic row is safe before replay"
        );
    }

    #[test]
    fn saved_viewport_preserves_and_releases_an_active_turn_runway() {
        let rows = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 640.0,
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &rows,
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.0),
            },
            false,
            0.0,
            Some(&own_turn),
        )
        .expect("active chat viewport");
        let SavedViewport::Anchored {
            own_turn: Some(saved_turn),
            ..
        } = &saved
        else {
            panic!("an active turn must keep its runway with the viewport");
        };
        assert_eq!(saved_turn.runway, 640.0);
        assert!(saved_turn.held);
        assert!(saved_turn.positioned);

        let restored = saved
            .resolve(&rows, false)
            .expect("exact queued echo survives an empty replay");
        let restored_turn = restored.own_turn.expect("valid restored runway");
        assert_eq!(restored_turn.runway, 640.0);
        assert!(!restored_turn.held);
        assert!(!restored_turn.positioned);
        assert!(restored_turn.seen_prompt);

        let list_state = ListState::new(rows.len(), ListAlignment::Bottom, px(0.0));
        list_state.reset(0);
        list_state.splice(0..0, rows.len());
        list_state.scroll_to(restored.offset);
        assert_eq!(list_state.logical_scroll_top().item_ix, 0);
        assert_eq!(list_state.logical_scroll_top().offset_in_item, px(0.0));

        assert!(
            SavedViewport::capture(&[], ListOffset::default(), false, 0.0, Some(&own_turn))
                .is_none(),
            "an empty rapid-switch replay must not overwrite the older snapshot"
        );
    }

    #[test]
    fn own_turn_waits_for_its_first_echo_then_retires_if_it_disappears() {
        let mut turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 0.0,
            held: true,
            positioned: false,
            seen_prompt: false,
        };

        assert!(turn.observe_prompt(false), "fresh send waits one state gap");
        assert!(turn.observe_prompt(true), "echo activates the runway");
        assert!(turn.seen_prompt);
        assert!(
            !turn.observe_prompt(false),
            "failed echo retires the activated runway"
        );
    }

    #[test]
    fn restored_viewport_discards_a_failed_optimistic_turn() {
        let outgoing = vec![viewport_row("prompt", "prompt")];
        let own_turn = OwnTurnAnchor {
            chat_id: "chat-a".into(),
            message_id: "prompt".into(),
            runway: 640.0,
            held: true,
            positioned: true,
            seen_prompt: true,
        };
        let saved = SavedViewport::capture(
            &outgoing,
            ListOffset::default(),
            false,
            420.0,
            Some(&own_turn),
        )
        .expect("outgoing viewport");

        // The failed echo vanished while A was hidden. The ordinary viewport
        // still restores by index, but no stale runway may intercept jump.
        let replay = vec![viewport_row("older", "older")];
        let restored = saved.resolve(&replay, true).expect("index fallback");
        assert!(restored.own_turn.is_none());
        assert_eq!(restored.offset.item_ix, 0);
        assert_eq!(restored.distance_from_bottom, 420.0);
    }

    #[test]
    fn pinned_viewports_follow_tail_and_the_cache_is_bounded() {
        let rows = vec![viewport_row("row", "entry")];
        let pinned = SavedViewport::capture(&rows, ListOffset::default(), true, 999.0, None)
            .expect("pinned viewport");
        assert!(matches!(pinned, SavedViewport::FollowTail));

        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS + 8 {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }
        assert_eq!(cache.len(), MAX_SAVED_VIEWPORTS);
        assert!(cache.get_cloned_and_touch("chat-0").is_none());
        assert!(
            cache
                .get_cloned_and_touch(&format!("chat-{}", MAX_SAVED_VIEWPORTS + 7))
                .is_some()
        );
    }

    #[test]
    fn reopening_the_oldest_cached_chat_protects_it_from_the_next_eviction() {
        let mut cache = SavedViewportCache::default();
        for ix in 0..MAX_SAVED_VIEWPORTS {
            cache.insert(format!("chat-{ix}"), SavedViewport::FollowTail);
        }

        assert!(cache.get_cloned_and_touch("chat-0").is_some());
        cache.insert("outgoing-new".into(), SavedViewport::FollowTail);

        assert!(cache.by_chat.contains_key("chat-0"));
        assert!(!cache.by_chat.contains_key("chat-1"));
        assert!(cache.by_chat.contains_key("outgoing-new"));
    }

    #[test]
    fn viewport_finalization_waits_for_current_generation_and_stable_layout() {
        let token = ViewportFinalizeToken {
            generation: 7,
            layout_revision: 11,
        };
        assert!(token.still_current(7));
        assert!(!token.still_current(8));
        assert!(token.layout_settled(11));
        assert!(!token.layout_settled(12));
    }

    fn parse(_: &str, text: &str) -> Arc<BlockTree> {
        Arc::new(parse_full(text))
    }

    fn assistant(id: &str, status: MessageStatus, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: Some(status),
            continuation_of: None,
        }
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn reasoning_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Reasoning {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn reasoning_joins_the_tool_group_accordion() {
        // Thought → tool → thought → tool folds into ONE group row (user
        // request: the thought process lives inside the combined accordion),
        // and the collapsed summary names the thinking.
        let entry = assistant(
            "a1",
            MessageStatus::Complete,
            vec![
                reasoning_part("r0", "planning the first step"),
                tool_part("t1", "ls"),
                reasoning_part("r2", "now the second step"),
                tool_part("t3", "pwd"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one combined accordion row");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(tools.len(), 4);
        assert!(tools[0].is_thought && tools[2].is_thought);
        assert!(!tools[1].is_thought && !tools[3].is_thought);
        // Thought chips carry their text as a styled-line detail with an
        // ANALYTIC height, so the group's fold tween covers them.
        assert!(matches!(
            tools[0].detail.as_deref(),
            Some(ToolDetail::Thought { lines, .. }) if !lines.is_empty()
        ));
        let summary = tool_group_summary(&tools);
        assert!(summary.starts_with("Thought 2 times"), "{summary}");
        assert!(summary.contains("2 commands"), "{summary}");

        // A lone thought is still an accordion (with the group tween), named
        // plainly.
        let entry = assistant(
            "a2",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "just thinking")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert_eq!(tool_group_summary(&tools), "Thought process");

        // Empty reasoning renders nothing.
        let entry = assistant(
            "a3",
            MessageStatus::Complete,
            vec![reasoning_part("r0", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }

    #[test]
    fn live_thought_streams_open_and_settles_closed() {
        let entry = assistant(
            "a1",
            MessageStatus::Streaming,
            vec![reasoning_part("r0", "thinking hard")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        // The live tail auto-opens the group; the chip itself is unresolved
        // (defaults open) until the part stops being the tail.
        assert!(*auto_open);
        assert!(!tools[0].resolved);

        let entry = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![
                reasoning_part("r0", "thinking hard"),
                text_part("t1", "answer"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("expected a tool group");
        };
        assert!(tools[0].resolved, "a followed thought is settled");
    }

    fn thought_of(text: &str) -> Vec<Vec<InlineRun>> {
        thought_lines(&parse_full(text))
    }

    fn line_chars(line: &[InlineRun]) -> usize {
        line.iter().map(|r| r.text.chars().count()).sum()
    }

    fn line_string(line: &[InlineRun]) -> String {
        line.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn thought_wrap_is_word_aware_and_bounded() {
        let lines = thought_of("one two three");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_string(&lines[0]), "one two three");
        let long = "word ".repeat(200);
        let lines = thought_of(&long);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        assert!(lines.len() > 5);
        let pathological = "x".repeat(300);
        let lines = thought_of(&pathological);
        assert!(lines.iter().all(|l| line_chars(l) <= THOUGHT_WRAP_COLS));
        // A word glued across style boundaries wraps as ONE unit — no line
        // may split inside `**bold**tail`.
        let glued = format!("{} **bold**tail", "word ".repeat(30));
        let lines = thought_of(&glued);
        let joined: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        assert!(joined.iter().any(|l| l.ends_with("boldtail")), "{joined:?}");
    }

    #[test]
    fn thought_markdown_styles_instead_of_literal_markers() {
        // The exact user report: `**bold**` markers showed as glyphs.
        let lines = thought_of("**Planning rollback** then *checking* `parse` [docs](https://d)");
        assert_eq!(lines.len(), 1);
        let flat = line_string(&lines[0]);
        assert!(
            !flat.contains('*') && !flat.contains('`') && !flat.contains('['),
            "{flat}"
        );
        let line = &lines[0];
        assert!(
            line.iter()
                .any(|r| r.style.bold && r.text.contains("Planning rollback")),
            "bold run survives: {line:?}"
        );
        assert!(
            line.iter()
                .any(|r| r.style.italic && r.text.contains("checking"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.code && r.text.contains("parse"))
        );
        assert!(
            line.iter()
                .any(|r| r.style.link.is_some() && r.text.contains("docs"))
        );
    }

    #[test]
    fn thought_blocks_flatten_structurally() {
        let lines = thought_of("# Head\n\npara\n\n- one\n- two\n\n```rust\nlet x = 1;\n```");
        let flat: Vec<String> = lines.iter().map(|l| line_string(l)).collect();
        // Heading renders bold, same size (one 18px row).
        assert!(
            lines[0]
                .iter()
                .any(|r| r.style.bold && r.text.contains("Head"))
        );
        // Blank separator rows between top-level blocks; tight list inside.
        assert_eq!(flat[1], "");
        assert_eq!(flat[2], "para");
        assert_eq!(flat[4], "• one");
        assert_eq!(flat[5], "• two");
        // Code lines verbatim, styled as code (mono at render).
        assert!(
            lines
                .last()
                .unwrap()
                .iter()
                .any(|r| r.style.code && r.text == "let x = 1;"),
            "{flat:?}"
        );
    }

    fn tool_part(id: &str, command: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Exec {
                command: command.into(),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    const MD: &str = "# Title\n\npara one\n\n```rust\nlet x = 1;\n```";

    #[test]
    fn live_entry_splits_per_block_with_id_continuity() {
        // Live rows split per block exactly like completed ones (the list
        // virtualizes them — the fading tail is the only per-frame work).
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        assert_eq!(live_rows.len(), 3, "one live row per top-level block");
        assert!(
            live_rows
                .iter()
                .all(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
        );
        assert_eq!(live_rows[0].id.as_ref(), "m1#t0.0");
        assert_eq!(live_rows[2].id.as_ref(), "m1#t0.2");

        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        assert_eq!(done_rows.len(), 3, "three top-level blocks");
        // Every block row keeps its id across the flip — no flicker on handoff.
        for (live, done) in live_rows.iter().zip(&done_rows) {
            assert_eq!(live.id, done.id);
            // The flip changes the version even at identical text (the
            // streaming bit), forcing a splice.
            assert_ne!(live.version, done.version);
        }
        assert!(matches!(
            done_rows[0].kind,
            RowKind::Markdown { block_ix: 0, .. }
        ));
    }

    #[test]
    fn live_commit_changes_only_tail_row_versions() {
        // Streaming commit: appending to the last block leaves every settled
        // block row's (id, version) untouched — the diff splices only the tail.
        let t1 = "para one\n\npara two\n\npara three";
        let t2 = "para one\n\npara two\n\npara three grows here";
        let live1 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t1)]);
        let live2 = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", t2)]);
        let r1 = rows_for_entry(&live1, false, &mut parse);
        let r2 = rows_for_entry(&live2, false, &mut parse);
        assert_eq!(r1.len(), 3);
        assert_eq!(r2.len(), 3);
        assert_eq!(r1[0].version, r2[0].version, "settled block untouched");
        assert_eq!(r1[1].version, r2[1].version, "settled block untouched");
        assert_ne!(r1[2].version, r2[2].version, "tail block respliced");
        assert_eq!(diff_rows(&r1, &r2), Some((2..3, 1)));
    }

    #[test]
    fn split_sibling_gaps_match_live_internal_spacing() {
        // The live row spaces its internal blocks by MD_BLOCK_GAP; after the
        // live→split handoff the same boundaries are inter-row gaps. They must
        // be identical or the whole message jumps at completion.
        let done = assistant(
            "m1",
            MessageStatus::Complete,
            vec![
                text_part("t0", MD),
                tool_part("a", "ls"),
                text_part("t1", "tail para"),
            ],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        // Rows: t0.0, t0.1, t0.2 (three MD blocks), g0, t1.0.
        assert_eq!(rows.len(), 5);
        // Sibling markdown blocks from the same part: md block gap.
        assert_eq!(top_gap_for(Some(&rows[0]), &rows[1]), render::MD_BLOCK_GAP);
        assert_eq!(top_gap_for(Some(&rows[1]), &rows[2]), render::MD_BLOCK_GAP);
        // Markdown → tool group and tool group → next part: larger boundary.
        assert_eq!(top_gap_for(Some(&rows[2]), &rows[3]), Theme::SPACE_MD);
        assert_eq!(top_gap_for(Some(&rows[3]), &rows[4]), Theme::SPACE_MD);
        // Turn starts get the turn gap regardless.
        assert_eq!(top_gap_for(None, &rows[0]), Theme::SPACE_LG);
    }

    #[test]
    fn consecutive_tools_fold_into_groups_between_text() {
        let entry = assistant(
            "m2",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                text_part("t1", "after"),
                tool_part("c", "make"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["m2#t0.0", "m2#g0", "m2#t1.0", "m2#g1"]);
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!("group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(rows[0].turn_start && !rows[1].turn_start);
    }

    fn agent_part(id: &str, description: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: format!("Agent: {description}"),
                input: Some(serde_json::json!({ "description": description })),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: Some(format!("chat--sub--{id}")),
            subagent_status: Some(SubagentStatus::Running),
            subagent_tail: None,
        }
    }

    #[test]
    fn agent_calls_split_out_of_ordinary_tool_groups() {
        // Agent/spawn chips must not share a collapse with Reads/Runs: a
        // lone Agent used to hide behind "Called 1 tool", and a mixed
        // group hid the running subagent until the user opened the fold.
        let entry = assistant(
            "m-agent",
            MessageStatus::Complete,
            vec![
                text_part("t0", "before"),
                tool_part("a", "ls"),
                tool_part("b", "pwd"),
                agent_part("s1", "Map URL import ingest path"),
                tool_part("c", "make"),
                agent_part("s2", "Audit the fold path"),
                agent_part("s3", "Verify the commit cadence"),
                text_part("t1", "after"),
            ],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(
            ids,
            [
                "m-agent#t0.0",
                "m-agent#g0",
                "m-agent#g1",
                "m-agent#g2",
                "m-agent#g3",
                "m-agent#t1.0",
            ]
        );

        let RowKind::ToolGroup { tools, auto_open } = &rows[1].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 2);
        assert!(tool_group_collapses(tools));
        assert!(!*auto_open);

        let RowKind::ToolGroup { tools, .. } = &rows[2].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_tool(&tools[0]));

        let RowKind::ToolGroup { tools, .. } = &rows[3].kind else {
            panic!("ordinary group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(tool_group_collapses(tools));

        let RowKind::ToolGroup { tools, .. } = &rows[4].kind else {
            panic!("consecutive agents share a group")
        };
        assert_eq!(tools.len(), 2);
        assert!(!tool_group_collapses(tools));
        assert!(tools.iter().all(is_agent_tool));
    }

    #[test]
    fn stray_subagent_ref_on_a_run_chip_stays_an_ordinary_tool() {
        // Docs written before the claude-driver fix carry subagent refs on
        // ordinary Run chips (a background shell's task_notification was
        // mis-tagged as subagent traffic). The ref alone must not change the
        // chip's genus: it folds with its neighbors and renders as a plain
        // tool, never as a spawn link to a doc that was never created.
        let mut stray = tool_part("b", "git clone …");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut stray
        {
            *subagent_ref = Some("chat--sub--b".into());
            *subagent_status = Some(SubagentStatus::Done);
        }
        let entry = assistant(
            "m-stray",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), stray, tool_part("c", "make")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1, "one folded group, no agent split");
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!("tool group expected")
        };
        assert_eq!(tools.len(), 3);
        assert!(tool_group_collapses(tools));
        assert!(tools.iter().all(|t| !is_agent_tool(t)));
        assert!(tools.iter().all(|t| !is_spawn_link(t)));
    }

    #[test]
    fn lone_completed_agent_stays_uncollapsed() {
        let entry = assistant(
            "m-lone",
            MessageStatus::Complete,
            vec![agent_part("s1", "scan repo")],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::ToolGroup { tools, auto_open } = &rows[0].kind else {
            panic!("agent group expected")
        };
        assert_eq!(tools.len(), 1);
        assert!(!tool_group_collapses(tools), "no 'Called 1 tool' wrap");
        assert!(
            !*auto_open,
            "auto_open is a streaming flag; agent rows ignore it at paint"
        );
    }

    #[test]
    fn pre_spawn_agent_name_is_enough_to_split() {
        // Before the engine stamps subagent_ref the chip is already named
        // "Agent: …" — that genus must split, or the spawn hides until the
        // first tagged event.
        let mut part = agent_part("s1", "scan repo");
        if let MessagePart::Tool {
            subagent_ref,
            subagent_status,
            ..
        } = &mut part
        {
            *subagent_ref = None;
            *subagent_status = None;
        }
        let entry = assistant(
            "m-pre",
            MessageStatus::Complete,
            vec![tool_part("a", "ls"), part],
        );
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 2);
        let RowKind::ToolGroup { tools, .. } = &rows[0].kind else {
            panic!()
        };
        assert!(tool_group_collapses(tools));
        let RowKind::ToolGroup { tools, .. } = &rows[1].kind else {
            panic!()
        };
        assert!(!tool_group_collapses(tools));
        assert!(is_agent_call(&tools[0].call));
    }

    #[test]
    fn trailing_group_auto_opens_only_while_streaming() {
        let parts = vec![text_part("t0", "hi"), tool_part("a", "ls")];
        let streaming = assistant("m3", MessageStatus::Streaming, parts.clone());
        let rows = rows_for_entry(&streaming, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(auto_open, "trailing group opens while streaming");

        let complete = assistant("m3", MessageStatus::Complete, parts);
        let rows = rows_for_entry(&complete, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[1].kind else {
            panic!()
        };
        assert!(!auto_open);

        // A non-trailing group never auto-opens.
        let mid = assistant(
            "m4",
            MessageStatus::Streaming,
            vec![tool_part("a", "ls"), text_part("t0", "hi")],
        );
        let rows = rows_for_entry(&mid, false, &mut parse);
        let RowKind::ToolGroup { auto_open, .. } = rows[0].kind else {
            panic!()
        };
        assert!(!auto_open);
    }

    #[test]
    fn user_rows_and_echo_versions() {
        let mut entry = assistant("u1", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", "hello")];
        let confirmed = rows_for_entry(&entry, false, &mut parse);
        let echoed = rows_for_entry(&entry, true, &mut parse);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].id, echoed[0].id);
        // Pending → confirmed changes the version so the row re-renders.
        assert_ne!(confirmed[0].version, echoed[0].version);
        assert!(matches!(
            &echoed[0].kind,
            RowKind::User { pending: true, .. }
        ));
    }

    #[test]
    fn user_rows_split_attachment_refs_from_text() {
        let content = crate::attachments::with_attachments(
            "what color is this?",
            &["/data/uploads/ab12-red.png".to_string()],
        );
        let mut entry = assistant("u2", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", &content)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        assert_eq!(rows.len(), 1);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "what color is this?");
        assert_eq!(rows[0].copy_text.as_deref(), Some("what color is this?"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, "/data/uploads/ab12-red.png");
        assert_eq!(attachments[0].name, "ab12-red.png");

        // Image-only send: no bubble text, refs parsed.
        let only = crate::attachments::with_attachments("", &["/a/p.png".to_string()]);
        entry.parts = vec![text_part("t0", &only)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User {
            text, attachments, ..
        } = &rows[0].kind
        else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "");
        assert!(rows[0].copy_text.is_none());
        assert_eq!(attachments.len(), 1);
    }

    /// A sent prompt's file mentions render as chips in the transcript: the
    /// row carries the projected display text plus spans, while ordinary
    /// prompts keep the empty-spans fast path. The row version derives from
    /// the RAW text either way, so projection never perturbs the diff key.
    #[test]
    fn user_rows_project_file_mentions_into_chips() {
        let raw = "look at [composer.rs](zeron-file:crates/ui/src/composer.rs) please";
        let mut entry = assistant("u3", MessageStatus::Complete, vec![]);
        entry.role = MessageRole::User;
        entry.status = None;
        entry.parts = vec![text_part("t0", raw)];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert!(
            !text.contains("zeron-file:"),
            "raw link left visible: {text}"
        );
        assert!(text.contains("composer.rs"));
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].is_dir);
        assert_eq!(mentions[0].path.as_ref(), "crates/ui/src/composer.rs");
        assert_eq!(&text[mentions[0].range.clone()], {
            let projected: &str = "\u{00A0}@composer.rs\u{00A0}";
            projected
        });
        assert_eq!(rows[0].version, (raw.len() as u64) << 1);

        entry.parts = vec![text_part("t0", "no mentions here")];
        let rows = rows_for_entry(&entry, false, &mut parse);
        let RowKind::User { text, mentions, .. } = &rows[0].kind else {
            panic!("expected a user row");
        };
        assert_eq!(text.as_ref(), "no mentions here");
        assert!(mentions.is_empty());
    }

    #[test]
    fn diff_rows_appends_and_middle_edits() {
        let entry1 = assistant("m1", MessageStatus::Complete, vec![text_part("t0", "one")]);
        let entry2 = assistant("m2", MessageStatus::Complete, vec![text_part("t0", "two")]);
        let r1 = rows_for_entry(&entry1, false, &mut parse);
        let mut both = r1.clone();
        both.extend(rows_for_entry(&entry2, false, &mut parse));

        // Identical → None.
        assert!(diff_rows(&r1, &r1.clone()).is_none());
        // Append → splice at the tail.
        assert_eq!(diff_rows(&r1, &both), Some((1..1, 1)));
        // Removal from the end.
        assert_eq!(diff_rows(&both, &r1), Some((1..2, 0)));

        // Middle content change: only the changed row splices.
        let entry1b = assistant(
            "m1",
            MessageStatus::Complete,
            vec![text_part("t0", "one more")],
        );
        let mut both_b = rows_for_entry(&entry1b, false, &mut parse);
        both_b.extend(rows_for_entry(&entry2, false, &mut parse));
        assert_eq!(diff_rows(&both, &both_b), Some((0..1, 1)));

        // Full reset when everything shifts.
        let r2 = rows_for_entry(&entry2, false, &mut parse);
        assert_eq!(diff_rows(&r1, &r2), Some((0..1, 1)));
    }

    #[test]
    fn diff_handles_live_to_split_growth() {
        let live = assistant("m1", MessageStatus::Streaming, vec![text_part("t0", MD)]);
        let done = assistant("m1", MessageStatus::Complete, vec![text_part("t0", MD)]);
        let live_rows = rows_for_entry(&live, false, &mut parse);
        let done_rows = rows_for_entry(&done, false, &mut parse);
        // Same ids; every version flips its streaming bit → one 3-row splice.
        assert_eq!(diff_rows(&live_rows, &done_rows), Some((0..3, 3)));
    }

    #[test]
    fn tool_diff_builds_real_hunks_with_context_and_numbers() {
        use crate::changes::LineKind;
        let old = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let mut new = old.clone();
        new[9] = "LINE 10".into();
        let diff = zeron_proto::ToolDiff {
            path: "/w/a.rs".into(),
            old_text: Some(old.join("\n") + "\n"),
            new_text: new.join("\n") + "\n",
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&diff), None)
        else {
            panic!("expected diff detail");
        };
        // One hunk: the change plus 3 context lines each side, real numbers.
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -7,7 +7,7 @@");
        assert_eq!(hunk.lines.len(), 8); // 6 context + 1 del + 1 add
        let del = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Del)
            .expect("del line");
        assert_eq!(del.old_no, Some(10));
        assert_eq!(del.new_no, None);
        assert_eq!(del.text, "line 10");
        let add = hunk
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Add)
            .expect("add line");
        assert_eq!(add.new_no, Some(10));
        assert_eq!(add.text, "LINE 10");
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert_eq!(old_text.as_deref(), diff.old_text.as_deref());
        assert_eq!(new_text.as_deref(), Some(diff.new_text.as_str()));
        // New files carry Added status (and no old numbers).
        let created = zeron_proto::ToolDiff {
            path: "/w/new.txt".into(),
            old_text: None,
            new_text: "only\n".into(),
        };
        let Some(ToolDetail::Diff {
            file,
            old_text,
            new_text,
        }) = tool_detail(None, Some(&created), None)
        else {
            panic!("expected diff detail");
        };
        assert_eq!(file.status, crate::changes::FileStatus::Added);
        assert!(old_text.is_none());
        assert_eq!(new_text.as_deref(), Some("only\n"));

        // Output: verbatim lines (indentation intact), counted-tail cap.
        let output = (0..40)
            .map(|i| format!("    indented {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = tool_detail(Some(&output), None, None)
        else {
            panic!("expected output detail");
        };
        assert_eq!(lines.len(), OUTPUT_DETAIL_MAX_LINES);
        assert_eq!(truncated_by, 40 - OUTPUT_DETAIL_MAX_LINES);
        assert_eq!(lines[0].as_ref(), "    indented 0");

        // Nothing → no affordance.
        assert!(tool_detail(None, None, None).is_none());
        assert!(tool_detail(Some("\n\n"), None, None).is_none());
    }

    #[test]
    fn tool_group_summaries() {
        let exec = |c: &str| ToolItem {
            call: ToolCall::Exec { command: c.into() },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
        };
        let edit = |p: &str| ToolItem {
            call: ToolCall::EditFile {
                path: p.into(),
                old_string: None,
                new_string: None,
            },
            is_error: false,
            resolved: true,
            detail: None,
            invocation: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
            is_thought: false,
        };
        let tools = vec![
            exec("ls"),
            exec("pwd"),
            exec("make"),
            edit("a.rs"),
            edit("b.rs"),
        ];
        assert_eq!(
            tool_group_summary(&tools),
            "Ran 3 commands · edited 2 files"
        );
        // Distinct-path dedupe: editing one file twice counts once.
        let tools = vec![edit("a.rs"), edit("a.rs")];
        assert_eq!(tool_group_summary(&tools), "Edited 1 file");
        // Failures append.
        let mut failing = exec("boom");
        failing.is_error = true;
        assert_eq!(tool_group_summary(&[failing]), "Ran 1 command · 1 failed");
        // Reads / searches / misc.
        let tools = vec![
            ToolItem {
                call: ToolCall::ReadFile { path: "x".into() },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
            ToolItem {
                call: ToolCall::Glob {
                    pattern: "*.rs".into(),
                },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
            ToolItem {
                call: ToolCall::WebSearch { query: "q".into() },
                is_error: false,
                resolved: true,
                detail: None,
                invocation: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
                is_thought: false,
            },
        ];
        assert_eq!(tool_group_summary(&tools), "Read 1 file · searched 2 times");
    }

    #[test]
    fn subagent_tab_titles() {
        // The tab is the BARE task — the "Agent:" genus is stripped.
        let named = ToolCall::Unknown {
            name: "Agent: scan repo".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&named).as_ref(), "scan repo");
        // A bare "Task"/"Agent" digs the description out of the call input
        // (which sheds any genus of its own).
        let bare = ToolCall::Unknown {
            name: "Task".into(),
            input: Some(serde_json::json!({
                "description": "Agent: audit the auth flow",
                "prompt": "very long instructions…",
            })),
        };
        assert_eq!(subagent_tab_title(&bare).as_ref(), "audit the auth flow");
        // Word boundaries only — a name that merely STARTS with the genus
        // keeps itself.
        let compound = ToolCall::Unknown {
            name: "Taskmaster".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&compound).as_ref(), "Taskmaster");
        // Nothing to derive → the generic label.
        let blank = ToolCall::Unknown {
            name: "agent".into(),
            input: None,
        };
        assert_eq!(subagent_tab_title(&blank).as_ref(), "Subagent");
        // Absurd lengths cap with an ellipsis; multiline prompts keep only
        // their first line.
        let long = ToolCall::Unknown {
            name: "x".repeat(120),
            input: None,
        };
        let title = subagent_tab_title(&long);
        assert_eq!(title.chars().count(), SUBAGENT_TITLE_MAX + 1);
        assert!(title.ends_with('…'));
        // Non-spawn-shaped calls stay generic.
        assert_eq!(
            subagent_tab_title(&ToolCall::Exec {
                command: "ls".into()
            })
            .as_ref(),
            "Subagent"
        );
    }

    #[test]
    fn tool_chip_labels_per_kind() {
        assert_eq!(
            tool_chip_content(&ToolCall::Exec {
                command: "cargo test".into()
            }),
            ("Run", "cargo test".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }),
            ("Search", "foo in src".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::ApplyPatch { path: None }),
            ("Patch", "workspace".to_string())
        );
        assert_eq!(
            tool_chip_content(&ToolCall::Mcp {
                server: "gh".into(),
                tool: "issues".into(),
                input: None
            }),
            ("MCP", "gh · issues".to_string())
        );
        let todo = ToolCall::Todo {
            items: vec![
                zeron_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                zeron_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        };
        assert_eq!(tool_chip_content(&todo), ("Todo", "1/2 done".to_string()));
    }

    #[test]
    fn multiline_command_flattens_to_one_chip_line() {
        // The user's breaker: a multi-line script in a Run chip. The detail
        // must come out as ONE sanitized line — the chip's fixed 30px card
        // then truncates it with an ellipsis like the original's CSS.
        let (label, detail) = tool_chip_content(&ToolCall::Exec {
            command: "set -e\nfixture_in_original=0\n\tgrep -c  \"x\"".into(),
        });
        assert_eq!(label, "Run");
        assert_eq!(detail, "set -e fixture_in_original=0 grep -c \"x\"");
        assert!(!detail.contains('\n'));
        // The chip row height is a constant, independent of content shape.
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        // Every detail kind is sanitized (MCP inputs / queries are model text).
        let (_, q) = tool_chip_content(&ToolCall::WebSearch {
            query: "line one\nline two".into(),
        });
        assert_eq!(q, "line one line two");
    }

    #[test]
    fn call_block_carries_the_full_invocation() {
        // Multi-line command: verbatim lines, not the flattened chip line.
        let Some(ToolDetail::Output {
            lines,
            truncated_by,
        }) = call_block(&ToolCall::Exec {
            command: "set -e\ncargo test".into(),
        })
        else {
            panic!("expected an output block")
        };
        assert_eq!(truncated_by, 0);
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["set -e", "cargo test"]
        );

        // A long single-line command soft-wraps instead of ellipsizing.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Exec {
            command: "x".repeat(CALL_WRAP_COLS * 2 + 10),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.chars().count() <= CALL_WRAP_COLS));

        // MCP input pretty-prints under the `server · tool` line.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Mcp {
            server: "gh".into(),
            tool: "issues".into(),
            input: Some(serde_json::json!({"repo": "zeron"})),
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(lines[0].as_ref(), "gh · issues");
        assert!(lines.iter().any(|l| l.contains("\"repo\": \"zeron\"")));

        // Todos list one item per line with checkbox state.
        let Some(ToolDetail::Output { lines, .. }) = call_block(&ToolCall::Todo {
            items: vec![
                zeron_proto::TodoItem {
                    text: "a".into(),
                    done: true,
                },
                zeron_proto::TodoItem {
                    text: "b".into(),
                    done: false,
                },
            ],
        }) else {
            panic!("expected an output block")
        };
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["[x] a", "[ ] b"]
        );

        // Blank invocation → no block; the chip stays a plain card.
        assert!(
            call_block(&ToolCall::Exec {
                command: "  \n ".into()
            })
            .is_none()
        );
    }

    #[test]
    fn timestamp_strip_lands_on_the_last_settled_row() {
        use chrono::FixedOffset;
        // Fixed zone (UTC−4): "Jul 1, 3:45 PM" — the exact formatTimestamp
        // shape (short month, numeric day, no leading zero, 2-digit minutes).
        let tz = FixedOffset::west_opt(4 * 3600).unwrap();
        let ms = chrono::DateTime::parse_from_rfc3339("2026-07-01T19:45:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(format_timestamp(ms, &tz), "Jul 1, 3:45 PM");

        // User entries carry the strip on their single row (pending too).
        let user = SessionMessageEntry {
            id: "u1".into(),
            role: MessageRole::User,
            parts: vec![text_part("p1", "hi")],
            created_at: ms,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        };
        let rows = rows_for_entry(&user, true, &mut parse);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp, Some(ms));

        // Assistant entries: strip on the LAST row once settled…
        let done = assistant(
            "a1",
            MessageStatus::Complete,
            vec![text_part("p1", "one\n\ntwo")],
        );
        let rows = rows_for_entry(&done, false, &mut parse);
        assert!(rows.len() >= 2);
        assert_eq!(rows.last().unwrap().timestamp, Some(done.created_at));
        assert_eq!(
            rows.last().unwrap().copy_text.as_deref(),
            Some("one\n\ntwo")
        );
        assert!(rows[..rows.len() - 1].iter().all(|r| r.timestamp.is_none()));
        assert!(rows[..rows.len() - 1].iter().all(|r| r.copy_text.is_none()));

        // …but never mid-stream (chat-view.tsx: no hover under a moving reply).
        let live = assistant(
            "a2",
            MessageStatus::Streaming,
            vec![text_part("p1", "streaming…")],
        );
        let rows = rows_for_entry(&live, false, &mut parse);
        assert!(rows.iter().all(|r| r.timestamp.is_none()));
        assert!(rows.iter().all(|r| r.copy_text.is_none()));
        // Every row knows its entry (the hover group).
        assert!(rows.iter().all(|r| r.entry_id.as_ref() == live.id));
    }

    #[test]
    fn message_copy_keeps_authored_text_and_excludes_tool_traces() {
        let entry = assistant(
            "a-copy",
            MessageStatus::Complete,
            vec![
                text_part("p1", "First **paragraph**."),
                tool_part("tool", "printf hidden"),
                text_part("p2", "    indented code\n    stays indented"),
            ],
        );
        assert_eq!(
            assistant_copy_text(&entry).as_deref(),
            Some("First **paragraph**.\n\n    indented code\n    stays indented")
        );
    }

    #[test]
    fn single_line_collapses_all_whitespace_runs() {
        assert_eq!(single_line("a\nb"), "a b");
        assert_eq!(single_line("  a\t\t b \r\n c  "), "a b c");
        assert_eq!(single_line("plain"), "plain");
        assert_eq!(single_line(""), "");
        assert_eq!(single_line("\n\n"), "");
    }

    #[test]
    fn chips_height_is_analytic() {
        assert_eq!(chips_height(0), 0.0);
        assert_eq!(chips_height(1), CHIPS_TOP_PAD + CHIP_HEIGHT);
        assert_eq!(
            chips_height(3),
            CHIPS_TOP_PAD + 3.0 * CHIP_HEIGHT + 2.0 * CHIP_GAP
        );
    }

    #[test]
    fn flavour_words_rotate_every_seven_seconds() {
        let seed = flavour_seed("chat-1");
        assert_eq!(flavour_word(seed, 0), flavour_word(seed, 6));
        assert_ne!(flavour_word(seed, 0), flavour_word(seed, 7));
        // Deterministic per chat; different chats usually differ in phase.
        assert_eq!(flavour_word(seed, 3), flavour_word(seed, 3));
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(92), "1m 32s");
        assert_eq!(format_elapsed(-5), "0s");
    }

    #[test]
    fn sending_bridge_holds_until_the_turn_outdates_the_send() {
        let send = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .unwrap()
            .to_utc();
        let before = send - chrono::Duration::seconds(90);
        let after = send + chrono::Duration::seconds(2);
        // In flight, row still on the previous turn (or no row yet).
        assert!(sending_bridge(Some(send), Some(before)));
        assert!(sending_bridge(Some(send), None));
        // The turn started after the send fired — timer takes over.
        assert!(!sending_bridge(Some(send), Some(after)));
        // No send in flight: never a bridge, whatever the row says.
        assert!(!sending_bridge(None, Some(before)));
        assert!(!sending_bridge(None, None));
    }

    #[test]
    fn empty_text_parts_produce_no_rows() {
        let entry = assistant(
            "m9",
            MessageStatus::Streaming,
            vec![text_part("t0", ""), text_part("t1", "   ")],
        );
        assert!(rows_for_entry(&entry, false, &mut parse).is_empty());
    }
}
