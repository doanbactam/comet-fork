//! The composer: a hand-rolled multiline text input (adapted from gpui's
//! `examples/input.rs`), the compact↔expanded flip, the Send/Steer/Stop morph,
//! optimistic send with failure recovery, per-chat drafts, and the question
//! wizard that replaces the composer while a run awaits input.
//!
//! Pure decision logic (flip, auto-grow math, button morph, wizard reducer,
//! pending-input detection) lives in free functions/structs with unit tests;
//! the gpui element only feeds them measurements.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyTooltip, App, BorderStyle, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle,
    DispatchPhase, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PaintQuad, PathPromptOptions, Pixels, Point,
    ScrollWheelEvent, SharedString, Style, StyledImage as _, Subscription, Task, TextRun,
    TextStyle, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, img, point,
    prelude::*, px, quad, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use std::sync::Arc;

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use zeron_proto::{
    FileSearchMatch, HarnessId, RunRequest, SandboxLevel, SlashCommand, UserInputAnswer,
    UserInputQuestion,
};
use zeron_rpc::{RpcError, methods};

use crate::attachments::{self, StagedAttachment};
use crate::motion;
use crate::pickers::Pickers;
use crate::state::{AppState, Indicator};
use crate::theme::Theme;

mod morph;
mod wizard;
mod input;
pub use morph::{
    collapse_text_glide, flip_morph_step, morph_cluster_dy, morph_cluster_inset, morph_text_pad,
    FlipMorph, ACTION_PRIMARY_GAP, ACTION_UTILITY_GAP, CLUSTER_X_DELTA, CLUSTER_Y_DELTA,
    ROUTE_SNAP_MS,
};
pub use wizard::{input_request_resolved, pending_input_request, Wizard, WizardStep};
pub use input::{sent_mention_display, ComposerEvent, ComposerInput, ComposerInputEvent, SentMentionSpan, init};
pub(crate) use input::FILE_MENTION_SCHEME;
use input::*;

// ---------------------------------------------------------------------------
// Constants + pure decision logic
// ---------------------------------------------------------------------------

/// Expanded-mode textarea vertical padding: `pt-4 pb-1` (zeron composer.tsx
/// line 578) = 16 + 4.
pub const TEXTAREA_PAD_V: f32 = 20.0;
/// The expanded textarea BOX (content + padding) is clamped by the original's
/// auto-grow effect: `ta.style.height = Math.min(Math.max(scrollHeight, 76),
/// 260)` (zeron composer.tsx line 235). The 76px floor applies even when
/// empty — it's what makes the always-expanded new-chat composer tall.
pub const TEXTAREA_MIN: f32 = 76.0;
pub const TEXTAREA_MAX: f32 = 260.0;
/// Expanded actions row: `pt-1` (4) + h-8 picker chips (32 — the tallest
/// children; composer/styles.tsx pickerChip) + `pb-2.5` (10) — zeron
/// composer-actions.tsx line 60.
pub const ACTIONS_ROW_HEIGHT: f32 = 46.0;
/// The pill's 1px hairline, top + bottom (`rounded-[26px] border`).
pub const PILL_BORDER_V: f32 = 2.0;
/// Expanded composer bounds, border-box: 76 + 46 + 2 = 124 when empty (the
/// new-chat canvas), 260 + 46 + 2 = 308 at the content cap.
pub const COMPOSER_MIN_HEIGHT: f32 = TEXTAREA_MIN + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
pub const COMPOSER_MAX_HEIGHT: f32 = TEXTAREA_MAX + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
/// Compact pill, border-box: one-line textarea `py-3` (24) + one 22.75px line
/// (scrollHeight rounds to 47 in the original) + the 2px hairline = 49. The
/// compact cluster (`py-1.5` + h-8 = 44) is shorter, so the textarea wins.
pub const COMPACT_TOTAL_HEIGHT: f32 = 49.0;
/// `max-w-3xl`: stable outer width of the centered composer column.
const COMPOSER_MAX_WIDTH: f32 = 768.0;
/// Ignore subpixel noise when the shell reports the conversation width.
const COMPOSER_WIDTH_EPSILON: f32 = 0.5;
/// Below this pill input width the composer always expands.
pub const MIN_COMPACT_INPUT_WIDTH: f32 = 200.0;
/// Input text metrics: `text-[14px] leading-relaxed` = 14 × 1.625 = 22.75.
pub const INPUT_LINE_HEIGHT: f32 = 22.75;
pub const INPUT_TEXT_SIZE: f32 = 14.0;
/// Single-select questions auto-advance after this long.
pub const AUTO_ADVANCE_MS: u64 = 220;
/// Drag-selection autoscroll runs at the display-friendly 60fps cadence.
pub const DRAG_SCROLL_FRAME_MS: u64 = 16;

/// Hysteresis slack for the expanded→compact flip: once expanded, the composer
/// only collapses when the text is comfortably narrower than the compact
/// capacity — expanding and collapsing share no boundary, so a width right at
/// the flip threshold can't oscillate between the two layouts.
pub const COLLAPSE_HYSTERESIS: f32 = 32.0;
/// During an interactive resize, collapsing back to the compact mode waits
/// until the measured widths have been stable this long. Expansion remains
/// immediate so a narrowing panel never traps the controls in a compact row.
pub const RESIZE_SETTLE_MS: u64 = 150;

/// Compact↔expanded flip with hysteresis. `capacity` is the *compact-mode*
/// input capacity (a layout-stable width: measured while compact, tracked by
/// container-width deltas while expanded — never the post-flip measured width,
/// which differs per mode and would feed back into the decision):
/// - a newline always expands;
/// - while `resizing`, an expanded composer stays expanded until sizes settle;
/// - a too-narrow pill (`capacity < MIN_COMPACT_INPUT_WIDTH`) always expands;
/// - compact expands only when `text_width > capacity`; expanded collapses
///   only when `text_width < capacity - COLLAPSE_HYSTERESIS`.
pub fn composer_flip(
    expanded: bool,
    text_width: f32,
    capacity: f32,
    has_newline: bool,
    resizing: bool,
) -> bool {
    if has_newline {
        return true;
    }
    if capacity < MIN_COMPACT_INPUT_WIDTH {
        return true;
    }
    if expanded {
        resizing || text_width >= capacity - COLLAPSE_HYSTERESIS
    } else {
        text_width > capacity
    }
}

fn composer_width_changed(previous: Option<f32>, current: f32) -> bool {
    previous.is_none_or(|previous| (current - previous).abs() > COMPOSER_WIDTH_EPSILON)
}

/// Caret blink half-period (standard textarea cadence: ~500ms on / 500ms off).
pub const CARET_BLINK_MS: u64 = 500;

/// Caret blink phase for a time since the last keystroke/caret move: solid
/// through the first half-period (typing bursts never blink — each keystroke
/// resets the phase), then alternating.
pub fn caret_visible(ms_since_activity: u64) -> bool {
    (ms_since_activity / CARET_BLINK_MS) % 2 == 0
}

/// Auto-grow: content height for a wrapped-line count.
pub fn input_content_height(wrapped_lines: usize) -> f32 {
    wrapped_lines.max(1) as f32 * INPUT_LINE_HEIGHT
}

/// Total expanded composer height (border-box) for a content height: the
/// textarea BOX (content + `pt-4 pb-1`) clamps to 76–260 exactly like the
/// original's auto-grow effect, then the 46px actions row and the hairline
/// ride on top. Range 124–308.
pub fn composer_total_height(content_height: f32) -> f32 {
    (content_height + TEXTAREA_PAD_V).clamp(TEXTAREA_MIN, TEXTAREA_MAX)
        + ACTIONS_ROW_HEIGHT
        + PILL_BORDER_V
}

fn input_max_scroll(content_height: f32, viewport_height: f32) -> f32 {
    (content_height - viewport_height).max(0.0)
}

/// Apply GPUI's wheel delta to a top-origin input offset. Positive deltas mean
/// scrolling toward the start, matching gpui's built-in list/div behavior.
fn input_scroll_offset(
    current: f32,
    delta_y: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    (current - delta_y).clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// Minimally adjust the viewport so the caret row is fully visible.
fn input_scroll_offset_for_cursor(
    current: f32,
    cursor_top: f32,
    cursor_height: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    let mut next = current;
    if cursor_top < next {
        next = cursor_top;
    } else if cursor_top + cursor_height > next + viewport_height {
        next = cursor_top + cursor_height - viewport_height;
    }
    next.clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// What a mouse press in a text field asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressIntent {
    /// Take the whole field.
    SelectAll,
    /// Grow the current selection to the pressed position.
    ExtendSelection,
    /// Put the caret at the pressed position.
    PlaceCaret,
}

impl PressIntent {
    /// Whether the press starts a drag selection. A select-all must not, or
    /// the next mouse move shrinks it back to a drag from the press position.
    fn arms_drag(self) -> bool {
        !matches!(self, Self::SelectAll)
    }
}

/// Read the intent from the press. Two clicks or more take the whole field,
/// and every further click keeps it, so holding the button through a third
/// click does not change what is selected.
fn press_intent(click_count: usize, shift: bool) -> PressIntent {
    if click_count >= 2 {
        PressIntent::SelectAll
    } else if shift {
        PressIntent::ExtendSelection
    } else {
        PressIntent::PlaceCaret
    }
}

/// Per-frame drag-selection scroll. Distance increases speed, capped at one
/// text row per frame so crossing the input boundary never causes a jump.
fn input_drag_scroll_delta(
    pointer_y: f32,
    viewport_top: f32,
    viewport_bottom: f32,
    line_height: f32,
) -> f32 {
    let distance = if pointer_y < viewport_top {
        pointer_y - viewport_top
    } else if pointer_y > viewport_bottom {
        pointer_y - viewport_bottom
    } else {
        return 0.0;
    };
    distance.signum() * (distance.abs() * 0.2).clamp(1.0, line_height)
}

/// Staged-attachment strip metrics (zeron attachment-ui.tsx AttachmentStrip:
/// `flex flex-wrap gap-2 px-4 pt-3`, `size-14` thumbs).
pub const STRIP_THUMB: f32 = 56.0;
pub const STRIP_GAP: f32 = 8.0;
pub const STRIP_PAD_TOP: f32 = 12.0;
pub const STRIP_PAD_X: f32 = 16.0;

/// Height the wrap strip adds to the pill for `count` staged thumbnails at an
/// `inner_width` pill content width (0 when empty). Mirrors flex-wrap: as many
/// 56px thumbs per row as fit with 8px gaps inside the 16px side insets.
pub fn attachment_strip_height(count: usize, inner_width: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let usable = (inner_width - 2.0 * STRIP_PAD_X).max(STRIP_THUMB);
    let per_row = (((usable + STRIP_GAP) / (STRIP_THUMB + STRIP_GAP)).floor() as usize).max(1);
    let rows = count.div_ceil(per_row);
    STRIP_PAD_TOP + rows as f32 * STRIP_THUMB + (rows - 1) as f32 * STRIP_GAP
}

pub fn comment_strip_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    STRIP_PAD_TOP + crate::badges::BADGE_HEIGHT
}

/// Engines at or above this version understand `pending://` attachment refs
/// and QueueCommand `transfers` (send-is-a-local-write attachments). Gated on
/// BOTH the local engine (an IPC daemon may be older than this UI) and, for
/// remotely-hosted chats, the host device's stamped registry version.
const QUEUED_ATTACHMENTS_MIN: (u64, u64, u64) = (0, 2, 12);

/// What the send button is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendButtonMode {
    /// No live run: plain send.
    Send,
    /// Live steerable run with text typed: "Send (steers the current run)".
    Steer,
    /// Live run, nothing typed: red stop square.
    Stop,
}

/// What the composer holds that a send could carry. A staged image or diff
/// comment counts: both synthesize their own prompt body, so either alone is
/// a legal send — and during a live run has to read as Steer, not Stop.
pub fn composer_has_content(text: &str, attachments: usize, comments: usize) -> bool {
    !text.trim().is_empty() || attachments > 0 || comments > 0
}

pub fn send_button_mode(run_live: bool, has_text: bool) -> SendButtonMode {
    match (run_live, has_text) {
        (false, _) => SendButtonMode::Send,
        (true, true) => SendButtonMode::Steer,
        (true, false) => SendButtonMode::Stop,
    }
}


// ---------------------------------------------------------------------------
// Multiline text input (adapted from gpui examples/input.rs)
// ---------------------------------------------------------------------------

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        Copy,
        Cut,
        Paste,
        Newline,
        Submit,
        Undo,
        Redo,
        MentionTab,
        MentionEscape,
    ]
);



/// The literal `@` a chip displays before its file name. Projected as TEXT so
/// it shapes, wraps, and hit-tests with the label — the earlier SVG icons
/// painted into a reserved whitespace slot never sat right at text size
/// (user report). Chips read as inline code: `@name` in the mono font over
/// A failed command discovery, translated for the popup.
fn slash_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "The session's device runs an older zeron — update it to list commands".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The session's device is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => {
            "Couldn't load this agent's commands".into()
        }
    }
}

pub struct Composer {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// Composer actions row: repo/branch/harness-model/traits (§1.7).
    /// Shared with the shell's new-session canvas, which renders the
    /// device/project target selectors ([`Pickers::render_target_selectors`]).
    pickers: Entity<Pickers>,
    /// Draft text per chat key ("" = new-chat canvas), surviving navigation.
    drafts: HashMap<String, String>,
    /// Staged-but-unsent attachments per chat key (use-attachments.ts `stash`):
    /// navigating away and back restores them; memory-only, like the original.
    attachments: HashMap<String, Vec<StagedAttachment>>,
    /// The staged attachment being viewed full-size (click a thumbnail).
    preview: Option<attachments::PreviewImage>,
    /// Focused while the lightbox is open so Escape reaches it; the input
    /// gets focus back on close.
    preview_focus: FocusHandle,
    /// Focus grab deferred to the next render (open sites don't all have a
    /// `Window` — the `ZERON_ATTACH_PREVIEW` boot knob opens in `new`).
    preview_focus_pending: bool,
    /// In-flight file-picker prompt (paperclip).
    picker_task: Option<Task<()>>,
    mention_task: Option<Task<()>>,
    mention: FileMentionState,
    slash_task: Option<Task<()>>,
    slash: SlashState,
    /// Advertised commands per harness (one `ListCommands` per harness per
    /// composer lifetime; the engine caches discovery on its side too).
    slash_cache: HashMap<HarnessId, Vec<SlashCommand>>,
    /// Slash-popup row scroll — the stack overflows into a wheel/keyboard-
    /// scrollable list once it outgrows the card.
    slash_scroll: gpui::ScrollHandle,
    /// File-mention popup row scroll (same treatment).
    mention_scroll: gpui::ScrollHandle,
    /// Shared scrollbar hover/drag state for both popups' floating rails —
    /// they never show at once (mutually exclusive by token shape).
    popup_bar: crate::popover::MenuScrollbarState,
    current_key: String,
    sending: bool,
    failure: Option<SharedString>,
    /// The chat key `failure` belongs to (`None` = global, e.g. "Engine not
    /// connected"). Chat-scoped failures survive navigation and render only
    /// under their own chat — a blanket clear-on-switch erased the one
    /// visible trace of a failed send (2026-08-19).
    failure_key: Option<String>,
    wizard: Option<Wizard>,
    wizard_focus: FocusHandle,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    answered_requests: HashSet<String>,
    advance_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    /// Interrupt/answer commands get their own slot: assigning `send_task`
    /// DROPPED an in-flight send future mid-upload — no banner, no cleanup,
    /// `sending` stuck true forever (2026-08-19 incident, "press Stop while
    /// a send grinds" shape).
    action_task: Option<Task<()>>,
    // -- compact/expanded flip state (hysteresis; see `composer_flip`) --
    /// Current layout mode (persisted across frames — never derived fresh).
    expanded_mode: bool,
    /// `layout_epoch` of the measurement that caused the last flip: the flip is
    /// re-evaluated only after the input has been laid out in the new mode, so
    /// at most one flip can happen per layout pass.
    flip_epoch: u64,
    /// Compact-mode input capacity, learned while compact (layout-stable).
    compact_capacity: f32,
    /// Input width first measured after expanding — container-width deltas
    /// while expanded shift `compact_capacity` by the same amount.
    expanded_anchor: f32,
    /// Last input width seen in the current mode (resize detection).
    last_seen_width: f32,
    /// Stable outer composer width supplied by the shell. Unlike Taffy's
    /// provisional input measurements, this changes only when the actual
    /// conversation column changes and can safely drive a follow-up render.
    last_available_width: Option<f32>,
    /// Set while an interactive resize is in flight; collapse is deferred
    /// until widths have settled for [`RESIZE_SETTLE_MS`].
    width_changed_at: Option<Instant>,
    settle_task: Option<Task<()>>,
    /// In-flight compact↔expanded morph (one per committed flip; manual
    /// drive — see [`FlipMorph`]).
    flip_morph: Option<FlipMorph>,
    /// Pill height actually rendered last frame — a committed flip morphs
    /// from here, so mid-flight reversals hand off without a jump.
    last_rendered_height: f32,
    /// Monotonic clock anchor for the morph timeline.
    morph_clock: Instant,
    /// Set on every session/route change: flips committed before this instant
    /// SNAP instead of morphing (see [`ROUTE_SNAP_MS`]).
    route_snap_until: Option<Instant>,
    _observe: Subscription,
    _pickers_observe: Subscription,
    _input_events: Subscription,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    /// The picker entity, for the shell's canvas target selectors.
    pub fn pickers(&self) -> &Entity<Pickers> {
        &self.pickers
    }

    /// Feed the stable conversation-column width into responsive composer
    /// controls.
    pub fn set_available_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let composer_width = width.clamp(0.0, COMPOSER_MAX_WIDTH);
        if composer_width_changed(self.last_available_width, composer_width) {
            self.last_available_width = Some(composer_width);
            // The shell renders before this child, so this queues one more
            // pass after the input has been laid out at its final width. That
            // pass can consume the completed measurement without emitting an
            // event from inside Taffy's multi-pass measurement callback.
            cx.notify();
        }
    }

    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = ComposerInput::new("Do anything…", cx);
            input.enable_mentions();
            input
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        // The footer toolbar (checkout kind + ref picker) is rendered INLINE
        // by the composer from picker state — a pickers-side notify (refs
        // loaded, popover toggled, pick made) must repaint the composer too.
        let pickers_observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.on_submit(cx),
            ComposerInputEvent::Edited | ComposerInputEvent::CursorMoved => {
                this.on_input_edited(cx)
            }
            ComposerInputEvent::ViewportChanged => cx.notify(),
            // The slash popup and the mention popup share the input's
            // completion key routing; they are mutually exclusive by token
            // shape (`/` at offset 0 vs `@` at a token boundary).
            ComposerInputEvent::MentionNavigate(delta) => {
                if this.slash.token.is_some() {
                    this.move_slash(*delta, cx)
                } else {
                    this.move_mention(*delta, cx)
                }
            }
            ComposerInputEvent::MentionAccept => {
                if this.slash.token.is_some() {
                    this.accept_slash(cx)
                } else {
                    this.accept_mention(cx)
                }
            }
            ComposerInputEvent::MentionDismiss => {
                if this.slash.token.is_some() {
                    this.dismiss_slash(cx)
                } else {
                    this.dismiss_mention(cx)
                }
            }
            ComposerInputEvent::PastedImages(images) => {
                let staged = images
                    .iter()
                    .map(|image| attachments::stage_clipboard_image(image.clone()))
                    .collect();
                this.add_staged(staged, cx);
            }
            ComposerInputEvent::PastedPaths(paths) => this.add_paths(paths.clone(), cx),
        });
        let current_key = state.read(cx).selected_chat.clone().unwrap_or_default();
        let mut composer = Self {
            state,
            input,
            pickers,
            drafts: HashMap::new(),
            attachments: HashMap::new(),
            preview: None,
            preview_focus: cx.focus_handle(),
            preview_focus_pending: false,
            picker_task: None,
            mention_task: None,
            mention: FileMentionState::default(),
            slash_task: None,
            slash: SlashState::default(),
            slash_cache: HashMap::new(),
            slash_scroll: gpui::ScrollHandle::new(),
            mention_scroll: gpui::ScrollHandle::new(),
            popup_bar: crate::popover::MenuScrollbarState::default(),
            current_key,
            sending: false,
            failure: None,
            wizard: None,
            wizard_focus: cx.focus_handle(),
            answered_requests: HashSet::new(),
            failure_key: None,
            action_task: None,
            advance_task: None,
            send_task: None,
            expanded_mode: false,
            flip_epoch: 0,
            compact_capacity: 0.0,
            expanded_anchor: 0.0,
            last_seen_width: 0.0,
            last_available_width: None,
            width_changed_at: None,
            settle_task: None,
            flip_morph: None,
            last_rendered_height: 0.0,
            morph_clock: Instant::now(),
            route_snap_until: None,
            _observe: observe,
            _pickers_observe: pickers_observe,
            _input_events: input_events,
        };
        // Dev knob: pre-stage attachments (drop/paste can't be synthesized on
        // a rig) — `ZERON_ATTACH=/path/a.png[,/path/b.png]`, and
        // `ZERON_ATTACH_PREVIEW=1` boots with the first one's lightbox open.
        if let Ok(spec) = std::env::var("ZERON_ATTACH") {
            let staged: Vec<StagedAttachment> = spec
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|path| {
                    match attachments::stage_file(std::path::Path::new(path.trim())) {
                        Ok(att) => Some(att),
                        Err(err) => {
                            tracing::warn!(%path, error = %err, "ZERON_ATTACH stage failed");
                            None
                        }
                    }
                })
                .collect();
            if std::env::var("ZERON_ATTACH_PREVIEW").is_ok_and(|v| v == "1")
                && let Some(first) = staged.first()
            {
                composer.preview = Some(attachments::PreviewImage {
                    name: first.name.clone().into(),
                    image: first.image.clone(),
                });
                composer.preview_focus_pending = true;
            }
            if !staged.is_empty() {
                composer
                    .attachments
                    .entry(composer.current_key.clone())
                    .or_default()
                    .extend(staged);
            }
        }
        composer
    }

    /// Capture-knob passthrough (`ZERON_OPEN_DIALOG=model`): open the
    /// combined harness/model menu.
    pub fn debug_open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pickers
            .update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
    }

    pub fn is_sending(&self) -> bool {
        self.sending
    }

    // ---- attachment staging (use-attachments.ts) ----

    /// Staged attachments for the chat the composer is showing.
    fn staged(&self) -> &[StagedAttachment] {
        self.attachments
            .get(&self.current_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn add_staged(&mut self, staged: Vec<StagedAttachment>, cx: &mut Context<Self>) {
        if staged.is_empty() {
            return;
        }
        self.attachments
            .entry(self.current_key.clone())
            .or_default()
            .extend(staged);
        cx.notify();
    }

    /// Stage image files (picker / drop / pasted paths). Non-images are
    /// skipped silently (matching the original's `image/*` filter); read
    /// failures and oversize files surface in the failure notice.
    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut staged = Vec::new();
        for path in &paths {
            if attachments::format_by_extension(path).is_none() {
                continue;
            }
            match attachments::stage_file(path) {
                Ok(att) => staged.push(att),
                Err(message) => {
                    self.failure = Some(message.into());
                    self.failure_key = Some(self.current_key.clone());
                    cx.notify();
                }
            }
        }
        self.add_staged(staged, cx);
    }

    fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(list) = self.attachments.get_mut(&self.current_key) {
            list.retain(|a| a.id != id);
            if list.is_empty() {
                self.attachments.remove(&self.current_key);
            }
        }
        cx.notify();
    }

    /// Drop a deleted chat's per-chat composer state — staged attachments hold
    /// raw image bytes, and a deleted chat's stage could never be sent again.
    pub fn purge_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.attachments.remove(chat_id);
        self.state.update(cx, |state, _| {
            state.purge_diff_comments(chat_id);
        });
    }

    /// Staged in `AppState` because the changes pane writes them.
    fn staged_comments(&self, cx: &App) -> Vec<crate::comments::DiffComment> {
        self.state
            .read(cx)
            .diff_comments(&self.current_key)
            .to_vec()
    }

    fn render_comments_chip(&self, theme: &Theme, cx: &App) -> Option<gpui::Div> {
        let count = self.staged_comments(cx).len();
        if count == 0 {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .px(px(STRIP_PAD_X))
                .pt(px(STRIP_PAD_TOP))
                .child(crate::badges::render(
                    "composer-comments",
                    &crate::badges::MessageBadge {
                        icon: crate::icons::CHAT_ROUND_LINE,
                        label: crate::comments::chip_label(count).into(),
                        // The staged set is already on screen in the changes
                        // pane, so a hover card would only repeat it.
                        details: Vec::new(),
                    },
                    theme,
                )),
        )
    }

    /// The staged-thumbnail strip (attachment-ui.tsx AttachmentStrip):
    /// `flex flex-wrap gap-2 px-4 pt-3`, 56px rounded thumbs, a remove button
    /// revealed on hover, click opens the full-size preview.
    fn render_attachment_strip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let staged = self.staged();
        if staged.is_empty() {
            return None;
        }
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(STRIP_GAP))
            .px(px(STRIP_PAD_X))
            .pt(px(STRIP_PAD_TOP));
        for (ix, att) in staged.iter().enumerate() {
            let group: SharedString = format!("composer-att-{}", att.id).into();
            let preview = attachments::PreviewImage {
                name: att.name.clone().into(),
                image: att.image.clone(),
            };
            let remove_id = att.id.clone();
            strip = strip.child(
                div()
                    .group(group.clone())
                    .relative()
                    .child(
                        div()
                            .id(("composer-att-thumb", ix))
                            .size(px(STRIP_THUMB))
                            .rounded(px(8.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(crate::theme::hairline(0.10))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview = Some(preview.clone());
                                this.preview_focus_pending = true;
                                cx.notify();
                            }))
                            .child(
                                img(att.image.clone())
                                    // EXPLICIT dims, not size_full: img layout
                                    // honors the image's intrinsic aspect
                                    // ratio over a percent height (gpui
                                    // f8d8a90 repoint), so size_full let a
                                    // tall photo grow past the frame — the
                                    // rectangular overflow clip then squared
                                    // the bottom corners (2026-08-19 report).
                                    // 56−2 = frame minus its 1px borders.
                                    .w(px(STRIP_THUMB - 2.0))
                                    .h(px(STRIP_THUMB - 2.0))
                                    // Own radii — the frame's rounding only
                                    // clips rectangularly (7 = 8 - border).
                                    .rounded(px(7.0))
                                    .object_fit(ObjectFit::Cover),
                            ),
                    )
                    // Own layer: inside the frosted pill everything shares one
                    // draw order and images render last, so without it the
                    // thumbnail paints OVER this button (user report).
                    .child(crate::frost::layered(
                        div()
                            .id(("composer-att-remove", ix))
                            .absolute()
                            .top(px(-6.0))
                            .right(px(-6.0))
                            .size(px(18.0))
                            .rounded_full()
                            .bg(theme.bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .shadow_sm()
                            .opacity(0.0)
                            .group_hover(group, |s| s.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // The button overhangs the thumbnail, whose
                                // hitbox is right underneath — don't let the
                                // same click also open the preview.
                                cx.stop_propagation();
                                this.remove_attachment(&remove_id, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    )),
            );
        }
        Some(strip)
    }

    /// Paperclip: the native image picker (the original's hidden
    /// `<input type=file accept=image/* multiple>`).
    fn open_file_picker(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |composer, cx| composer.add_paths(paths, cx))
                    .ok();
            }
        }));
    }

    fn sync_mention_controls(&mut self, cx: &mut Context<Self>) {
        let open = self.mention.token.is_some() || self.slash.token.is_some();
        let has_selection = if self.slash.token.is_some() {
            self.slash.active.is_some()
        } else {
            self.mention.active.is_some()
        };
        self.input.update(cx, |input, cx| {
            input.set_mention_controls(open, has_selection, cx)
        });
    }

    /// Tear down the entire completion lifecycle. Advancing the generation is
    /// important even when the spawned task is dropped: an RPC response may
    /// already be queued for delivery on the UI executor.
    fn reset_mention(&mut self, dismissed: Option<(Range<usize>, String)>, cx: &mut Context<Self>) {
        let request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        self.mention = FileMentionState {
            request,
            dismissed,
            ..FileMentionState::default()
        };
        self.sync_mention_controls(cx);
    }

    fn on_input_edited(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            if self.mention.token.is_some() || self.mention_task.is_some() {
                self.reset_mention(None, cx);
            }
            if self.slash.token.is_some() || self.slash_task.is_some() {
                self.reset_slash(None, cx);
            }
            return;
        }
        let (text, cursor) = {
            let input = self.input.read(cx);
            (input.text().to_string(), input.cursor_offset())
        };
        self.update_slash(&text, cursor, cx);
        let token = mention_token(&text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.mention
                .dismissed
                .as_ref()
                .is_some_and(|(range, value)| {
                    token.range == *range && text.get(range.clone()) == Some(value.as_str())
                })
        });
        if still_dismissed {
            self.mention.token = None;
            self.mention_task = None;
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.dismissed = None;
        if token == self.mention.token {
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        // Refining an open menu keeps the stale rows visible until the new
        // response lands — clearing here made the popup bounce through the
        // skeleton (and a different height) on every keystroke.
        let refining = self.mention.token.is_some() && token.is_some();
        self.mention.token = token.clone();
        if !refining {
            self.mention.results.clear();
            self.mention.active = None;
            // Fresh open: the row stack restarts at the top.
            reset_scroll_offset(&self.mention_scroll);
        }
        self.mention.error = None;
        self.mention.loading = token.is_some();
        self.sync_mention_controls(cx);
        let Some(token) = token else {
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.mention.loading = false;
            cx.notify();
            return;
        };
        let selected_worktree = match self.pickers.read(cx).checkout_plan() {
            crate::pickers::CheckoutPlan::ReuseWorktree { path, .. } => Some(path),
            _ => None,
        };
        let (params, target) = {
            let state = self.state.read(cx);
            let mut params = serde_json::Map::new();
            params.insert("query".into(), token.query.clone().into());
            let target = if let Some(chat) = state.selected_chat_row() {
                params.insert("chatId".into(), chat.id.clone().into());
                Some(chat.device_id.clone())
            } else if let Some(space) = state.selected_space_row() {
                params.insert("spaceId".into(), space.id.clone().into());
                if let Some(path) = selected_worktree {
                    params.insert("path".into(), path.into());
                }
                Some(space.device_id.clone())
            } else {
                None
            };
            if let Some(target) = &target {
                params.insert("targetDeviceId".into(), target.clone().into());
            }
            (serde_json::Value::Object(params), target)
        };
        if target.is_none() {
            self.mention.loading = false;
            cx.notify();
            return;
        }
        let request = self.mention.request;
        self.mention_task = Some(cx.spawn(async move |this, cx| {
            // A short debounce prevents one full workspace walk per keystroke
            // during normal typing. The generation check below still guards
            // requests that were already in flight when the query changed.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let mut result = engine
                .client()
                .call(methods::SEARCH_FILES, params.clone())
                .await;
            if matches!(result, Err(RpcError::Transport(_)) | Err(RpcError::Closed)) {
                // One retry rides out a cold relay dial to the host device
                // (the diffs pane retries forever; a keystroke-scoped search
                // gets a single second chance).
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = engine.client().call(methods::SEARCH_FILES, params).await;
            }
            this.update(cx, |composer, cx| {
                if !mention_response_is_current(&composer.mention, request) {
                    return;
                }
                composer.mention.loading = false;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<FileSearchMatch>>(value) {
                        Ok(results) => {
                            composer.mention.error = None;
                            composer.mention.active = (!results.is_empty()).then_some(0);
                            composer.mention.results = results;
                            // New result set: the row stack restarts at the top.
                            reset_scroll_offset(&composer.mention_scroll);
                        }
                        Err(err) => tracing::warn!(%err, "file mention response decode failed"),
                    },
                    Err(err) => {
                        tracing::warn!(%err, "file mention search failed");
                        composer.mention.results.clear();
                        composer.mention.active = None;
                        composer.mention.error = Some(mention_error_message(&err));
                    }
                }
                composer.sync_mention_controls(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn move_mention(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.mention.active =
            crate::popover::menu_step(self.mention.active, self.mention.results.len(), delta);
        if let Some(active) = self.mention.active {
            // Keep the keyboard cursor visible in the scrolled row stack.
            self.mention_scroll.scroll_to_item(active);
        }
        self.sync_mention_controls(cx);
        cx.notify();
    }

    fn dismiss_mention(&mut self, cx: &mut Context<Self>) {
        let dismissed = self.mention.token.as_ref().and_then(|token| {
            self.input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range.clone(), text.to_string()))
        });
        self.reset_mention(dismissed, cx);
        cx.notify();
    }

    fn accept_mention(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.mention.token.clone() else {
            return;
        };
        let Some((path, is_dir)) = self
            .mention
            .active
            .and_then(|active| self.mention.results.get(active))
            .map(|result| (result.path.clone(), result.is_dir))
        else {
            return;
        };
        self.input.update(cx, |input, cx| {
            input.replace_mention(token.range, &path, is_dir, cx)
        });
        self.reset_mention(None, cx);
        cx.notify();
    }

    fn render_file_mention_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let token = self.mention.token.as_ref()?;
        let mut card = crate::popover::popover_card(theme)
            .w_full()
            .max_h(px(320.0))
            .overflow_hidden()
            // GPUI dispatches this captured stream while the thumb is
            // dragged, including when the pointer has left the popup.
            .on_drag_move(cx.listener(Self::on_popup_bar_drag_move))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_mention(cx)));
        if self.mention.loading && self.mention.results.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "file-mention-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.mention.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.mention.results.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(if token.query.is_empty() {
                        "No files available"
                    } else {
                        "No matching files"
                    }),
            );
        } else {
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(self.mention.results.len());
            for (ix, result) in self.mention.results.iter().enumerate() {
                let selected = self.mention.active == Some(ix);
                let (directory, name) = match result.path.rsplit_once('/') {
                    Some((directory, name)) => (directory.to_string(), name.to_string()),
                    None => (String::new(), result.path.clone()),
                };
                rows.push(
                    crate::popover::menu_row(theme, selected, format!("file-mention-result-{ix}"))
                        .id(("file-mention-result", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mention.active = Some(ix);
                            this.accept_mention(cx);
                        }))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    crate::icons::icon(if result.is_dir {
                                        crate::icons::FOLDER
                                    } else {
                                        crate::icons::DOCUMENT
                                    })
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(13.0))
                                        .text_color(theme.text)
                                        .child(name),
                                )
                                .when(!directory.is_empty(), |row| {
                                    row.child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .overflow_hidden()
                                            .truncate()
                                            .text_size(px(12.5))
                                            .text_color(theme.text_muted)
                                            .child(directory),
                                    )
                                }),
                        )
                        .into_any_element(),
                );
            }
            // Overflowing rows wheel-scroll inside a bounded viewport; the
            // floating rail mirrors the model-list scrollbar treatment.
            card = card.child(
                div()
                    .id("mention-scroll-host")
                    .relative()
                    .on_hover(cx.listener(Self::on_popup_list_hover))
                    .child(
                        div()
                            .id("mention-list")
                            .max_h(px(312.0))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&self.mention_scroll)
                            .children(rows),
                    )
                    .children(self.popup_scrollbar(
                        "mention-scrollbar",
                        &self.mention_scroll,
                        theme,
                        cx,
                    )),
            );
        }
        Some(crate::popover::full_width_menu_above(
            "file-mention-popup",
            card.into_any_element(),
            None,
        ))
    }

    fn render_input_with_completion(&self) -> gpui::Div {
        div().relative().child(self.input.clone())
    }

    // ---- slash commands ---------------------------------------------------

    /// Track the `/` token on every edit: open/refresh the popup, fetch the
    /// harness's command list on first open, filter locally per keystroke.
    fn update_slash(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) {
        let token = slash_token(text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.slash.dismissed.as_ref().is_some_and(|(range, value)| {
                token.range == *range && text.get(range.clone()) == Some(value.as_str())
            })
        });
        if still_dismissed {
            self.slash.token = None;
            self.sync_mention_controls(cx);
            return;
        }
        self.slash.dismissed = None;
        let harness = self.pickers.read(cx).resolved(cx).harness;
        let harness_changed = self.slash.harness != harness;
        if token == self.slash.token && !harness_changed {
            self.refilter_slash(cx);
            return;
        }
        self.slash.token = token.clone();
        self.slash.harness = harness;
        self.slash.error = None;
        if token.is_none() {
            self.slash.active = None;
            self.sync_mention_controls(cx);
            return;
        }
        // No resolved harness (catalog still loading): empty popup, no fetch.
        let Some(harness) = harness else {
            self.slash.loading = false;
            self.refilter_slash(cx);
            return;
        };
        if self.slash_cache.contains_key(&harness) {
            self.slash.loading = false;
            self.refilter_slash(cx);
            return;
        }
        // First open for this harness: one ListCommands, targeted like file
        // search (the chat/space host device owns the agent binary).
        self.slash.request = self.slash.request.wrapping_add(1);
        self.slash.loading = true;
        self.refilter_slash(cx);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.slash.loading = false;
            return;
        };
        let target = {
            let state = self.state.read(cx);
            state
                .selected_chat_row()
                .map(|chat| chat.device_id.clone())
                .or_else(|| state.selected_space_row().map(|s| s.device_id.clone()))
        };
        let request = self.slash.request;
        self.slash_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "harness": harness });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert("targetDeviceId".into(), target.clone().into());
            }
            let result = engine.client().call(methods::LIST_COMMANDS, params).await;
            this.update(cx, |composer, cx| {
                if composer.slash.request != request {
                    return;
                }
                composer.slash.loading = false;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<SlashCommand>>(value) {
                        Ok(commands) => {
                            composer.slash_cache.insert(harness, commands);
                        }
                        Err(err) => tracing::warn!(%err, "slash command decode failed"),
                    },
                    Err(err) => {
                        tracing::debug!(%err, "slash command discovery failed");
                        composer.slash.error = Some(slash_error_message(&err));
                    }
                }
                composer.refilter_slash(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Re-rank the cached list for the current query (pure local filter).
    fn refilter_slash(&mut self, cx: &mut Context<Self>) {
        let query = self
            .slash
            .token
            .as_ref()
            .map(|t| t.query.clone())
            .unwrap_or_default();
        let commands = self
            .slash
            .harness
            .and_then(|h| self.slash_cache.get(&h))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        self.slash.filtered = crate::popover::filter_indices(&query, &names);
        self.slash.active = (!self.slash.filtered.is_empty()).then_some(0);
        // A fresh query/reopen restarts the row stack at the top.
        reset_scroll_offset(&self.slash_scroll);
        self.sync_mention_controls(cx);
        cx.notify();
    }

    fn move_slash(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.slash.active =
            crate::popover::menu_step(self.slash.active, self.slash.filtered.len(), delta);
        if let Some(active) = self.slash.active {
            // Keep the keyboard cursor visible in the scrolled row stack.
            self.slash_scroll.scroll_to_item(active);
        }
        self.sync_mention_controls(cx);
        cx.notify();
    }

    fn dismiss_slash(&mut self, cx: &mut Context<Self>) {
        let dismissed = self.slash.token.as_ref().and_then(|token| {
            self.input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range.clone(), text.to_string()))
        });
        self.reset_slash(dismissed, cx);
        cx.notify();
    }

    fn accept_slash(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.slash.token.clone() else {
            return;
        };
        let Some(command) = self
            .slash
            .active
            .and_then(|active| self.slash.filtered.get(active))
            .and_then(|&ix| {
                self.slash
                    .harness
                    .and_then(|h| self.slash_cache.get(&h))
                    .and_then(|c| c.get(ix))
            })
            .cloned()
        else {
            return;
        };
        self.input.update(cx, |input, cx| {
            input.replace_plain_token(token.range, &format!("/{}", command.name), cx)
        });
        self.reset_slash(None, cx);
        cx.notify();
    }

    /// Tear down the slash completion (mirrors [`Self::reset_mention`]).
    fn reset_slash(&mut self, dismissed: Option<(Range<usize>, String)>, cx: &mut Context<Self>) {
        let request = self.slash.request.wrapping_add(1);
        self.slash_task = None;
        self.slash = SlashState {
            request,
            dismissed,
            harness: self.slash.harness,
            ..SlashState::default()
        };
        self.sync_mention_controls(cx);
    }

    fn render_slash_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // Only while a slash token is active.
        self.slash.token.as_ref()?;
        let commands = self
            .slash
            .harness
            .and_then(|h| self.slash_cache.get(&h))
            .map(Vec::as_slice)
            .unwrap_or_default();
        // Full pill width at the mention card's height budget — both composer
        // completions share the same surface shape.
        let mut card = crate::popover::popover_card(theme)
            .w_full()
            .max_h(px(320.0))
            .overflow_hidden()
            // GPUI dispatches this captured stream while the thumb is
            // dragged, including when the pointer has left the popup.
            .on_drag_move(cx.listener(Self::on_popup_bar_drag_move))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_slash(cx)));
        if self.slash.loading && commands.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "slash-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.slash.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.slash.filtered.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(if commands.is_empty() {
                        "This agent has no slash commands"
                    } else {
                        "No matching commands"
                    }),
            );
        } else {
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(self.slash.filtered.len());
            for (row_ix, &cmd_ix) in self.slash.filtered.iter().enumerate() {
                let Some(command) = commands.get(cmd_ix) else {
                    continue;
                };
                let selected = self.slash.active == Some(row_ix);
                let name: SharedString = format!("/{}", command.name).into();
                let mut description = command.description.clone();
                if let Some(hint) = &command.input_hint {
                    if description.is_empty() {
                        description = format!("<{hint}>");
                    } else {
                        description = format!("{description} · <{hint}>");
                    }
                }
                let description: SharedString = description.into();
                rows.push(
                    crate::popover::menu_row(theme, selected, format!("slash-result-{row_ix}"))
                        .id(("slash-result", row_ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.slash.active = Some(row_ix);
                            this.accept_slash(cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    crate::icons::icon(crate::icons::COMMAND)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(12.5))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .overflow_hidden()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .child(description),
                                ),
                        )
                        .into_any_element(),
                );
            }
            // Overflowing rows wheel-scroll inside a bounded viewport; the
            // floating rail mirrors the model-list scrollbar treatment.
            card = card.child(
                div()
                    .id("slash-scroll-host")
                    .relative()
                    .on_hover(cx.listener(Self::on_popup_list_hover))
                    .child(
                        div()
                            .id("slash-list")
                            .max_h(px(312.0))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&self.slash_scroll)
                            .children(rows),
                    )
                    .children(self.popup_scrollbar(
                        "slash-scrollbar",
                        &self.slash_scroll,
                        theme,
                        cx,
                    )),
            );
        }
        // Full pill width above the composer, matching the file-mention popup.
        Some(crate::popover::full_width_menu_above(
            "slash-popup",
            card.into_any_element(),
            None,
        ))
    }

    /// The floating scrollbar rail for a composer popup's scroll host (the
    /// model-list treatment). Callers pass the id and that popup's scroll
    /// handle; the hover/drag interaction state is shared.
    fn popup_scrollbar(
        &self,
        id: &'static str,
        scroll: &gpui::ScrollHandle,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let metrics = self.popup_bar.metrics(scroll)?;
        Some(
            self.popup_bar
                .render_rail(theme, metrics)?
                .id(id)
                .on_hover(cx.listener(Self::on_popup_bar_hover))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_down),
                )
                .on_drag(crate::popover::MenuScrollbarDrag, |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| crate::popover::MenuScrollbarDragGhost)
                })
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_up),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_up),
                )
                .into_any_element(),
        )
    }

    /// The popup whose rows a scrollbar drag is moving — the tokens are
    /// mutually exclusive, so at most one exists.
    fn active_popup_scroll(&self) -> Option<gpui::ScrollHandle> {
        if self.slash.token.is_some() {
            Some(self.slash_scroll.clone())
        } else if self.mention.token.is_some() {
            Some(self.mention_scroll.clone())
        } else {
            None
        }
    }

    fn on_popup_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.popup_bar.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_popup_bar_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.popup_bar.set_bar_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_popup_bar_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scroll) = self.active_popup_scroll() else {
            return;
        };
        if !self.popup_bar.begin_press(&scroll, event.position.y) {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_popup_bar_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::popover::MenuScrollbarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scroll) = self.active_popup_scroll() else {
            return;
        };
        if self.popup_bar.drag_to(&scroll, event.event.position.y) {
            cx.notify();
        }
    }

    fn on_popup_bar_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.popup_bar.end_press();
        cx.notify();
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (key, pending) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone().unwrap_or_default(),
                pending_input_request(&s.transcript),
            )
        };

        // Draft swap on chat navigation — the input entity itself survives.
        if key != self.current_key {
            let old_text = self.input.read(cx).text().to_string();
            if old_text.is_empty() {
                self.drafts.remove(&self.current_key);
            } else {
                self.drafts.insert(self.current_key.clone(), old_text);
            }
            let draft = self.drafts.get(&key).cloned().unwrap_or_default();
            self.current_key = key;
            // `failure` deliberately survives navigation: chat-scoped
            // failures render only under their own chat (see `failure_key`),
            // so switching away and back must not erase the one visible
            // trace of a failed send.
            self.wizard = None;
            // Attachments stay stashed under their chat key (the map swap IS
            // the navigation); only the transient chrome resets.
            self.preview = None;
            self.reset_mention(None, cx);
            // Route changes snap (round 5/6): a mode difference between the
            // old and new session's composer must not glide across
            // navigation. Killing the in-flight morph here isn't enough —
            // the nav-driven flip only commits AFTER the swapped draft has
            // been re-measured, one or two renders later, so the whole
            // window snaps (see ROUTE_SNAP_MS).
            self.flip_morph = None;
            self.last_rendered_height = 0.0;
            self.route_snap_until = Some(Instant::now() + Duration::from_millis(ROUTE_SNAP_MS));
            self.input.update(cx, |input, cx| input.set_text(draft, cx));
        }

        // Question panel lifecycle (wizard state cached per request id).
        match pending {
            Some((request_id, questions)) if !self.answered_requests.contains(&request_id) => {
                let same = self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id);
                if !same {
                    self.reset_mention(None, cx);
                    self.wizard = Some(Wizard::new(request_id, questions));
                    self.advance_task = None;
                    // The shared input becomes the panel's free-text override.
                    self.input.update(cx, |input, cx| {
                        input.set_placeholder("Type your own answer, or pick an option above", cx)
                    });
                }
            }
            _ => {
                if let Some(wizard) = self.wizard.as_ref() {
                    // LATCH (original composer.tsx `inputLatch`): a transient
                    // fold/sync blip — or a steer appended behind the
                    // streaming entry — must not unmount the panel and lose
                    // the user's picks. Release only on explicit resolution
                    // (here or on another device) or when a NON-EMPTY
                    // transcript shows the question superseded (a newer
                    // assistant entry took over). Never on run death: the
                    // question stays answerable until answered — the engine
                    // delivers a dead run's answer as a resumed turn.
                    let released = {
                        let s = self.state.read(cx);
                        input_request_resolved(&s.transcript, &wizard.request_id)
                            || (!s.transcript.is_empty()
                                && !self.answered_requests.contains(&wizard.request_id))
                    };
                    if released {
                        self.wizard = None;
                        self.advance_task = None;
                        self.input
                            .update(cx, |input, cx| input.set_placeholder("Do anything…", cx));
                    }
                }
            }
        }
        cx.notify();
    }

    fn run_live(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        let Some(chat_id) = s.selected_chat.as_deref() else {
            return false;
        };
        matches!(
            s.indicator_for(chat_id, chrono::Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        )
    }

    /// New-chat sends need a project: with none picked (empty device, or a
    /// selection healed away) the send button dims and submit is a no-op —
    /// project-less `~`-cwd sessions are no longer mintable from the canvas.
    /// Existing chats carry their own project, so they always send.
    fn send_blocked(&self, cx: &App) -> bool {
        let state = self.state.read(cx);
        if state.selected_chat.is_some() {
            return false;
        }
        // New-chat canvas: needs a project AND a runnable agent. The
        // no-agents check only fires once the catalog is loaded — offline
        // and still-loading states must not block (the harness resolves from
        // the remembered default and the engine reports real failures).
        state.selected_space_row().is_none() || self.pickers.read(cx).no_agents_available()
    }

    fn button_mode(&self, cx: &App) -> SendButtonMode {
        let has_text = composer_has_content(
            self.input.read(cx).text(),
            self.staged().len(),
            self.staged_comments(cx).len(),
        );
        send_button_mode(self.run_live(cx), has_text)
    }

    fn on_submit(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            // Enter inside the panel's free-text input submits the page.
            let typed = self.input.read(cx).text().trim().to_string();
            if let Some(w) = self.wizard.as_mut() {
                w.set_typed(typed);
            }
            self.wizard_advance(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        let no_content =
            !composer_has_content(&text, self.staged().len(), self.staged_comments(cx).len());
        match self.button_mode(cx) {
            SendButtonMode::Stop => self.interrupt(cx),
            _ if no_content => {}
            _ if self.send_blocked(cx) => {}
            SendButtonMode::Send => self.send(text, false, cx),
            SendButtonMode::Steer => self.send(text, true, cx),
        }
    }

    /// Queue a Run (or Steer) doc command with an optimistic echo. New chats
    /// thread the picked config in: worktree creation (when the isolated toggle
    /// is on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
    /// reasoning / options on the Run request itself (§1.7).
    fn send(&mut self, text: String, steer: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            self.failure_key = None; // global — meaningful on every chat
            cx.notify();
            return;
        };
        // Chat id: existing selection, or client-minted for the new-chat canvas
        // (the chat then appears from the doc host once the doc materializes).
        let (chat_id, is_new) = match self.state.read(cx).selected_chat.clone() {
            Some(id) => (id, false),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        // Where the new session runs (Current checkout / reuse an existing
        // worktree / fresh worktree off the picked base) — resolved NOW so
        // the async block needs no picker access.
        let plan = self.pickers.read(cx).checkout_plan();
        // Fully-resolved model/reasoning/options — concrete values (chat config
        // or defaults), so the engine never has to guess a "default".
        let resolved = self.pickers.read(cx).resolved(cx);
        let existing_cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.cwd.clone());
        // The PROJECT fixes the new chat's device + base folder — sessions are
        // minted onto the project's device, not necessarily this one. With no
        // project ("Don't work in a project") the composer's device pick is
        // the host and the session runs from `~` there.
        let space = self.state.read(cx).selected_space_row().cloned();
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let target_device_id = self.state.read(cx).effective_device_id();
        let device_id = if is_new {
            target_device_id
                .clone()
                .unwrap_or_else(|| "local".to_string())
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
                .or_else(|| local_device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        };
        // Uploads/read-backs target the chat's HOST device (forwardable RPCs);
        // for a new chat that's the target device (None when it's local).
        let host_device_id = if is_new {
            target_device_id
                .clone()
                .filter(|id| local_device_id.as_deref() != Some(id.as_str()))
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
        };
        let space_id = space.as_ref().map(|s| s.id.clone());
        let space_path = space.as_ref().map(|s| s.path.clone());
        // Snapshot-and-clear NOW (use-attachments.ts takeAttachments): the
        // strip empties the instant you hit send; a failure hands the files
        // back into the chat's stash.
        let staged = self
            .attachments
            .remove(&self.current_key)
            .unwrap_or_default();
        // `typed` keeps the user's own words for the failure hand-back below:
        // restoring the folded prompt would paste the comment block into the
        // input as literal text.
        let key = self.current_key.clone();
        let comments = self.state.update(cx, |state, cx| {
            let taken = state.take_diff_comments(&key);
            if !taken.is_empty() {
                cx.notify();
            }
            taken
        });
        let typed = text.clone();
        let text = crate::comments::with_comments(&text, &comments);
        self.preview = None;
        let message_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().timestamp_millis();

        // Queued-attachment flow (durable-by-design): stage the bytes on the
        // LOCAL engine, queue the command immediately with `pending://` refs,
        // and let the engine push the bytes to a remote host afterwards —
        // staging must never gate the queue (2026-08-19 incident: a send
        // died with a zombie peer link because the upload sat in front of
        // QueueCommand). Requires every engine involved to understand the
        // ref scheme — the local engine (an IPC daemon may be older than
        // this UI) and, for remotely-hosted chats, the host; anything older
        // keeps the legacy blocking upload.
        let host_is_remote = host_device_id
            .as_deref()
            .is_some_and(|id| local_device_id.as_deref() != Some(id));
        let queued_flow = !staged.is_empty() && {
            let state = self.state.read(cx);
            let local_ok = local_device_id
                .as_deref()
                .is_some_and(|id| state.device_version_at_least(id, QUEUED_ATTACHMENTS_MIN));
            let host_ok = !host_is_remote
                || host_device_id
                    .as_deref()
                    .is_some_and(|id| state.device_version_at_least(id, QUEUED_ATTACHMENTS_MIN));
            local_ok && host_ok
        };
        // Upload identities minted NOW: in the queued flow the `pending://`
        // ref IS the persisted transport until the host rewrites it, so the
        // id must exist before any bytes move.
        let upload_ids: Vec<String> = staged
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        // The echo carries attachment refs from the first frame, so photos
        // render while the send is still pending. Queued flow: the refs are
        // the real `pending://` identities (stable — no post-upload refresh).
        // Legacy flow: synthetic `pending/…` paths that the post-upload
        // refresh replaces with the host's absolute paths. Either way the
        // staged bytes are seeded into the transcript cache under every
        // device key the transcript consults.
        let echo_paths: Vec<String> = if queued_flow {
            staged
                .iter()
                .zip(&upload_ids)
                .map(|(att, id)| format!("pending://{id}/{}", att.name))
                .collect()
        } else {
            staged
                .iter()
                .map(|att| format!("pending/{}/{}", att.id, att.name))
                .collect()
        };
        let echo_text = attachments::with_attachments(&text, &echo_paths);
        // Queued flow also seeds the UPLOAD ALIAS: the host rewrites the
        // persisted ref to `{its uploads dir}/{id8}-{name}` — an absolute
        // path the sender can't predict, but whose id8 it minted. The alias
        // keeps the thumbnail on the already-local bytes through that
        // rewrite instead of blanking into a reload skeleton.
        if queued_flow {
            for (upload_id, att) in upload_ids.iter().zip(&staged) {
                attachments::seed_attachment_alias(
                    &device_id,
                    upload_id,
                    &att.name,
                    att.image.clone(),
                );
                if let Some(local) = local_device_id.as_deref()
                    && local != device_id
                {
                    attachments::seed_attachment_alias(
                        local,
                        upload_id,
                        &att.name,
                        att.image.clone(),
                    );
                }
            }
        }
        for (path, att) in echo_paths.iter().zip(&staged) {
            attachments::seed_attachment(&device_id, path, &att.name, att.image.clone());
            if let Some(local) = local_device_id.as_deref()
                && local != device_id
            {
                attachments::seed_attachment(local, path, &att.name, att.image.clone());
            }
        }

        // Optimistic echo (client-minted id doubles as the persisted message id,
        // so the doc frame dedups it away).
        let echo = SessionMessageEntry {
            id: message_id.clone(),
            role: zeron_doc::MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: echo_text.clone(),
            }],
            created_at,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        self.state.update(cx, |s, cx| {
            if is_new {
                s.select_chat(Some(chat_id.clone()), cx);
            }
            s.push_echo(&chat_id, echo);
            // Working overlay until the host executes the queued command —
            // without it a remote send flashed Completed (and could ring the
            // done-chime) in the queue→drain→sync gap.
            s.begin_pending_send(&chat_id, &message_id, chrono::Utc::now());
            cx.notify();
        });

        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&self.current_key);
        self.failure = None;
        self.sending = true;
        cx.emit(ComposerEvent::Sent {
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
        });
        cx.notify();

        let steer_cmd = steer && !is_new;
        let restore_text = typed;
        let err_chat_id = chat_id.clone();
        let err_message_id = message_id.clone();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<(), String> = async {
                // Attachments stage FIRST — before the chat row or anything
                // else exists. Staging is chat-independent (keyed by
                // uploadId), and ordering it first makes a new-chat send
                // atomic: a staging failure aborts with NOTHING created,
                // instead of stranding a just-minted empty chat (v0.2.12
                // "failed to stage → empty transcript" report).
                //
                // Queued flow: commit the bytes to the LOCAL engine's uploads
                // dir (fast, offline-safe) — the queued command carries the
                // `pending://` refs and the engine delivers the bytes to a
                // remote host afterwards, retrying until they land. Legacy
                // flow (old engines): stage on the host device up front,
                // bounded by a total budget so a degraded link fails the send
                // loudly instead of grinding through silent per-chunk retries
                // for minutes.
                let mut content = text.clone();
                let mut attachment_paths: Vec<String> = Vec::new();
                let mut transfers: Vec<serde_json::Value> = Vec::new();
                if !staged.is_empty() && queued_flow {
                    // Local staging is disk-speed; publish progress anyway so
                    // huge files still narrate.
                    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let total: u64 = staged.iter().map(|a| a.bytes().len() as u64).sum();
                    {
                        let progress = progress.clone();
                        this.update(cx, |composer, cx| {
                            composer.state.update(cx, |s, cx| {
                                s.begin_upload_progress(total, progress);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                    for (att, upload_id) in staged.iter().zip(&upload_ids) {
                        if let Err(err) = attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            None,
                            upload_id,
                            att,
                            Some(progress.clone()),
                        )
                        .await
                        {
                            tracing::warn!(name = %att.name, error = %err, "local attachment stage failed");
                            return Err("Couldn't stage the attachment locally.".to_string());
                        }
                        transfers.push(serde_json::json!({
                            "uploadId": upload_id,
                            "fileName": att.name,
                        }));
                    }
                    // The echo refs ARE the persisted refs — no refresh pass.
                    attachment_paths = echo_paths.clone();
                    content = echo_text.clone();
                } else if !staged.is_empty() {
                    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let total: u64 = staged.iter().map(|a| a.bytes().len() as u64).sum();
                    {
                        let progress = progress.clone();
                        this.update(cx, |composer, cx| {
                            composer.state.update(cx, |s, cx| {
                                s.begin_upload_progress(total, progress);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                    for (att, upload_id) in staged.iter().zip(&upload_ids) {
                        match attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            host_device_id.as_deref(),
                            upload_id,
                            att,
                            Some(progress.clone()),
                        )
                        .await
                        {
                            Ok(path) => attachment_paths.push(path),
                            Err(err) => {
                                tracing::warn!(name = %att.name, error = %err, "attachment upload failed");
                                return Err(
                                    "Couldn't upload the attachment — the device may be offline."
                                        .to_string(),
                                );
                            }
                        }
                    }
                    // Seed the transcript cache from local bytes so the sent
                    // bubble's thumbnails never round-trip (seedTranscript-
                    // Attachment in the original send path).
                    let seed_device = host_device_id.clone().unwrap_or_else(|| device_id.clone());
                    for (path, att) in attachment_paths.iter().zip(&staged) {
                        attachments::seed_attachment(&seed_device, path, &att.name, att.image.clone());
                        if seed_device != device_id {
                            attachments::seed_attachment(&device_id, path, &att.name, att.image.clone());
                        }
                    }
                    content = attachments::with_attachments(&text, &attachment_paths);
                    // Refresh the echo in place with the attachment refs
                    // (same id, same clock — the bubble grows its thumbnails
                    // without flickering).
                    let refreshed = SessionMessageEntry {
                        id: message_id.clone(),
                        role: zeron_doc::MessageRole::User,
                        parts: vec![MessagePart::Text {
                            id: "t0".into(),
                            text: content.clone(),
                        }],
                        created_at,
                        device_id: "local".into(),
                        status: None,
                        continuation_of: None,
                    };
                    let echo_chat_id = chat_id.clone();
                    this.update(cx, |composer, cx| {
                        composer.state.update(cx, |s, cx| {
                            s.remove_echo(&echo_chat_id, &message_id);
                            s.push_echo(&echo_chat_id, refreshed);
                            cx.notify();
                        });
                    })
                    .ok();
                }

                // Resolve the working directory: existing chats keep theirs;
                // new chats run per the checkout plan (t3code env-mode): the
                // space's folder as-is, an EXISTING worktree of the picked ref
                // (a plain cwd override — multiple sessions share one
                // worktree), or a fresh isolated worktree created off the
                // picked base ref (CreateWorktree on send, targeted at the
                // space's device; the RPC relay-forwards).
                let mut cwd = if is_new {
                    // Project-less sessions run from the host's home dir —
                    // "~" is expanded on the host when the run spawns.
                    space_path.clone().or_else(|| Some("~".to_string()))
                } else {
                    existing_cwd
                }
                .unwrap_or_else(|| ".".to_string());
                let mut worktree_cwd: Option<String> = None;
                // Fresh-worktree plans ride the QUEUED Run command (a
                // WorktreeSpec the HOST materializes at drain time) instead of
                // a blocking CreateWorktree relay RPC here: the RPC had no
                // timeout, so a lost relay frame wedged the send on "Sending…"
                // forever while the session ran remotely anyway (2026-08-18).
                let mut run_worktree: Option<zeron_proto::WorktreeSpec> = None;
                // The picked ref rides createChat so the session footer names
                // it from the first frame (it read "Select ref" until the
                // host's diff reconciler got around to stamping the branch).
                let mut chat_branch: Option<String> = None;
                if is_new {
                    match &plan {
                        crate::pickers::CheckoutPlan::CurrentCheckout { branch } => {
                            chat_branch = branch.clone();
                        }
                        crate::pickers::CheckoutPlan::ReuseWorktree { path, branch } => {
                            cwd = path.clone();
                            worktree_cwd = Some(path.clone());
                            chat_branch = Some(branch.clone());
                        }
                        crate::pickers::CheckoutPlan::NewWorktree { base } => {
                            // Footer shows the base until the host stamps the
                            // actual zeron/<name> branch post-creation. cwd
                            // stays the repo folder — an old host that doesn't
                            // know the spec degrades to the main checkout
                            // instead of failing the run.
                            chat_branch = base.clone();
                            if let Some(repo_path) = &space_path {
                                // A remote repo's branch list loads over the
                                // relay — on a bad link it may never arrive
                                // and the picker has no base. That must NOT
                                // silently drop the isolation the user picked
                                // (2026-08-19: "New worktree" ran in the main
                                // checkout): default to HEAD, which git — any
                                // host version — resolves as the repo's
                                // current checkout state.
                                let base =
                                    base.clone().unwrap_or_else(|| "HEAD".to_string());
                                run_worktree = Some(zeron_proto::WorktreeSpec {
                                    repo_path: repo_path.clone(),
                                    base,
                                });
                            }
                        }
                    }
                }

                // Best-effort Mutate createChat with the picked config: the
                // engine resolves device + cwd from the PROJECT row when one
                // is picked; project-less chats name the host device outright
                // (idempotent; the doc host would materialize the chat on
                // first command anyway, so failures are non-fatal).
                if is_new {
                    let mut mutate = serde_json::json!({
                        "op": "createChat",
                        "chatId": chat_id,
                    });
                    if let Some(object) = mutate.as_object_mut() {
                        match &space_id {
                            Some(space_id) => {
                                object.insert(
                                    "spaceId".into(),
                                    serde_json::Value::String(space_id.clone()),
                                );
                            }
                            None => {
                                object.insert(
                                    "deviceId".into(),
                                    serde_json::Value::String(device_id.clone()),
                                );
                            }
                        }
                    }
                    if let Some(object) = mutate.as_object_mut() {
                        if let Some(worktree_cwd) = &worktree_cwd {
                            object.insert(
                                "cwd".into(),
                                serde_json::Value::String(worktree_cwd.clone()),
                            );
                        }
                        if let Some(branch) = &chat_branch {
                            object.insert(
                                "branch".into(),
                                serde_json::Value::String(branch.clone()),
                            );
                        }
                        if let Some(config) = resolved.chat_config()
                            && let Ok(config) = serde_json::to_value(&config)
                        {
                            object.insert("config".into(), config);
                        }
                    }
                    if let Err(err) = attachments::call_with_timeout(
                        &engine,
                        cx.background_executor(),
                        methods::MUTATE,
                        mutate,
                        std::time::Duration::from_secs(30),
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "CreateChat mutate unavailable; doc host will materialize the chat");
                    }
                }

                let command = if steer_cmd {
                    SessionCommandPayload::Steer {
                        prompt: content.clone(),
                        message_id: Some(message_id.clone()),
                    }
                } else {
                    SessionCommandPayload::Run {
                        request: RunRequest {
                            prompt: content.clone(),
                            harness: resolved.harness,
                            model: resolved.model.clone(),
                            reasoning: resolved.reasoning,
                            model_options: resolved.model_options.clone(),
                            cwd,
                            sandbox: SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            resume: None,
                            attachments: attachment_paths,
                            worktree: run_worktree,
                        },
                        message_id: message_id.clone(),
                    }
                };
                let command = serde_json::to_value(&command)
                    .map_err(|e| format!("Send failed: {e}"))?;
                let mut params = serde_json::json!({ "chatId": chat_id, "command": command });
                if !transfers.is_empty() {
                    params["transfers"] = serde_json::Value::Array(transfers);
                }
                // Deadline-bounded: QueueCommand is a local write (in-process
                // or IPC), but a deferred engine handle can park forever —
                // the send task must never grind silently (2026-08-19).
                attachments::call_with_timeout(
                    &engine,
                    cx.background_executor(),
                    methods::QUEUE_COMMAND,
                    params,
                    std::time::Duration::from_secs(30),
                )
                .await
                .map_err(|e| format!("Send failed: {e}"))?;
                Ok(())
            }
            .await;
            if result.is_err() && is_new {
                // A failed new-chat send must not strand a just-minted empty
                // chat in the sidebar (v0.2.12 "empty transcript" report).
                // Staging now runs before CreateChat, so usually nothing was
                // created — but a post-mutate failure (QueueCommand) still
                // leaves a row. Best-effort delete; a no-op if the chat was
                // never materialized.
                let _ = attachments::call_with_timeout(
                    &engine,
                    cx.background_executor(),
                    methods::MUTATE,
                    serde_json::json!({ "op": "deleteChat", "chatId": err_chat_id }),
                    std::time::Duration::from_secs(5),
                )
                .await;
            }
            this.update(cx, |composer, cx| {
                composer.sending = false;
                composer
                    .state
                    .update(cx, |s, _| s.end_upload_progress());
                if let Err(message) = result {
                    // Failure: red banner, echo removed, prompt back in the
                    // draft, staged files back in the stash. A failed NEW
                    // chat restores to the CANVAS (key "") and navigates back
                    // there — the minted chat is gone (deleted above), so
                    // nothing may restore under its key.
                    let restore_key = if is_new {
                        String::new()
                    } else {
                        err_chat_id.clone()
                    };
                    composer.failure = Some(message.into());
                    composer.failure_key = Some(restore_key.clone());
                    composer.state.update(cx, |s, cx| {
                        s.remove_echo(&err_chat_id, &err_message_id);
                        s.end_pending_send(&err_chat_id, &err_message_id);
                        if is_new && s.selected_chat.as_deref() == Some(err_chat_id.as_str()) {
                            // Back to the canvas; the navigation draft-swap
                            // loads the restored draft below.
                            s.select_chat(None, cx);
                        }
                        for comment in &comments {
                            s.add_diff_comment(&restore_key, comment.clone());
                        }
                        cx.notify();
                    });
                    if is_new && composer.current_key != restore_key {
                        // A re-key swap to the canvas is pending (the
                        // select_chat(None) above); it loads this draft into
                        // the input on flush — setting the input directly
                        // here would be clobbered by that same swap.
                        composer.drafts.insert(restore_key.clone(), restore_text.clone());
                    } else {
                        // Already keyed to the restore target (either an
                        // existing chat, or the deleted row's watch event
                        // re-keyed to the canvas before this handler ran —
                        // no further swap will fire). Set the input directly.
                        composer.input.update(cx, |input, cx| input.set_text(restore_text, cx));
                    }
                    if !staged.is_empty() {
                        // Merge by id (stashAttachments): files the user staged
                        // while the send was in flight survive the hand-back —
                        // draining the minted chat's slot too when the restore
                        // target is the canvas.
                        let mut merged = staged.clone();
                        for key in [err_chat_id.clone(), restore_key.clone()] {
                            if let Some(slot) = composer.attachments.get_mut(&key) {
                                let fresh: Vec<_> = slot
                                    .drain(..)
                                    .filter(|e| !merged.iter().any(|f| f.id == e.id))
                                    .collect();
                                merged.extend(fresh);
                            }
                        }
                        composer.attachments.insert(restore_key, merged);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let failure_chat = chat_id.clone();
        let params = serde_json::json!({
            "chatId": chat_id,
            "command": { "kind": "interrupt" },
        });
        // `action_task`, NOT `send_task`: a Stop pressed while a send is in
        // flight must not drop the send future on the floor.
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Stop failed: {err}").into());
                    composer.failure_key = Some(failure_chat);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    // ---- wizard glue ----

    fn wizard_select(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let step = wizard.select(option_ix);
        let has_pick = wizard.page_has_pick();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                if has_pick {
                    "Type your own answer, or leave this blank to use the selected option"
                } else {
                    "Type your own answer, or pick an option above"
                },
                cx,
            )
        });
        match step {
            WizardStep::AutoAdvance => self.schedule_auto_advance(cx),
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            WizardStep::Stay => {}
        }
        cx.notify();
    }

    fn schedule_auto_advance(&mut self, cx: &mut Context<Self>) {
        self.advance_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTO_ADVANCE_MS))
                .await;
            this.update(cx, |composer, cx| composer.wizard_advance(cx))
                .ok();
        }));
    }

    fn wizard_advance(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        match wizard.advance() {
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            _ => {
                // Moving on: clear the shared free-text input for the next page.
                self.input.update(cx, |input, cx| input.set_text("", cx));
                cx.notify();
            }
        }
    }

    fn wizard_back(&mut self, cx: &mut Context<Self>) {
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.back();
            cx.notify();
        }
    }

    /// Submit RespondInput and retire the panel.
    fn wizard_finish(&mut self, answers: Vec<UserInputAnswer>, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        self.advance_task = None;
        self.answered_requests.insert(wizard.request_id.clone());
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            // The panel borrowed the composer input; hand back its identity.
            input.set_placeholder("Do anything…", cx);
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let request_id = wizard.request_id.clone();
        let command = SessionCommandPayload::RespondInput {
            request_id: request_id.clone(),
            answers,
        };
        let failure_chat = chat_id.clone();
        let params = match serde_json::to_value(&command) {
            Ok(value) => serde_json::json!({ "chatId": chat_id, "command": value }),
            Err(_) => return,
        };
        // `action_task`, NOT `send_task` — see `interrupt`.
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Answer failed: {err}").into());
                    composer.failure_key = Some(failure_chat);
                    // The answer never left this device — put the panel back.
                    composer.answered_requests.remove(&request_id);
                    cx.notify();
                })
                .ok();
                return;
            }
            // Safety net against a dead-looking session: the command queued,
            // but the host may still REJECT it (e.g. the run's resolver is
            // gone). If the very same request is still the live pending input
            // once the host has had ample time to execute and the resolved
            // flag to sync back, the answer demonstrably didn't take —
            // un-hide the panel instead of leaving the question unanswerable.
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |composer, cx| {
                let still_pending = {
                    let s = composer.state.read(cx);
                    pending_input_request(&s.transcript)
                        .is_some_and(|(pending_id, _)| pending_id == request_id)
                };
                if still_pending && composer.answered_requests.remove(&request_id) {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_wizard_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // Keys bubbling out of the free-text input must not double-handle:
        // digits select options only while the input is empty, and Enter is the
        // input's own Submit action when it has focus.
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let input_empty = self.input.read(cx).is_empty();
        let key = event.keystroke.key.as_str();
        // A BARE digit picks an option. With a modifier held the keystroke
        // belongs to an app shortcut — ⌘1..⌘9 jump to a sidebar row — and the
        // panel must not also consume it as a selection.
        if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
            && !event.keystroke.modifiers.modified()
        {
            if !input_focused || input_empty {
                self.wizard_select(digit - 1, cx);
                // Consumed as a selection: stop the platform from also
                // inserting the digit into the focused free-text input.
                cx.stop_propagation();
            }
        } else if key == "enter" {
            if !input_focused {
                self.wizard_advance(cx);
                cx.stop_propagation();
            }
        } else if key == "escape" && (!input_focused || input_empty) {
            self.wizard_back(cx);
            cx.stop_propagation();
        }
    }

    // ---- render pieces ----

    /// The agent-asked-a-question panel (zeron question-panel.tsx), rendered in
    /// place of the composer: the same floating-pill chrome (`rounded-[26px]
    /// border-white/[0.08] bg-white/[0.03] shadow-xl`), uppercase header +
    /// "1/3" counter chip, option rows with number kbd chips, a free-text
    /// override over a hairline, and Back / Next-Submit footer.
    fn render_wizard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(wizard) = self.wizard.clone() else {
            return gpui::Empty.into_any_element();
        };
        let counter = wizard.counter();
        let Some(question) = wizard.current().cloned() else {
            return gpui::Empty.into_any_element();
        };
        let page = wizard.page;
        let last = page + 1 >= wizard.questions.len();
        let typed_empty = self.input.read(cx).is_empty();
        let can_advance = wizard.page_has_pick() || !typed_empty;

        let options = question.options.iter().enumerate().map(|(ix, label)| {
            // Selection reads on the row only while no typed override exists
            // (typed answers win — zeron question-panel.tsx `isSel`).
            let picked = wizard.is_picked(ix) && typed_empty;
            div()
                .id(("wizard-option", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(if picked {
                    crate::theme::ink(0.16)
                } else {
                    gpui::transparent_black()
                })
                // zeron question-panel.tsx option rows: `transition-colors`.
                .bg(if picked {
                    crate::theme::ink(0.09)
                } else {
                    motion::hover_blend(
                        &format!("wizard-option-{ix}"),
                        crate::theme::ink(0.025),
                        crate::theme::ink(0.06),
                    )
                })
                .on_hover(motion::hover_listener(format!("wizard-option-{ix}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.wizard_select(ix, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(crate::typography::ui_rems(13.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if picked {
                            theme.text
                        } else {
                            theme.text.opacity(0.9)
                        })
                        .child(SharedString::from(label.clone())),
                )
                .when(ix < 9, |el| {
                    el.child(
                        // Number kbd chip: `size-[22px] rounded-md text-[11px]`.
                        div()
                            .flex_none()
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .bg(if picked {
                                crate::theme::ink(0.16)
                            } else {
                                crate::theme::ink(0.05)
                            })
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(if picked {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.6)
                            })
                            .child(SharedString::from(format!("{}", ix + 1))),
                    )
                })
        });

        div()
            .id("question-panel")
            .track_focus(&self.wizard_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_wizard_key(event, window, cx)
            }))
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_frost(), |el| el.shadow_lg())
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.0))
                    .pt(px(16.0))
                    .flex()
                    .flex_col()
                    // Header: tracked uppercase + counter chip when paged.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(crate::popover::tracked_upper(
                                        &question.header,
                                    ))),
                            )
                            .when(wizard.questions.len() > 1, |el| {
                                el.child(
                                    div()
                                        .h(px(20.0))
                                        .px(px(6.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(6.0))
                                        .bg(crate::theme::ink(0.06))
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text_muted.opacity(0.6))
                                        .child(SharedString::from(counter)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .text_size(crate::typography::ui_rems(15.0))
                            .line_height(px(20.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(question.question.clone())),
                    )
                    .when(question.multi_select, |el| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted.opacity(0.65))
                                .child(SharedString::from("Select one or more options.")),
                        )
                    })
                    .child(
                        div()
                            .mt(px(12.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(options),
                    )
                    // Free-text override over a hairline (shares the composer
                    // input entity).
                    .child(
                        div()
                            .mt(px(12.0))
                            .border_t_1()
                            .border_color(crate::theme::hairline(0.06))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .px(px(4.0))
                            .child(self.input.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .pt(px(4.0))
                    .child(if page > 0 {
                        crate::popover::btn_ghost(&theme, "Back", "wizard-back")
                            .id("wizard-back")
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_back(cx)))
                            .into_any_element()
                    } else {
                        gpui::Empty.into_any_element()
                    })
                    .child(
                        crate::popover::btn_primary(&theme, if last { "Submit" } else { "Next" })
                            .id("wizard-submit")
                            .px(px(16.0))
                            .when(!can_advance, |el| el.opacity(0.4))
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_advance(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_send_button(
        &mut self,
        mode: SendButtonMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::of(cx);
        // Zeron composer-actions.tsx: a size-7 filled circle — up-arrow to
        // send/steer, a dark rounded square on the same light circle to stop.
        match mode {
            SendButtonMode::Stop => div()
                .id("composer-stop")
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .child(div().size(px(11.0)).rounded(px(3.0)).bg(theme.bg))
                .into_any_element(),
            SendButtonMode::Send | SendButtonMode::Steer => {
                // Dimmed and inert while no project is picked or no agent is
                // runnable (`send_blocked` also gates `on_submit`, so Enter
                // is a no-op too).
                let blocked = self.send_blocked(cx);
                div()
                    .id("composer-send")
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(blocked, |el| el.opacity(0.35))
                    .when(!blocked, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.opacity(0.85))
                            .on_click(cx.listener(|this, _, _, cx| this.on_submit(cx)))
                    })
                    .child(
                        crate::icons::icon(crate::icons::ARROW_UP)
                            .size(px(14.0))
                            .text_color(theme.bg),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Focus lands on the prompt input (window-level focus fallbacks — e.g. after
/// the focused terminal panel is hidden — route here).
impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let wizard_active = self.wizard.is_some();
        if self.mention.token.is_some()
            && (wizard_active || !self.input.focus_handle(cx).is_focused(window))
        {
            self.reset_mention(None, cx);
        }
        if self.slash.token.is_some()
            && (wizard_active || !self.input.focus_handle(cx).is_focused(window))
        {
            self.reset_slash(None, cx);
        }
        let mode = self.button_mode(cx);
        let (text_width, has_newline, content_height, last_width, epoch) = {
            let input = self.input.read(cx);
            (
                input.measured_text_width(),
                input.has_newline(),
                input.measured_content_height(),
                input.last_width,
                input.layout_epoch,
            )
        };
        let now = Instant::now();
        // Only measurements taken *after* the last flip may drive the next one
        // (at most one flip per layout pass — a flip invalidates the widths).
        let measured_since_flip = epoch > self.flip_epoch && last_width > 0.0;
        if measured_since_flip {
            // A same-mode width change is an interactive window/pane resize:
            // defer collapse until sizes settle for RESIZE_SETTLE_MS. Expansion
            // remains live so compact controls never squeeze the input away.
            if self.last_seen_width > 0.0 && (last_width - self.last_seen_width).abs() > 0.5 {
                self.width_changed_at = Some(now);
            }
            self.last_seen_width = last_width;
            if self.expanded_mode {
                if self.expanded_anchor <= 0.0 {
                    self.expanded_anchor = last_width;
                }
            } else {
                // The compact pill's content box is the layout-stable capacity
                // both thresholds measure against.
                self.compact_capacity = last_width - 8.0;
            }
        }
        let resizing = self
            .width_changed_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(RESIZE_SETTLE_MS));
        if resizing && self.settle_task.is_none() {
            // Re-evaluate once the settle window has passed.
            self.settle_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_SETTLE_MS + 20))
                    .await;
                this.update(cx, |composer, cx| {
                    composer.settle_task = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        // Layout-stable compact capacity: measured directly while compact;
        // while expanded, the learned value shifted by any container resize
        // (the expanded input width tracks the container 1:1).
        let capacity = if !self.expanded_mode {
            if last_width > 0.0 {
                last_width - 8.0
            } else {
                f32::MAX // before first measure default to compact
            }
        } else if self.compact_capacity > 0.0 {
            if self.expanded_anchor > 0.0 && last_width > 0.0 {
                self.compact_capacity + (last_width - self.expanded_anchor)
            } else {
                self.compact_capacity
            }
        } else {
            f32::MAX
        };
        let next = composer_flip(
            self.expanded_mode,
            text_width,
            capacity,
            has_newline,
            resizing,
        );
        let committed_flip = next != self.expanded_mode && measured_since_flip;
        if committed_flip {
            self.expanded_mode = next;
            self.flip_epoch = epoch;
            self.expanded_anchor = 0.0;
            // The mode change moves the input width; don't read that jump as
            // an interactive resize.
            self.last_seen_width = 0.0;
        }
        // New chats render expanded regardless of `expanded_mode` (see below),
        // so a mode flip there changes nothing visible — never morph it.
        let new_chat = self.state.read(cx).selected_chat.is_none();
        // Morph clock in ms; dividing by the measurement knob stretches the
        // timeline exactly like shell.rs eval_tween's scaled duration.
        let now_ms = self.morph_clock.elapsed().as_secs_f32() * 1000.0 / motion::speed_scale();
        let route_snap = self
            .route_snap_until
            .is_some_and(|until| Instant::now() < until);
        self.flip_morph = flip_morph_step(
            self.flip_morph,
            committed_flip && !new_chat,
            self.last_rendered_height,
            now_ms,
            motion::reduced_motion(cx),
            route_snap,
        );
        let expanded = self.expanded_mode;

        // Chat-scoped failures render only under their own chat; a global
        // failure (no key) renders everywhere.
        let failure = self.failure.clone().filter(|_| {
            self.failure_key
                .as_ref()
                .is_none_or(|key| *key == self.current_key)
        });
        // Composer honesty: when the target's delivery path is degraded, say
        // UP FRONT that a send will queue (a durable local write delivered on
        // reconnect) instead of letting the button imply instant delivery.
        let queue_notice: Option<(SharedString, bool)> = {
            use zeron_proto::ConnectivityState as S;
            let state = self.state.read(cx);
            let degraded = match state.selected_chat.as_deref() {
                Some(id) => state.chat_delivery_degraded(id),
                None => {
                    // New-chat canvas: judge by the picked target device.
                    let remote_target = state
                        .effective_device_id()
                        .is_some_and(|id| state.local_device_id.as_deref() != Some(id.as_str()));
                    remote_target
                        && (matches!(state.connectivity.state, S::Offline | S::Reconnecting)
                            || state
                                .effective_device_id()
                                .is_some_and(|id| !state.device_online(&id, chrono::Utc::now())))
                }
            };
            let offline = state.connectivity.state == S::Offline;
            degraded.then(|| {
                let text: SharedString = if offline {
                    "Offline — messages will send when you're back online.".into()
                } else {
                    "Messages will send once the connection recovers.".into()
                };
                (text, offline)
            })
        };
        // Centered composer column (zeron `mx-auto w-full max-w-3xl`).
        let container = div()
            .w_full()
            .max_w(px(COMPOSER_MAX_WIDTH))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            .when_some(failure, |el, message| {
                // zeron composer.tsx `Notice` (matches the transcript
                // ErrorChip palette): `flex items-start gap-2 rounded-xl
                // border px-3 py-2 text-[12px] leading-snug` with a 14px
                // DangerTriangle — a subtle tinted wash, not a bare red
                // stroke. Amber for the offline-ish case (engine not
                // connected), red for send/run failures. Click dismisses.
                let offline = message.as_ref() == "Engine not connected";
                let (border_c, wash, text_c) = if offline {
                    let amber = theme.warning; // amber-400
                    let amber_200 = theme.warning_muted;
                    (
                        amber.opacity(0.16),
                        amber.opacity(0.05),
                        amber_200.opacity(0.9),
                    )
                } else {
                    let danger = theme.danger; // red-400
                    let red_300 = theme.danger_muted;
                    (
                        danger.opacity(0.16),
                        danger.opacity(0.05),
                        red_300.opacity(0.9),
                    )
                };
                el.child(
                    div()
                        .id("composer-failure")
                        .mx(px(4.0))
                        .mt(px(6.0))
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(border_c)
                        .bg(wash)
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(16.0))
                        .text_color(text_c)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.failure = None;
                            this.failure_key = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(14.0))
                                .mt(px(2.0))
                                .text_color(text_c),
                        )
                        .child(div().min_w_0().child(message)),
                )
            })
            .when_some(queue_notice, |el, (notice, offline)| {
                // Not a warning box (v0.2.12 feedback: the amber Notice read
                // as an error and flashed on every blip — pre-grace). One
                // quiet caption line, amber dot only for hard offline; it
                // clears itself the moment the path heals.
                let dot = if offline {
                    theme.warning
                } else {
                    theme.text_faint
                };
                el.child(crate::motion::fade_in(
                    "composer-queue-notice",
                    div()
                        .id("composer-queue-notice")
                        .mx(px(8.0))
                        .mt(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(11.0))
                        .line_height(px(14.0))
                        .text_color(theme.text_faint)
                        .child(div().size(px(5.0)).rounded_full().bg(dot))
                        .child(div().min_w_0().truncate().child(notice)),
                ))
            });

        // Turn-boundary steering notice: for agents without mid-turn
        // injection (Grok over ACP today), a "steer" is queued and applies
        // when the current turn finishes. Without this hint the queue read
        // as a dropped steer (user report: "my steer didn't apply until
        // grok already finished").
        let steer_queues = mode == SendButtonMode::Steer
            && self.pickers.read(cx).resolved_steering_mode(cx)
                == Some(zeron_proto::SteeringMode::TurnBoundary);
        let container = container.when(steer_queues, |el| {
            el.child(
                div()
                    .mt(px(6.0))
                    .px(px(12.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child("This agent can't be steered mid-turn — your message will be queued and sent when the current turn finishes."),
            )
        });

        if wizard_active {
            let wizard = self.render_wizard(cx);
            return container.child(motion::fade_quick("composer-wizard", div().child(wizard)));
        }

        // New chats always use the expanded layout: the repo/branch pickers
        // need the full-width actions row (zeron composer-actions.tsx
        // `mustExpand = isNew || …`).
        let expanded = expanded || new_chat;

        // Committed-height morph: the layout below is already the NEW mode's;
        // only the pill's height (and the entrance fade/text glide driven by
        // `morph_t`) animates. Steady state renders exactly the target.
        // Staged attachments add the wrap strip's height to the pill in BOTH
        // modes (attachment-ui.tsx AttachmentStrip sits above the input row).
        let staged_count = self.staged().len();
        let strip_width_hint = if last_width > 0.0 { last_width } else { 720.0 };
        let strip_h = attachment_strip_height(staged_count, strip_width_hint);
        let comment_strip_h = comment_strip_height(self.staged_comments(cx).len());
        let base_height = if expanded {
            composer_total_height(content_height)
        } else {
            COMPACT_TOTAL_HEIGHT
        };
        let target_height = base_height + strip_h + comment_strip_h;
        let (pill_height, morph_t, morphing) = match self.flip_morph {
            Some(m) if !m.done(now_ms) => {
                (m.height(target_height, now_ms), m.progress(now_ms), true)
            }
            _ => (target_height, 1.0, false),
        };
        if !morphing {
            self.flip_morph = None;
        } else {
            // Manual tween drive: keep frames coming (shell.rs motion_active).
            window.request_animation_frame();
        }
        self.last_rendered_height = pill_height;

        let send_button = self.render_send_button(mode, cx);
        // Attach button — opens the native image picker (the original's hidden
        // `<input type=file accept="image/*" multiple>`); paste/drop also feed
        // the same strip. The parent action cluster owns the spacing: adding a
        // second margin here made the picker→attachment gap twice as wide as
        // attachment→send and made the paperclip look detached.
        let attach = div()
            .id("composer-attach")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            // zeron composer-actions.tsx attach: `transition-colors`.
            .bg(motion::hover_blend(
                "composer-attach",
                gpui::transparent_black(),
                crate::theme::ink(0.10),
            ))
            .on_hover(motion::hover_listener("composer-attach"))
            .on_click(cx.listener(|this, _, _, cx| this.open_file_picker(cx)))
            .child(
                crate::icons::icon(crate::icons::PAPERCLIP)
                    .size(px(16.0))
                    // The source path's painted bounds are centered at x=11
                    // inside a 24px viewbox. Correct that optical offset while
                    // keeping the 28px hit target geometrically centered.
                    .relative()
                    .left(px(1.0))
                    .text_color(theme.text_muted),
            );
        // Staged-thumbnail strip (attachment-ui.tsx AttachmentStrip), above
        // the input inside the pill in both modes.
        let strip = self.render_attachment_strip(&theme, cx);
        let comments_chip = self.render_comments_chip(&theme, cx);

        // The pill chrome (zeron composer.tsx): `rounded-[26px] border
        // border-white/[0.08] bg-white/[0.03] shadow-xl` — a floating pill with
        // a hairline over a faint wash, never a solid grey box. Picker chips,
        // attach, and the send circle all live INSIDE the pill.
        let pill_bg = theme.input_glass_bg();
        // No drop shadow on glass: it paints BEHIND the translucent fill and
        // shows through as an inner glow (theme.rs's card_selected_shadows
        // lesson; user report).
        let pill = div()
            .rounded(px(26.0))
            .bg(pill_bg)
            .border_1()
            .border_color(theme.border)
            .when(!theme.is_frost(), |el| el.shadow_lg());
        // The pill's bottom edge is stationary on screen (the composer sits at
        // the bottom of the shell column; growth moves the TOP edge), so the
        // controls pin to the bottom and only the text glides with the reveal
        // (round-9 follow-up: the send/attach/chips must not ride the height,
        // and none of them fade — the full cluster stays visible throughout).
        let cluster_dy = morph_cluster_dy(morph_t);
        let body = if expanded {
            // Expanded: textarea on top (`px-4 pb-1 pt-4`), actions row
            // (`px-3 pb-2.5 pt-1`, h-8 chips → 46px) ABSOLUTE at the pill's
            // stationary bottom — constant screen-y through the morph, with
            // the 2.5px compact↔expanded centering delta gliding out. The
            // text container is laid out at TARGET size (committed layout
            // never reflows mid-tween — the caret can't jump); its top pad
            // eases 12→16 so the first line glides from its compact resting
            // place. The whole control cluster stays at full alpha — chips,
            // attach and send are all (near-)stationary on the bottom anchor.
            let text_pt = morph_text_pad(morph_t);
            pill.h(px(pill_height))
                .overflow_hidden()
                .relative()
                .flex()
                .flex_col()
                .children(comments_chip)
                .children(strip)
                .child(
                    div()
                        .h(px(
                            (base_height - PILL_BORDER_V - ACTIONS_ROW_HEIGHT).max(0.0)
                        ))
                        .px(px(16.0))
                        .pt(px(text_pt))
                        .pb(px(4.0))
                        .child(self.render_input_with_completion()),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(-cluster_dy))
                        .h(px(ACTIONS_ROW_HEIGHT))
                        .flex()
                        .flex_row()
                        .items_center()
                        // Shared group geometry (see CLUSTER_X_DELTA): the
                        // attachment belongs to the utility pickers, while
                        // Send has a larger structural separation.
                        .gap(px(ACTION_PRIMARY_GAP))
                        .pl(px(12.0))
                        .pr(px(morph_cluster_inset(true, morph_t)))
                        .pt(px(4.0))
                        .pb(px(10.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_end()
                                .gap(px(ACTION_UTILITY_GAP))
                                .child(self.pickers.clone())
                                .child(attach),
                        )
                        .child(send_button),
                )
        } else {
            // Compact pill: input and the actions cluster on one 47px line
            // (`py-3 pl-4 pr-2` textarea, `gap-2 py-1.5 pl-1 pr-2` cluster;
            // the 22.75px line centers to the same 12px inset as `py-3`).
            // The row is BOTTOM-justified: during the collapse morph the pill
            // top sweeps down over a stationary row, the text walks down from
            // its expanded resting place via a decaying relative offset, and
            // the whole inline cluster (chips + attach/send) holds its spot at
            // full alpha (2.5px centering delta gliding in).
            let text_glide = match self.flip_morph {
                Some(m) if morphing => collapse_text_glide(m.from, morph_t),
                _ => 0.0,
            };
            pill.h(px(pill_height))
                .overflow_hidden()
                .flex()
                .flex_col()
                .justify_end()
                .children(comments_chip)
                .children(strip)
                .child(
                    div()
                        .h(px(COMPACT_TOTAL_HEIGHT - PILL_BORDER_V))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pl(px(16.0))
                                .pr(px(8.0))
                                .relative()
                                .top(px(-text_glide))
                                .child(self.render_input_with_completion()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                // Same utility/primary grouping as expanded;
                                // the right inset alone glides 12→8.
                                .gap(px(ACTION_PRIMARY_GAP))
                                .pl(px(4.0))
                                .pr(px(morph_cluster_inset(false, morph_t)))
                                .relative()
                                .top(px(-cluster_dy))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(ACTION_UTILITY_GAP))
                                        .child(self.pickers.clone())
                                        .child(attach),
                                )
                                .child(send_button),
                        ),
                )
        };
        // New sessions: the TARGET row (device + project chips) sits ABOVE
        // the pill, left-aligned like the checkout toolbar below it (user
        // request — moved off the canvas). Existing sessions name their
        // target in the titlebar instead.
        let container = if new_chat {
            let selectors = self
                .pickers
                .update(cx, |pickers, cx| pickers.render_target_selectors(cx));
            container.child(selectors)
        } else {
            container
        };
        // The file dropzone lives in the shell (the whole conversation column,
        // not just the pill — shell.rs `chat-dropzone`); drops land back here
        // via `add_paths`.
        // Frosted: the pill backdrop-blurs the transcript scrolling under it
        // (the popover glass treatment; radius matches the pill's rounding).
        let container = container.child(
            div()
                .relative()
                .child(crate::frost::frosted(
                    26.0,
                    16.0,
                    motion::fade_quick("composer-input", body),
                ))
                // Both completion popups span the full pill width above it —
                // the file-mention and slash tokens are mutually exclusive.
                .children(self.render_file_mention_popup(&theme, cx))
                .children(self.render_slash_popup(&theme, cx)),
        );
        // Branch/worktree toolbar under the pill (t3code BranchToolbar): the
        // checkout-kind selector + ref picker for new sessions, read-only
        // labels once the session exists. Git spaces only.
        let footer = self
            .pickers
            .update(cx, |pickers, cx| pickers.render_footer(cx));
        let container = match footer {
            Some(footer) => container.child(footer),
            None => container,
        };
        // Full-size preview of a staged thumbnail (AttachmentPreviewDialog).
        if let Some(preview) = self.preview.clone() {
            if std::mem::take(&mut self.preview_focus_pending) {
                window.focus(&self.preview_focus, cx);
            }
            let weak = cx.weak_entity();
            return container.child(attachments::lightbox(
                window.viewport_size(),
                &preview,
                &self.preview_focus,
                move |window, cx| {
                    // Hand focus back to the input so typing (and the next
                    // Escape) lands where it did before the lightbox opened.
                    if let Ok(input_focus) = weak.update(cx, |this, cx| {
                        this.preview = None;
                        cx.notify();
                        this.input.read(cx).focus_handle.clone()
                    }) {
                        window.focus(&input_focus, cx);
                    }
                },
            ));
        }
        container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The press intent is judged by eye everywhere except here: that a
    /// multi-click leaves the drag disarmed is invisible until a selection
    /// collapses under the pointer.
    #[test]
    fn a_press_of_two_or_more_clicks_takes_the_whole_field_and_leaves_the_drag_disarmed() {
        assert_eq!(press_intent(1, false), PressIntent::PlaceCaret);
        assert_eq!(press_intent(1, true), PressIntent::ExtendSelection);
        assert_eq!(press_intent(2, false), PressIntent::SelectAll);
        // A triple click keeps the whole field, so holding the button down
        // through a third click does not change what is selected.
        assert_eq!(press_intent(3, false), PressIntent::SelectAll);
        // The whole field wins over the shift modifier: shift has nothing
        // left to extend once everything is selected.
        assert_eq!(press_intent(2, true), PressIntent::SelectAll);
        // Only a caret press arms the drag. A select-all that armed it would
        // collapse to a drag selection on the next mouse move.
        assert!(press_intent(1, false).arms_drag());
        assert!(press_intent(1, true).arms_drag());
        assert!(!press_intent(2, false).arms_drag());
    }

    #[test]
    fn stable_outer_width_only_schedules_reflow_on_real_changes() {
        assert!(composer_width_changed(None, 400.0));
        assert!(!composer_width_changed(Some(400.0), 400.0));
        assert!(!composer_width_changed(Some(400.0), 400.5));
        assert!(composer_width_changed(Some(400.0), 400.51));
    }

    fn tooltip_target(range: Range<usize>, path: &str) -> MentionTooltipTarget {
        MentionTooltipTarget {
            range,
            path: path.into(),
        }
    }

    #[test]
    fn mention_tooltip_wait_survives_pointer_jitter_and_promotes_once() {
        let target = tooltip_target(3..20, "src/composer.rs");
        let waiting = MentionTooltipPhase::Waiting {
            target: target.clone(),
            generation: 1,
        };
        let restarted = mention_tooltip_reduce(waiting.clone(), Some(target.clone()), false, 2);
        assert_eq!(restarted, waiting);
        assert!(matches!(
            restarted,
            MentionTooltipPhase::Waiting { generation: 1, .. }
        ));
        assert_eq!(
            mention_tooltip_promote(restarted.clone(), 2, true),
            restarted,
            "a stale timer must not reveal the tooltip"
        );
        let visible = mention_tooltip_promote(restarted, 1, true);
        assert!(matches!(
            visible,
            MentionTooltipPhase::Visible { generation: 1, .. }
        ));
        assert_eq!(
            mention_tooltip_reduce(visible.clone(), Some(target), false, 3),
            visible,
            "one visible activation keeps its presentation generation stable"
        );
    }

    #[test]
    fn mention_tooltip_changes_target_and_cancels_disappeared_target() {
        let first = tooltip_target(0..10, "src/a.rs");
        let second = tooltip_target(20..30, "src/a.rs");
        let visible = MentionTooltipPhase::Visible {
            target: first,
            generation: 4,
        };
        assert!(matches!(
            mention_tooltip_reduce(visible, Some(second), false, 5),
            MentionTooltipPhase::Waiting { generation: 5, .. }
        ));
        assert_eq!(
            mention_tooltip_promote(
                MentionTooltipPhase::Waiting {
                    target: tooltip_target(20..30, "src/a.rs"),
                    generation: 5,
                },
                5,
                false,
            ),
            MentionTooltipPhase::Hidden
        );
    }

    #[test]
    fn mention_tooltip_stays_visible_over_chip_or_popup_only() {
        assert!(mention_tooltip_contains(true, false));
        assert!(mention_tooltip_contains(false, true));
        assert!(!mention_tooltip_contains(false, false));
    }

    #[test]
    fn mention_wash_moves_wholly_to_the_next_visual_row_at_a_wrap() {
        assert_eq!(
            display_row_segments(12..24, [12, 40]),
            vec![(1, 12, 12..24)]
        );
        assert_eq!(
            display_row_segments(8..24, [12, 40]),
            vec![(0, 0, 8..12), (1, 12, 12..24)]
        );
    }

    #[test]
    fn mention_token_requires_a_token_boundary_and_tracks_full_token() {
        assert_eq!(
            mention_token("Fix @src/com", 12),
            Some(MentionToken {
                range: 4..12,
                query: "src/com".into(),
            })
        );
        assert!(mention_token("mail@example.com", 16).is_none());
        assert!(mention_token("word@file", 9).is_none());
        assert!(mention_token("path/@file", 10).is_none());
        assert_eq!(
            mention_token("See (@lib", 9).map(|token| token.range),
            Some(5..9)
        );
    }

    #[test]
    fn slash_token_only_opens_the_prompt() {
        assert_eq!(
            slash_token("/comp", 5),
            Some(MentionToken {
                range: 0..5,
                query: "comp".into(),
            })
        );
        // Token range spans the whole command word even mid-cursor.
        assert_eq!(
            slash_token("/compact now", 3),
            Some(MentionToken {
                range: 0..8,
                query: "co".into(),
            })
        );
        // Not at offset 0 → prose, not a command.
        assert!(slash_token("run /compact", 12).is_none());
        // Cursor past the command word (typing the argument) → closed.
        assert!(slash_token("/goal ship it", 10).is_none());
        // A typed absolute path is not a command.
        assert!(slash_token("/usr/bin", 8).is_none());
        // Bare "/" with cursor at 0 → closed; cursor after it → open-all.
        assert!(slash_token("/", 0).is_none());
        assert_eq!(slash_token("/", 1).map(|t| t.query), Some(String::new()));
    }

    #[test]
    fn dismissed_mentions_reject_stale_responses() {
        let mut state = FileMentionState {
            token: mention_token("@src", 4),
            request: 7,
            ..FileMentionState::default()
        };
        assert!(mention_response_is_current(&state, 7));
        state.request += 1;
        state.token = None;
        assert!(!mention_response_is_current(&state, 7));
        assert!(!mention_response_is_current(&state, 8));
    }

    #[test]
    fn file_mentions_serialize_to_strict_local_markdown() {
        let raw = local_file_link("src/a file#[x].rs", false);
        assert_eq!(
            raw,
            "[a file#\\[x\\].rs](zeron-file:src/a%20file%23%5Bx%5D.rs)"
        );
        let links = file_mention_links(&raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, "src/a file#[x].rs");
        assert_eq!(links[0].basename, "a file#[x].rs");
        assert!(!links[0].is_dir);

        let folder = local_file_link("src/components", true);
        assert_eq!(folder, "[components](zeron-file:src/components/)");
        let links = file_mention_links(&folder);
        assert_eq!(links[0].path, "src/components");
        assert!(links[0].is_dir);
    }

    #[test]
    fn file_mentions_reject_external_or_noncanonical_markdown() {
        assert!(file_mention_links("[site](https://example.com/a)").is_empty());
        assert!(file_mention_links("[a.rs](../a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a file.rs)").is_empty());
        assert!(file_mention_links("[other](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src%5Cfake%5Ca.rs)").is_empty());
        assert!(file_mention_links("[a.rs](src/a%0A.rs)").is_empty());
    }

    #[test]
    fn duplicate_mention_basenames_use_unique_suffixes() {
        let raw = format!(
            "{} {}",
            local_file_link("src/one/mod.rs", false),
            local_file_link("src/two/mod.rs", false)
        );
        let projection = TextProjection::new(&raw);
        assert!(projection.display.contains("one/mod.rs"));
        assert!(projection.display.contains("two/mod.rs"));
    }

    #[test]
    fn mention_suffixes_compare_path_components() {
        let links = vec![
            FileMentionLink {
                range: 0..0,
                basename: "mod.rs".into(),
                path: "foo/mod.rs".into(),
                is_dir: false,
            },
            FileMentionLink {
                range: 0..0,
                basename: "oomod.rs".into(),
                path: "bar/oomod.rs".into(),
                is_dir: false,
            },
        ];
        assert_eq!(
            mention_display_labels(&links),
            vec!["mod.rs".to_string(), "oomod.rs".to_string()]
        );
    }

    #[test]
    fn projection_maps_and_expands_atomic_chip_ranges() {
        let raw = format!("open {} now", local_file_link("src/composer.rs", false));
        let projection = TextProjection::new(&raw);
        let (link, chip) = &projection.mentions[0];
        assert_eq!(
            &projection.display[chip.clone()],
            "\u{00A0}@composer.rs\u{00A0}"
        );
        assert_eq!(projection.display_to_raw(chip.start + 1), link.range.start);
        assert_eq!(projection.display_to_raw(chip.end - 1), link.range.end);
        assert_eq!(
            projection.previous_boundary(link.range.end),
            Some(link.range.start)
        );
        assert_eq!(
            projection.next_boundary(link.range.start),
            Some(link.range.end)
        );
        assert_eq!(
            projection.normalize_range(link.range.start + 2..link.range.end - 2),
            link.range
        );
    }

    #[test]
    fn sent_mention_display_projects_chips_for_the_transcript() {
        let raw = format!(
            "check {} and {}",
            local_file_link("src/composer.rs", false),
            local_file_link("src/components", true)
        );
        let (display, spans) = sent_mention_display(&raw).expect("mentions project");
        assert!(!display.contains(FILE_MENTION_SCHEME));
        assert!(display.contains("composer.rs"));
        assert!(display.contains("components"));
        assert_eq!(spans.len(), 2);
        assert_eq!(
            &display[spans[0].range.clone()],
            "\u{00A0}@composer.rs\u{00A0}"
        );
        assert!(!spans[0].is_dir);
        assert_eq!(spans[0].path.as_ref(), "src/composer.rs");
        assert!(spans[1].is_dir);
        assert_eq!(spans[1].path.as_ref(), "src/components/");
    }

    /// Ordinary prompts must stay on the zero-cost path, including ones that
    /// merely *talk about* the scheme without containing a valid mention.
    #[test]
    fn sent_mention_display_leaves_plain_prompts_untouched() {
        assert_eq!(sent_mention_display("fix the composer"), None);
        assert_eq!(
            sent_mention_display("what is a zeron-file: link?"),
            None,
            "scheme substring without a valid mention link"
        );
        assert_eq!(
            sent_mention_display("[a.rs](zeron-file:../a.rs)"),
            None,
            "a hostile path never becomes a chip in the transcript either"
        );
    }

    fn question(id: &str, options: &[&str], multi: bool) -> UserInputQuestion {
        UserInputQuestion {
            id: id.into(),
            header: "Header".into(),
            question: format!("Question {id}"),
            options: options.iter().map(|s| s.to_string()).collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn flip_decision() {
        // Fits in the pill → compact stays compact.
        assert!(!composer_flip(false, 150.0, 300.0, false, false));
        // Overflow → expand.
        assert!(composer_flip(false, 320.0, 300.0, false, false));
        // Newline always expands (either mode, even mid-resize).
        assert!(composer_flip(false, 10.0, 300.0, true, false));
        assert!(composer_flip(true, 10.0, 300.0, true, true));
        // Narrow column (< MIN_COMPACT_INPUT_WIDTH) always expands.
        assert!(composer_flip(false, 10.0, 199.0, false, false));
        assert!(!composer_flip(false, 10.0, 200.0, false, false));
    }

    #[test]
    fn flip_hysteresis_band_prevents_oscillation() {
        let cap = 300.0;
        // Text just over capacity expands…
        assert!(composer_flip(false, cap + 1.0, cap, false, false));
        // …and the SAME width, now expanded, does NOT collapse back — the
        // collapse threshold sits COLLAPSE_HYSTERESIS below the expand one.
        assert!(composer_flip(true, cap + 1.0, cap, false, false));
        // Anywhere inside the band the two modes are both stable (no width in
        // (cap - 32, cap] flips in either direction).
        let in_band = cap - COLLAPSE_HYSTERESIS + 1.0;
        assert!(!composer_flip(false, in_band, cap, false, false));
        assert!(composer_flip(true, in_band, cap, false, false));
        // Comfortably under the band → collapses.
        assert!(!composer_flip(
            true,
            cap - COLLAPSE_HYSTERESIS - 1.0,
            cap,
            false,
            false
        ));
    }

    #[test]
    fn resize_expands_live_but_defers_collapse() {
        // A compact composer expands immediately as its text or controls stop
        // fitting, even while the divider is moving.
        assert!(composer_flip(false, 500.0, 300.0, false, true));
        assert!(composer_flip(false, 10.0, 150.0, false, true));
        // An expanded composer waits for the drag to settle before collapsing,
        // avoiding mode chatter while the user reverses direction.
        assert!(composer_flip(true, 0.0, 300.0, false, true));
        // Once settled, the same wide layout may collapse.
        assert!(composer_flip(false, 500.0, 300.0, false, false));
        assert!(!composer_flip(true, 0.0, 300.0, false, false));
        assert!(composer_flip(false, 10.0, 150.0, false, false));
    }

    #[test]
    fn caret_blink_phase() {
        // Solid through the first half-period (typing burst never blinks).
        assert!(caret_visible(0));
        assert!(caret_visible(CARET_BLINK_MS - 1));
        // Off for the second half-period, back on for the third.
        assert!(!caret_visible(CARET_BLINK_MS));
        assert!(!caret_visible(2 * CARET_BLINK_MS - 1));
        assert!(caret_visible(2 * CARET_BLINK_MS));
    }

    #[test]
    fn auto_grow_math() {
        // The source heights (zeron composer.tsx line 235 clamp, composer-
        // actions.tsx row, 1px hairlines): 76+46+2 empty … 260+46+2 capped.
        assert_eq!(COMPOSER_MIN_HEIGHT, 124.0);
        assert_eq!(COMPOSER_MAX_HEIGHT, 308.0);
        // One line sits at the floor: the textarea BOX (content + `pt-4 pb-1`)
        // clamps UP to 76 exactly like `Math.max(scrollHeight, 76)` — this is
        // what makes the always-expanded new-chat composer 124px tall.
        assert_eq!(
            composer_total_height(input_content_height(1)),
            COMPOSER_MIN_HEIGHT
        );
        // Growth is linear once the textarea box exceeds its 76px floor.
        let h4 = composer_total_height(input_content_height(4));
        assert_eq!(
            h4,
            4.0 * INPUT_LINE_HEIGHT + TEXTAREA_PAD_V + ACTIONS_ROW_HEIGHT + PILL_BORDER_V
        );
        // Caps at a 260px textarea box (zeron max-h-[260px] / the JS clamp).
        assert_eq!(
            composer_total_height(input_content_height(100)),
            COMPOSER_MAX_HEIGHT
        );
        // Zero lines still measures one.
        assert_eq!(input_content_height(0), INPUT_LINE_HEIGHT);
    }

    #[test]
    fn input_wheel_scroll_uses_gpui_direction_and_clamps() {
        // Positive wheel delta moves toward the start; negative moves down.
        assert_eq!(input_scroll_offset(40.0, 20.0, 200.0, 100.0), 20.0);
        assert_eq!(input_scroll_offset(40.0, -30.0, 200.0, 100.0), 70.0);
        // Neither edge can be overscrolled.
        assert_eq!(input_scroll_offset(10.0, 50.0, 200.0, 100.0), 0.0);
        assert_eq!(input_scroll_offset(90.0, -50.0, 200.0, 100.0), 100.0);
        // Short content has no internal scroll range.
        assert_eq!(input_scroll_offset(20.0, -50.0, 80.0, 100.0), 0.0);
    }

    #[test]
    fn input_scroll_reveals_only_when_caret_leaves_viewport() {
        // A visible caret preserves the user's viewport.
        assert_eq!(
            input_scroll_offset_for_cursor(40.0, 60.0, 20.0, 300.0, 100.0),
            40.0
        );
        // Moving above or below reveals the row with the smallest adjustment.
        assert_eq!(
            input_scroll_offset_for_cursor(80.0, 30.0, 20.0, 300.0, 100.0),
            30.0
        );
        assert_eq!(
            input_scroll_offset_for_cursor(20.0, 130.0, 20.0, 300.0, 100.0),
            50.0
        );
        // Revealing the final row clamps exactly to the content end.
        assert_eq!(
            input_scroll_offset_for_cursor(0.0, 290.0, 20.0, 300.0, 100.0),
            200.0
        );
    }

    #[test]
    fn input_drag_autoscroll_is_edge_proportional_and_capped() {
        let top = 100.0;
        let bottom = 300.0;
        let line = INPUT_LINE_HEIGHT;
        assert_eq!(input_drag_scroll_delta(200.0, top, bottom, line), 0.0);
        assert_eq!(input_drag_scroll_delta(90.0, top, bottom, line), -2.0);
        assert_eq!(input_drag_scroll_delta(315.0, top, bottom, line), 3.0);
        assert_eq!(input_drag_scroll_delta(-100.0, top, bottom, line), -line);
        assert_eq!(input_drag_scroll_delta(500.0, top, bottom, line), line);
    }

    /// One frame short of the full morph timeline (never rounds up to done).
    const ALMOST: f32 = 179.0;

    #[test]
    fn flip_morph_starts_once_per_committed_flip() {
        // No committed flip → no morph.
        assert_eq!(flip_morph_step(None, false, 49.0, 0.0, false, false), None);
        // A committed flip starts one, from the last rendered height…
        let m = flip_morph_step(None, true, 49.0, 100.0, false, false).unwrap();
        assert_eq!(m.from, 49.0);
        assert_eq!(m.start_ms, 100.0);
        // …and same-mode renders keep it UNCHANGED (no restart at the
        // boundary, whatever the heights are doing).
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 150.0, false, false),
            Some(m)
        );
        // A finished morph clears on the next same-mode render.
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 100.0 + ALMOST, false, false),
            Some(m)
        );
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 300.0, false, false),
            None
        );
    }

    #[test]
    fn flip_morph_height_ramps_monotonically_to_target() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Starts exactly at the committed height…
        let mut prev = m.height(124.0, 0.0);
        assert_eq!(prev, 49.0);
        // …ramps without ever moving backwards…
        for step in 1..=18 {
            let h = m.height(124.0, step as f32 * 10.0);
            assert!(h >= prev, "height regressed at {step}: {h} < {prev}");
            prev = h;
        }
        // …and lands exactly on the target when done (and stays there).
        assert_eq!(m.height(124.0, 180.0), 124.0);
        assert!(m.done(180.0));
        assert_eq!(m.height(124.0, 500.0), 124.0);
        // Collapse runs the same ramp downward.
        assert!(m.height(124.0, 90.0) > 49.0);
        let down = FlipMorph {
            from: 124.0,
            start_ms: 0.0,
        };
        assert!(down.height(49.0, 90.0) < 124.0);
        assert!(down.height(49.0, 90.0) > 49.0);
    }

    #[test]
    fn flip_morph_reverse_hands_off_from_current_height() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        let mid = m.height(124.0, 90.0);
        assert!(mid > 49.0 && mid < 124.0);
        // A reverse flip mid-flight commits a new morph FROM the animated
        // height — continuous at the handoff, no pop to an endpoint.
        let rev = flip_morph_step(Some(m), true, mid, 90.0, false, false).unwrap();
        assert_eq!(rev.from, mid);
        assert_eq!(rev.height(49.0, 90.0), mid);
    }

    #[test]
    fn flip_morph_snaps_for_reduced_motion_and_first_paint() {
        // Reduced motion never creates a morph (the flip just snaps)…
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, true, false), None);
        // …and neither does a flip before anything was ever rendered.
        assert_eq!(flip_morph_step(None, true, 0.0, 0.0, false, false), None);
    }

    #[test]
    fn route_change_never_arms_the_morph() {
        // A flip committed inside the route-snap window must NOT animate —
        // switching sessions (chat↔chat or chat↔new-session) snaps the
        // composer straight to the target mode, like the header (round 6).
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, false, true), None);
        // The route change also kills anything already in flight…
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 50.0, false, true),
            None
        );
        assert_eq!(
            flip_morph_step(Some(m), true, 80.0, 50.0, false, true),
            None
        );
        // …while outside the window the same flip animates as usual.
        let armed = flip_morph_step(None, true, 49.0, 300.0, false, false).unwrap();
        assert_eq!(armed.from, 49.0);
    }

    #[test]
    fn morph_anchoring_holds_controls_and_glides_text() {
        // Steady state (progress 1): no offsets, everything at rest.
        assert_eq!(morph_cluster_dy(1.0), 0.0);
        assert_eq!(morph_text_pad(1.0), 16.0);
        assert_eq!(collapse_text_glide(124.0, 1.0), 0.0);
        // At the commit instant the pieces start from the OLD mode's resting
        // geometry: text pad at the compact 12px inset, cluster displaced by
        // exactly the 2.5px centering delta.
        assert_eq!(morph_text_pad(0.0), 12.0);
        assert_eq!(morph_cluster_dy(0.0), CLUSTER_Y_DELTA);
        // Collapse glide: starts where the expanded text sat (17px below the
        // committed pill top → `from − 53` above the compact resting spot)…
        assert_eq!(collapse_text_glide(124.0, 0.0), 71.0);
        // …decays monotonically to zero…
        let mut prev = collapse_text_glide(124.0, 0.0);
        for step in 1..=10 {
            let g = collapse_text_glide(124.0, step as f32 / 10.0);
            assert!(g <= prev, "glide regressed at {step}");
            prev = g;
        }
        // …and can't go negative on shallow mid-flight reversals.
        assert_eq!(collapse_text_glide(50.0, 0.0), 0.0);
    }

    #[test]
    fn cluster_inset_glides_between_the_source_endpoints() {
        assert_eq!(ACTION_UTILITY_GAP, 2.0);
        assert_eq!(ACTION_PRIMARY_GAP, Theme::SPACE_SM);
        assert!(ACTION_UTILITY_GAP < ACTION_PRIMARY_GAP);
        // The morph starts from the OLD mode's resting inset (no sideways
        // step at the commit) and eases to the committed mode's…
        assert_eq!(morph_cluster_inset(true, 0.0), 8.0); // expand: from compact pr-2
        assert_eq!(morph_cluster_inset(true, 1.0), 12.0); // …to expanded px-3
        assert_eq!(morph_cluster_inset(false, 0.0), 12.0); // collapse: from px-3
        assert_eq!(morph_cluster_inset(false, 1.0), 8.0); // …to pr-2
        // …monotonically, bounded by the 4px source delta.
        let mut prev = morph_cluster_inset(true, 0.0);
        for step in 1..=10 {
            let v = morph_cluster_inset(true, step as f32 / 10.0);
            assert!(v >= prev && v <= 8.0 + CLUSTER_X_DELTA);
            prev = v;
        }
        // Internal group spacing is shared between modes — only this wrapper
        // inset may differ across the flip.
    }

    #[test]
    fn flip_morph_tracks_live_target_and_drives_fade() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Auto-grow can move the target mid-morph: evaluation tracks the
        // live value instead of finishing on a stale height.
        assert!(m.height(159.0, 90.0) > m.height(124.0, 90.0));
        // The eased progress is the actions-row fade: 0 at commit, 1 at rest.
        assert_eq!(m.progress(0.0), 0.0);
        assert_eq!(m.progress(180.0), 1.0);
        let mid = m.progress(90.0);
        assert!(mid > 0.0 && mid < 1.0);
    }

    #[test]
    fn staged_comments_alone_are_content() {
        assert!(!composer_has_content("   ", 0, 0));
        assert!(composer_has_content("hi", 0, 0));
        assert!(composer_has_content("", 1, 0));
        assert!(composer_has_content("", 0, 1));
    }

    #[test]
    fn a_comment_only_stage_steers_a_live_run_instead_of_stopping_it() {
        let live = true;
        let comment_only = composer_has_content("", 0, 2);
        assert_eq!(
            send_button_mode(live, comment_only),
            SendButtonMode::Steer,
            "comment-only submit must steer, not interrupt the run"
        );
        // Nothing staged at all is still the stop square.
        assert_eq!(
            send_button_mode(live, composer_has_content("", 0, 0)),
            SendButtonMode::Stop
        );
    }

    #[test]
    fn send_button_morph() {
        assert_eq!(send_button_mode(false, false), SendButtonMode::Send);
        assert_eq!(send_button_mode(false, true), SendButtonMode::Send);
        assert_eq!(send_button_mode(true, true), SendButtonMode::Steer);
        assert_eq!(send_button_mode(true, false), SendButtonMode::Stop);
    }

    #[test]
    fn wizard_single_select_auto_advances_and_completes() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a", "b"], false),
                question("q2", &["x"], false),
            ],
        );
        assert_eq!(w.counter(), "1/2");
        assert_eq!(w.select(1), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.advance(), WizardStep::Stay);
        assert_eq!(w.counter(), "2/2");
        assert_eq!(w.select(0), WizardStep::AutoAdvance);
        let WizardStep::Done(answers) = w.advance() else {
            panic!("expected Done")
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].labels, vec!["b"]);
        assert_eq!(answers[1].labels, vec!["x"]);
    }

    #[test]
    fn wizard_multi_select_toggles_and_stays() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b", "c"], true)]);
        assert_eq!(w.select(0), WizardStep::Stay);
        assert_eq!(w.select(2), WizardStep::Stay);
        assert!(w.is_picked(0) && w.is_picked(2));
        // Toggle off.
        assert_eq!(w.select(0), WizardStep::Stay);
        assert!(!w.is_picked(0));
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["c"]);
    }

    #[test]
    fn wizard_number_keys_and_bounds() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b"], false)]);
        assert_eq!(w.press_number(9), WizardStep::Stay, "out of range ignored");
        assert_eq!(w.press_number(0), WizardStep::Stay);
        assert_eq!(w.press_number(2), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.select(5), WizardStep::Stay, "bad option ix ignored");
    }

    #[test]
    fn wizard_typed_answer_overrides_and_back_pages() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a"], false),
                question("q2", &["x", "y"], false),
            ],
        );
        w.select(0);
        w.advance();
        assert_eq!(w.page, 1);
        assert!(w.back());
        assert_eq!(w.page, 0);
        assert!(!w.back(), "already at first page");
        w.advance();
        w.set_typed("  custom answer  ".into());
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["a"]);
        assert_eq!(
            answers[1].labels,
            vec!["custom answer"],
            "typed overrides picked, trimmed"
        );
    }

    #[test]
    fn pending_input_detection() {
        use zeron_doc::MessageStatus;
        fn arc(entries: Vec<SessionMessageEntry>) -> Vec<Arc<SessionMessageEntry>> {
            entries.into_iter().map(Arc::new).collect()
        }
        let input_part = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![question("q", &["a"], false)],
            resolved: false,
        };
        let entry = |status: Option<MessageStatus>, parts: Vec<MessagePart>| SessionMessageEntry {
            id: "m".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
        };
        // Streaming entry with unresolved input → panel.
        let t = arc(vec![entry(
            Some(MessageStatus::Streaming),
            vec![input_part.clone()],
        )]);
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // DEAD entry with an unresolved input STILL gets the panel: the
        // question stays answerable until answered (the engine delivers the
        // answer as a resumed turn), so a run reaped under its question —
        // engine restart — must not orphan it (user report).
        let t = arc(vec![entry(
            Some(MessageStatus::Aborted),
            vec![input_part.clone()],
        )]);
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // A NEWER assistant entry supersedes an unanswered question.
        let t = arc(vec![
            entry(Some(MessageStatus::Aborted), vec![input_part.clone()]),
            SessionMessageEntry {
                id: "m2".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t2".into(),
                    text: "moved on".into(),
                }],
                created_at: 2,
                device_id: "d".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            },
        ]);
        assert!(pending_input_request(&t).is_none());
        // Resolved part → no panel.
        let resolved = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![],
            resolved: true,
        };
        let t = arc(vec![entry(
            Some(MessageStatus::Streaming),
            vec![resolved.clone()],
        )]);
        assert!(pending_input_request(&t).is_none());
        assert!(pending_input_request(&[]).is_none());

        // Regression (user forensics): a steer prompt appends a USER entry
        // AFTER the streaming assistant entry — the question must still be
        // found (a last-entry-only read vanished the panel exactly when the
        // user typed, bricking the answer flow).
        let user_echo = SessionMessageEntry {
            id: "u2".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t".into(),
                text: "I answered".into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        let t = arc(vec![
            entry(Some(MessageStatus::Streaming), vec![input_part.clone()]),
            user_echo,
        ]);
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into()),
            "question survives entries appended behind the streaming entry"
        );

        // Latch release: only an explicitly resolved matching part releases.
        assert!(!input_request_resolved(&t, "r1"));
        let t = arc(vec![entry(Some(MessageStatus::Streaming), vec![resolved])]);
        assert!(input_request_resolved(&t, "r1"));
        assert!(!input_request_resolved(&t, "other"));
    }
}
