//! Composer text input, file mentions, and slash commands.

use super::*;

/// The literal `@` a chip displays before its file name. Projected as TEXT so
/// it shapes, wraps, and hit-tests with the label — the earlier SVG icons
/// painted into a reserved whitespace slot never sat right at text size
/// (user report). Chips read as inline code: `@name` in the mono font over
/// the code wash.
const MENTION_PREFIX: char = '@';
const MENTION_TOOLTIP_DELAY: Duration = Duration::from_millis(420);
const MENTION_TOOLTIP_HEIGHT: f32 = 24.0;
const MENTION_SIDE_PAD: &str = "\u{00A0}";
/// A private URI scheme keeps file mentions distinguishable from ordinary
/// Markdown links pasted into the composer.
pub(crate) const FILE_MENTION_SCHEME: &str = "zeron-file:";

const UNDO_COALESCE: Duration = Duration::from_millis(700);
const UNDO_LIMIT: usize = 200;

pub(super) struct EditSnapshot {
    pub(super) content: String,
    pub(super) selected_range: Range<usize>,
    pub(super) selection_reversed: bool,
}

/// A strict, local-only Markdown representation of a file mention. The
/// underlying prompt always contains this form; the editor projects it to a
/// chip for display without leaking a second data model into submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileMentionLink {
    pub(super) range: Range<usize>,
    pub(super) basename: String,
    pub(super) path: String,
    pub(super) is_dir: bool,
}

pub(super) fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

pub(super) fn percent_decode_path(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let raw = encoded.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

pub(super) fn escape_mention_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

pub(super) fn local_file_link(path: &str, is_dir: bool) -> String {
    let path = path.trim_end_matches('/');
    let basename = path
        .rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(path);
    format!(
        "[{}]({}{})",
        escape_mention_label(basename),
        FILE_MENTION_SCHEME,
        percent_encode_path(&format!("{path}{}", if is_dir { "/" } else { "" }))
    )
}

pub(super) fn local_path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

pub(super) fn label_close(text: &str, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (at, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ']' && text[start + at + 1..].starts_with('(') {
            return Some(start + at);
        }
    }
    None
}

pub(super) fn file_mention_links(text: &str) -> Vec<FileMentionLink> {
    let mut links = Vec::new();
    let mut search = 0;
    while let Some(relative_start) = text[search..].find('[') {
        let start = search + relative_start;
        let Some(label_end) = label_close(text, start + 1) else {
            search = start + 1;
            continue;
        };
        let target_start = label_end + 2;
        let Some(relative_end) = text[target_start..].find(')') else {
            search = start + 1;
            continue;
        };
        let end = target_start + relative_end + 1;
        let label = &text[start + 1..label_end];
        let Some(encoded) = text[target_start..end - 1].strip_prefix(FILE_MENTION_SCHEME) else {
            search = end;
            continue;
        };
        let parsed = percent_decode_path(encoded).and_then(|target| {
            let is_dir = target.ends_with('/');
            let path = target.strip_suffix('/').unwrap_or(&target);
            (local_path_is_safe(path)
                && percent_encode_path(&target) == encoded
                && path
                    .rsplit('/')
                    .next()
                    .is_some_and(|basename| escape_mention_label(basename) == label))
            .then(|| (path.to_string(), is_dir))
        });
        if let Some((path, is_dir)) = parsed {
            let basename = path.rsplit('/').next().unwrap_or_default().to_string();
            links.push(FileMentionLink {
                range: start..end,
                basename,
                path,
                is_dir,
            });
        }
        search = end;
    }
    links
}

#[derive(Debug, Clone, Default)]
pub(super) struct TextProjection {
    pub(super) display: String,
    pub(super) mentions: Vec<(FileMentionLink, Range<usize>)>,
}

/// A path alone is not enough: two identical relative paths can appear in a
/// draft, so the raw range remains part of the hover identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MentionTooltipTarget {
    pub(super) range: Range<usize>,
    pub(super) path: SharedString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MentionTooltipPhase {
    Hidden,
    Waiting {
        target: MentionTooltipTarget,
        generation: u64,
    },
    Visible {
        target: MentionTooltipTarget,
        generation: u64,
    },
}

impl MentionTooltipPhase {
    pub(super) fn target(&self) -> Option<&MentionTooltipTarget> {
        match self {
            Self::Hidden => None,
            Self::Waiting { target, .. } | Self::Visible { target, .. } => Some(target),
        }
    }
}

/// Pure tooltip lifecycle reducer. Motion within the same chip preserves both
/// waiting and visible phases, so normal pointer jitter cannot starve the
/// delay or flicker an already-visible tooltip.
pub(super) fn mention_tooltip_reduce(
    phase: MentionTooltipPhase,
    pointer_target: Option<MentionTooltipTarget>,
    pointer_in_popup: bool,
    generation: u64,
) -> MentionTooltipPhase {
    match pointer_target {
        Some(target) if phase.target() == Some(&target) => phase,
        Some(target) => MentionTooltipPhase::Waiting { target, generation },
        None if pointer_in_popup && matches!(phase, MentionTooltipPhase::Visible { .. }) => phase,
        None => MentionTooltipPhase::Hidden,
    }
}

pub(super) fn mention_tooltip_promote(
    phase: MentionTooltipPhase,
    generation: u64,
    target_is_live: bool,
) -> MentionTooltipPhase {
    match phase {
        MentionTooltipPhase::Waiting {
            target,
            generation: current,
        } if current == generation && target_is_live => MentionTooltipPhase::Visible {
            target,
            generation: current,
        },
        MentionTooltipPhase::Waiting {
            generation: current,
            ..
        } if current == generation => MentionTooltipPhase::Hidden,
        phase => phase,
    }
}

pub(super) fn mention_tooltip_contains(in_chip: bool, in_popup: bool) -> bool {
    in_chip || in_popup
}

pub(super) fn display_row_segments(
    range: Range<usize>,
    row_ends: impl IntoIterator<Item = usize>,
) -> Vec<(usize, usize, Range<usize>)> {
    let mut segments = Vec::new();
    let mut row_start = 0usize;
    for (row_ix, row_end) in row_ends.into_iter().enumerate() {
        let start = range.start.max(row_start);
        let end = range.end.min(row_end);
        if start < end {
            segments.push((row_ix, row_start, start..end));
        }
        row_start = row_end;
        if row_start >= range.end {
            break;
        }
    }
    segments
}

#[derive(Debug, Clone)]
pub(super) struct MentionHit {
    pub(super) target: MentionTooltipTarget,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) anchor: Point<Pixels>,
}

impl TextProjection {
    pub(super) fn new(raw: &str) -> Self {
        let links = file_mention_links(raw);
        let labels = mention_display_labels(&links);
        let mut projection = Self::default();
        let mut raw_at = 0;
        for (link, label) in links.into_iter().zip(labels) {
            projection.display.push_str(&raw[raw_at..link.range.start]);
            let display_start = projection.display.len();
            // The chip is plain projected text — `@` plus the label between
            // non-breaking side bearings; the rounded code wash beneath it is
            // painted by `ComposerTextElement::paint`. Every character here
            // must exist in Geist (no exotic whitespace — U+2003/U+202F shape
            // at fallback width and collapsed the chip once already).
            projection.display.push_str(MENTION_SIDE_PAD);
            projection.display.push(MENTION_PREFIX);
            for ch in label.chars() {
                projection
                    .display
                    .push(if ch == ' ' { '\u{00A0}' } else { ch });
            }
            projection.display.push('\u{00A0}');
            let display_end = projection.display.len();
            projection
                .mentions
                .push((link.clone(), display_start..display_end));
            raw_at = link.range.end;
        }
        projection.display.push_str(&raw[raw_at..]);
        projection
    }

    pub(super) fn raw_to_display(&self, raw: usize) -> usize {
        let mut raw_at = 0;
        let mut display_at = 0;
        for (link, display) in &self.mentions {
            if raw <= link.range.start {
                return display_at + raw.saturating_sub(raw_at);
            }
            if raw < link.range.end {
                return display.start;
            }
            raw_at = link.range.end;
            display_at = display.end;
        }
        display_at + raw.saturating_sub(raw_at)
    }

    pub(super) fn display_to_raw(&self, display_offset: usize) -> usize {
        let mut raw_at = 0;
        let mut display_at = 0;
        for (link, display) in &self.mentions {
            if display_offset <= display.start {
                return raw_at + display_offset.saturating_sub(display_at);
            }
            if display_offset < display.end {
                return if display_offset - display.start < display.len() / 2 {
                    link.range.start
                } else {
                    link.range.end
                };
            }
            raw_at = link.range.end;
            display_at = display.end;
        }
        raw_at + display_offset.saturating_sub(display_at)
    }

    pub(super) fn normalize_range(&self, range: Range<usize>) -> Range<usize> {
        if range.is_empty() {
            for (link, _) in &self.mentions {
                if link.range.start < range.start && range.start < link.range.end {
                    let midpoint = link.range.start + link.range.len() / 2;
                    let at = if range.start < midpoint {
                        link.range.start
                    } else {
                        link.range.end
                    };
                    return at..at;
                }
            }
            return range;
        }
        let mut normalized = range;
        for (link, _) in &self.mentions {
            if normalized.start < link.range.end && normalized.end > link.range.start {
                normalized.start = normalized.start.min(link.range.start);
                normalized.end = normalized.end.max(link.range.end);
            }
        }
        normalized
    }

    pub(super) fn previous_boundary(&self, raw: usize) -> Option<usize> {
        self.mentions
            .iter()
            .find_map(|(link, _)| (raw == link.range.end).then_some(link.range.start))
    }

    pub(super) fn next_boundary(&self, raw: usize) -> Option<usize> {
        self.mentions
            .iter()
            .find_map(|(link, _)| (raw == link.range.start).then_some(link.range.end))
    }
}

/// Basenames are compact in the common case. When the same basename appears
/// more than once, use the shortest unique path suffix so chips remain
/// distinguishable without always expanding to full paths.
pub(super) fn mention_display_labels(links: &[FileMentionLink]) -> Vec<String> {
    links
        .iter()
        .enumerate()
        .map(|(ix, link)| {
            if links
                .iter()
                .filter(|other| other.basename == link.basename)
                .count()
                == 1
            {
                return link.basename.clone();
            }
            let parts: Vec<_> = link.path.split('/').collect();
            (1..=parts.len())
                .map(|count| parts[parts.len() - count..].join("/"))
                .find(|suffix| {
                    let suffix: Vec<_> = suffix.split('/').collect();
                    links.iter().enumerate().all(|(other_ix, other)| {
                        other_ix == ix
                            || !other
                                .path
                                .split('/')
                                .rev()
                                .take(suffix.len())
                                .eq(suffix.iter().rev().copied())
                    })
                })
                .unwrap_or_else(|| link.path.clone())
        })
        .collect()
}

/// One chip in a *sent* message: its byte range over the projected display
/// string (`@label` between side bearings). The transcript renders these
/// read-only — no editing state, no tooltip machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMentionSpan {
    pub range: Range<usize>,
    /// Full workspace-relative path (labels can be shortened to basenames).
    pub path: SharedString,
    pub is_dir: bool,
}

/// Project a sent message's raw Markdown for transcript display: mention links
/// collapse to the same chip labels the composer shows, everything else passes
/// through untouched. `None` when the text has no valid mention — the
/// substring probe keeps ordinary prompts on the zero-allocation path, so this
/// is safe to call for every user row.
pub fn sent_mention_display(raw: &str) -> Option<(String, Vec<SentMentionSpan>)> {
    if !raw.contains(FILE_MENTION_SCHEME) {
        return None;
    }
    let projection = TextProjection::new(raw);
    if projection.mentions.is_empty() {
        return None;
    }
    let spans = projection
        .mentions
        .iter()
        .map(|(link, display)| SentMentionSpan {
            range: display.clone(),
            path: SharedString::from(format!(
                "{}{}",
                link.path,
                if link.is_dir { "/" } else { "" }
            )),
            is_dir: link.is_dir,
        })
        .collect();
    Some((projection.display, spans))
}

/// Direction of the last edit — a run only merges with edits of its own kind.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum EditKind {
    Insert,
    Delete,
}

/// Bind the composer keymap. Call once at app boot.
pub fn init(cx: &mut App) {
    let ctx = Some("Composer");
    let mut bindings = vec![
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("tab", MentionTab, ctx),
        KeyBinding::new("escape", MentionEscape, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        // macOS line/document motion — a laptop keyboard has no home/end keys,
        // so Cmd+arrow is the only way users reach either edge.
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("cmd-up", DocStart, ctx),
        KeyBinding::new("cmd-down", DocEnd, ctx),
        KeyBinding::new("shift-cmd-left", SelectHome, ctx),
        KeyBinding::new("shift-cmd-right", SelectEnd, ctx),
        KeyBinding::new("shift-cmd-up", SelectDocStart, ctx),
        KeyBinding::new("shift-cmd-down", SelectDocEnd, ctx),
        // Line-edge deletion (Cmd+Delete on macOS).
        KeyBinding::new("cmd-backspace", DeleteToLineStart, ctx),
        KeyBinding::new("cmd-delete", DeleteToLineEnd, ctx),
    ];
    for prefix in ["cmd", "ctrl"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-z"), Undo, ctx));
        bindings.push(KeyBinding::new(&format!("shift-{prefix}-z"), Redo, ctx));
    }
    // Word-level editing: Option on macOS, Ctrl on Windows/Linux.
    let word_edit_prefix = if cfg!(target_os = "macos") {
        "alt"
    } else {
        "ctrl"
    };
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-backspace"),
        DeleteWordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-delete"),
        DeleteWordRight,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-left"),
        WordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-right"),
        WordRight,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-left"),
        SelectWordLeft,
        ctx,
    ));
    bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-right"),
        SelectWordRight,
        ctx,
    ));
    for prefix in ["cmd", "ctrl"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, ctx));
    }
    // Palette-search context: TEXT-EDITING keys only. gpui dispatches matched
    // keybindings BEFORE raw key listeners (window.rs `dispatch_key_event`),
    // so anything bound here can never reach a palette's `on_key_down` —
    // navigation keys (up/down/left/right/enter) are deliberately unbound and
    // bubble to the palette frame instead.
    let palette = Some("PaletteSearch");
    let mut palette_bindings = vec![
        KeyBinding::new("backspace", Backspace, palette),
        KeyBinding::new("delete", Delete, palette),
        KeyBinding::new("home", Home, palette),
        KeyBinding::new("end", End, palette),
        KeyBinding::new("shift-left", SelectLeft, palette),
        KeyBinding::new("shift-right", SelectRight, palette),
        // Modifier-qualified motion is safe here: the palette's own navigation
        // uses BARE arrows/enter, which stay unbound and bubble to its frame.
        KeyBinding::new("cmd-left", Home, palette),
        KeyBinding::new("cmd-right", End, palette),
        KeyBinding::new("shift-cmd-left", SelectHome, palette),
        KeyBinding::new("shift-cmd-right", SelectEnd, palette),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, palette),
    ];
    palette_bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-backspace"),
        DeleteWordLeft,
        palette,
    ));
    palette_bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-delete"),
        DeleteWordRight,
        palette,
    ));
    palette_bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-left"),
        WordLeft,
        palette,
    ));
    palette_bindings.push(KeyBinding::new(
        &format!("{word_edit_prefix}-right"),
        WordRight,
        palette,
    ));
    palette_bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-left"),
        SelectWordLeft,
        palette,
    ));
    palette_bindings.push(KeyBinding::new(
        &format!("shift-{word_edit_prefix}-right"),
        SelectWordRight,
        palette,
    ));
    for prefix in ["cmd", "ctrl"] {
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-z"), Undo, palette));
        palette_bindings.push(KeyBinding::new(&format!("shift-{prefix}-z"), Redo, palette));
    }
    cx.bind_keys(palette_bindings);
    cx.bind_keys(bindings);
}

/// Events the composer wrapper listens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerInputEvent {
    Submitted,
    Edited,
    CursorMoved,
    ViewportChanged,
    MentionNavigate(isize),
    MentionAccept,
    MentionDismiss,
    /// Images pasted from the clipboard (screenshots / copied image data) —
    /// the wrapper stages them as attachments (use-attachments.ts onPaste).
    PastedImages(Vec<gpui::Image>),
    /// File paths pasted from the clipboard (a file manager "Copy").
    PastedPaths(Vec<PathBuf>),
}

/// Multiline input entity: content + selection + IME marked text + measured
/// layout (wrapped lines) for mouse mapping and auto-grow.
pub struct ComposerInput {
    /// Key context for the binding map ("Composer", or "PaletteSearch" for
    /// palette filters whose navigation keys must bubble).
    pub(super) key_context: &'static str,
    pub(super) focus_handle: FocusHandle,
    pub(super) content: String,
    pub(super) placeholder: SharedString,
    pub(super) selected_range: Range<usize>,
    pub(super) selection_reversed: bool,
    pub(super) marked_range: Option<Range<usize>>,
    pub(super) is_selecting: bool,
    pub(super) drag_position: Option<Point<Pixels>>,
    pub(super) drag_generation: u64,
    pub(super) drag_autoscroll_active: bool,
    /// Vertical scroll inside the input once content exceeds the max height.
    pub(super) scroll_top: f32,
    /// Normally keeps the caret visible through edits and rewraps. Manual
    /// wheel scrolling pauses it until the next caret move or edit.
    pub(super) follow_cursor: bool,
    // -- measured state (written during layout/paint) --
    pub(super) last_lines: Vec<WrappedLine>,
    pub(super) line_starts: Vec<usize>,
    pub(super) last_bounds: Option<Bounds<Pixels>>,
    pub(super) line_height: Pixels,
    pub(super) content_height: f32,
    pub(super) max_line_width: f32,
    pub(super) last_width: f32,
    /// Raw Markdown → chip display projection from the last layout pass.
    pub(super) projection: TextProjection,
    /// Inline completion preview: painted in faint ink after the text while
    /// the caret sits at the end (palette tab-completion). Owned by the
    /// wrapper — it recomputes and re-sets this on every render pass, so the
    /// input never has to know what the completion means.
    pub(super) ghost: Option<SharedString>,
    /// File mentions are a composer feature, not a behavior of generic inputs
    /// (picker searches and rename fields also use this type).
    pub(super) mentions_enabled: bool,
    /// Bumped once per `layout_text` pass — the flip logic uses it to apply at
    /// most one compact↔expanded flip per layout (a flip is only re-evaluated
    /// after the input has been measured in the new mode).
    pub(super) layout_epoch: u64,
    pub(super) display_is_placeholder: bool,
    /// Caret blink anchor: reset on every keystroke/caret move so the caret is
    /// solid while typing and blinks at [`CARET_BLINK_MS`] when idle.
    pub(super) blink_anchor: Instant,
    /// Half-period repaint driver, alive only while the input is focused.
    pub(super) blink_task: Option<Task<()>>,
    // -- undo history --
    pub(super) undo_stack: Vec<EditSnapshot>,
    pub(super) redo_stack: Vec<EditSnapshot>,
    /// Kind, trailing offset, and time of the last edit — the merge test that
    /// decides whether the next edit extends the current undo step.
    pub(super) last_edit: Option<(EditKind, usize, Instant)>,
    /// The wrapper owns mention state; this only redirects bound keys while a
    /// mention token is active, keeping input focus and native text editing.
    pub(super) mention_open: bool,
    pub(super) mention_has_selection: bool,
    /// Last prepainted chip bounds; the paint-phase pointer listener uses
    /// these instead of attempting to infer text geometry from the cursor.
    pub(super) mention_hits: Vec<MentionHit>,
    pub(super) mention_tooltip: MentionTooltipPhase,
    pub(super) mention_tooltip_generation: u64,
    pub(super) mention_tooltip_popup: Option<Bounds<Pixels>>,
    pub(super) mention_tooltip_task: Option<Task<()>>,
    /// Created once when Waiting promotes; retaining this entity preserves
    /// GPUI's global animation state across prepaint frames.
    pub(super) mention_tooltip_view: Option<Entity<MentionPathTooltip>>,
}

impl ComposerInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self::with_context(placeholder, "Composer", cx)
    }

    /// An input in a custom KEY context — palettes use `"PaletteSearch"`,
    /// whose keymap binds only text-editing keys so navigation keys bubble to
    /// the surrounding frame (see `init`).
    pub fn with_context(
        placeholder: impl Into<SharedString>,
        key_context: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            key_context,
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            drag_position: None,
            drag_generation: 0,
            drag_autoscroll_active: false,
            scroll_top: 0.0,
            follow_cursor: true,
            last_lines: Vec::new(),
            line_starts: vec![0],
            last_bounds: None,
            line_height: px(INPUT_LINE_HEIGHT),
            content_height: INPUT_LINE_HEIGHT,
            max_line_width: 0.0,
            last_width: 0.0,
            projection: TextProjection::default(),
            ghost: None,
            mentions_enabled: false,
            layout_epoch: 0,
            display_is_placeholder: true,
            blink_anchor: Instant::now(),
            blink_task: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            mention_open: false,
            mention_has_selection: false,
            mention_hits: Vec::new(),
            mention_tooltip: MentionTooltipPhase::Hidden,
            mention_tooltip_generation: 0,
            mention_tooltip_popup: None,
            mention_tooltip_task: None,
            mention_tooltip_view: None,
        }
    }

    /// Reset the caret blink phase (solid again) — called on every edit and
    /// caret move, matching textarea behavior.
    pub(super) fn reset_blink(&mut self) {
        self.blink_anchor = Instant::now();
    }

    /// Caret paint gate: focused input in an active window, in the "on" blink
    /// phase. Also (re)arms the half-period repaint driver while focused, and
    /// drops it on blur so an unfocused input schedules no frames.
    pub(super) fn caret_shown(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let focused = self.focus_handle.is_focused(window);
        if !focused || !window.is_window_active() {
            self.blink_task = None;
            return false;
        }
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(CARET_BLINK_MS))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        caret_visible(self.blink_anchor.elapsed().as_millis() as u64)
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_mention_controls(
        &mut self,
        open: bool,
        has_selection: bool,
        cx: &mut Context<Self>,
    ) {
        if self.mention_open == open && self.mention_has_selection == has_selection {
            return;
        }
        self.mention_open = open;
        self.mention_has_selection = has_selection;
        cx.notify();
    }

    pub(super) fn enable_mentions(&mut self) {
        self.mentions_enabled = true;
        self.refresh_projection();
    }

    pub(super) fn refresh_projection(&mut self) {
        self.projection = if self.mentions_enabled {
            TextProjection::new(&self.content)
        } else {
            TextProjection {
                display: self.content.clone(),
                mentions: Vec::new(),
            }
        };
    }

    /// Replace a completed `@query` token as one non-coalescing undo step.
    pub fn replace_mention(
        &mut self,
        range: Range<usize>,
        path: &str,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_mention_tooltip();
        let path = local_file_link(path, is_dir);
        let next = self.content[range.end..].chars().next();
        let existing_separator = next.filter(|ch| ch.is_whitespace() && *ch != '\n' && *ch != '\r');
        let inserted = if existing_separator.is_some() {
            path
        } else {
            format!("{path} ")
        };
        self.record_edit(&range, &inserted);
        self.content =
            self.content[..range.start].to_owned() + &inserted + &self.content[range.end..];
        self.refresh_projection();
        let cursor =
            range.start + inserted.len() + existing_separator.map(char::len_utf8).unwrap_or(0);
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    /// Replace a completed plain-text token (slash commands) as one
    /// non-coalescing undo step. Unlike [`Self::replace_mention`], the
    /// replacement is ordinary text — no link, no chip projection.
    pub fn replace_plain_token(
        &mut self,
        range: Range<usize>,
        replacement: &str,
        cx: &mut Context<Self>,
    ) {
        let next = self.content[range.end..].chars().next();
        let existing_separator = next.filter(|ch| ch.is_whitespace() && *ch != '\n' && *ch != '\r');
        let inserted = if existing_separator.is_some() {
            replacement.to_owned()
        } else {
            format!("{replacement} ")
        };
        self.record_edit(&range, &inserted);
        self.content =
            self.content[..range.start].to_owned() + &inserted + &self.content[range.end..];
        self.refresh_projection();
        let cursor =
            range.start + inserted.len() + existing_separator.map(char::len_utf8).unwrap_or(0);
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Set (or clear) the inline completion preview. Only paints while the
    /// caret sits at the end of a non-empty draft — see the prepaint gate.
    pub fn set_ghost(&mut self, ghost: Option<SharedString>, cx: &mut Context<Self>) {
        if self.ghost == ghost {
            return;
        }
        self.ghost = ghost;
        cx.notify();
    }

    pub fn has_newline(&self) -> bool {
        self.content.contains('\n')
    }

    /// Unwrapped width of the widest line — feeds the compact/expanded flip.
    pub fn measured_text_width(&self) -> f32 {
        self.max_line_width
    }

    pub fn measured_content_height(&self) -> f32 {
        self.content_height
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.invalidate_mention_tooltip();
        self.content = text.into();
        self.refresh_projection();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = 0.0;
        self.follow_cursor = true;
        // Programmatic replacement (draft load, clear-on-submit) is a new
        // document, not an edit — undo must not reach back past it.
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    pub(super) fn invalidate_mention_tooltip(&mut self) {
        self.mention_tooltip_generation = self.mention_tooltip_generation.wrapping_add(1);
        self.mention_tooltip = MentionTooltipPhase::Hidden;
        self.mention_tooltip_popup = None;
        self.mention_tooltip_task = None;
        self.mention_tooltip_view = None;
    }

    pub(super) fn set_mention_hits(&mut self, hits: Vec<MentionHit>) {
        self.mention_hits = hits;
        let live = self
            .mention_tooltip
            .target()
            .is_none_or(|target| self.mention_hits.iter().any(|hit| &hit.target == target));
        if !live {
            self.invalidate_mention_tooltip();
        }
    }

    pub(super) fn start_mention_tooltip_wait(&mut self, target: MentionTooltipTarget, cx: &mut Context<Self>) {
        self.mention_tooltip_generation = self.mention_tooltip_generation.wrapping_add(1);
        let generation = self.mention_tooltip_generation;
        self.mention_tooltip = MentionTooltipPhase::Waiting { target, generation };
        self.mention_tooltip_popup = None;
        self.mention_tooltip_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MENTION_TOOLTIP_DELAY).await;
            this.update(cx, |input, cx| {
                let live = input.mention_tooltip.target().is_some_and(|target| {
                    input.mention_hits.iter().any(|hit| &hit.target == target)
                });
                let next = mention_tooltip_promote(input.mention_tooltip.clone(), generation, live);
                if next != input.mention_tooltip {
                    input.mention_tooltip = next;
                    input.mention_tooltip_task = None;
                    if let MentionTooltipPhase::Visible { target, generation } =
                        &input.mention_tooltip
                    {
                        input.mention_tooltip_view = Some(cx.new(|_| MentionPathTooltip {
                            path: target.path.clone(),
                            activation: *generation,
                        }));
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub(super) fn on_mention_pointer_move(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.invalidate_mention_tooltip();
            return;
        }
        let target = self
            .mention_hits
            .iter()
            .find(|hit| hit.bounds.contains(&position))
            .map(|hit| hit.target.clone());
        let in_popup = self
            .mention_tooltip_popup
            .is_some_and(|popup| popup.contains(&position));
        let next_generation = self.mention_tooltip_generation.wrapping_add(1);
        let next = mention_tooltip_reduce(
            self.mention_tooltip.clone(),
            target.clone(),
            in_popup,
            next_generation,
        );
        if next == self.mention_tooltip {
            return;
        }
        match next {
            MentionTooltipPhase::Waiting { target, .. } => {
                self.start_mention_tooltip_wait(target, cx)
            }
            _ => {
                self.invalidate_mention_tooltip();
                self.mention_tooltip = next;
                cx.notify();
            }
        }
    }

    fn visible_mention_tooltip(
        &self,
    ) -> Option<(
        MentionTooltipTarget,
        Point<Pixels>,
        u64,
        Entity<MentionPathTooltip>,
    )> {
        let MentionTooltipPhase::Visible { target, generation } = &self.mention_tooltip else {
            return None;
        };
        self.mention_hits
            .iter()
            .find(|hit| hit.target == *target)
            .and_then(|hit| {
                let view = self.mention_tooltip_view.clone()?;
                Some((target.clone(), hit.anchor, *generation, view))
            })
    }

    fn check_mention_tooltip_visibility(
        &mut self,
        popup: Bounds<Pixels>,
        pointer: Point<Pixels>,
    ) -> bool {
        let Some((target, _, _, _)) = self.visible_mention_tooltip() else {
            return false;
        };
        let in_chip = self
            .mention_hits
            .iter()
            .any(|hit| hit.target == target && hit.bounds.contains(&pointer));
        if mention_tooltip_contains(in_chip, popup.contains(&pointer)) {
            self.mention_tooltip_popup = Some(popup);
            true
        } else {
            self.invalidate_mention_tooltip();
            false
        }
    }

    // ---- undo history ----

    pub(super) fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    /// Called with the range about to be replaced, BEFORE the content changes,
    /// so the pushed snapshot is the pre-edit state.
    pub(super) fn record_edit(&mut self, range: &Range<usize>, new_text: &str) {
        let kind = if new_text.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Insert
        };
        // A run merges only while it stays single-character, contiguous with
        // the previous edit, of the same kind, and inside the idle window. A
        // pause, a word break, a paste, or a caret jump all break the run so
        // undo lands on a boundary the user recognizes.
        let mergeable = match (kind, &self.last_edit) {
            (EditKind::Insert, Some((EditKind::Insert, at, when))) => {
                range.is_empty()
                    && range.start == *at
                    && new_text.chars().count() == 1
                    && !new_text.starts_with(['\n', ' ', '\t'])
                    && when.elapsed() < UNDO_COALESCE
            }
            (EditKind::Delete, Some((EditKind::Delete, at, when))) => {
                range.end == *at && when.elapsed() < UNDO_COALESCE
            }
            _ => false,
        };
        if !mergeable {
            self.undo_stack.push(self.snapshot());
            if self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.remove(0);
            }
        }
        // Any fresh edit invalidates the redo branch.
        self.redo_stack.clear();
        let tail = match kind {
            EditKind::Insert => range.start + new_text.len(),
            EditKind::Delete => range.start,
        };
        self.last_edit = Some((kind, tail, Instant::now()));
    }

    pub(super) fn restore(&mut self, snapshot: EditSnapshot, cx: &mut Context<Self>) {
        self.invalidate_mention_tooltip();
        self.content = snapshot.content;
        self.refresh_projection();
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.marked_range = None;
        self.follow_cursor = true;
        // Never merge a subsequent edit into a step that undo just crossed.
        self.last_edit = None;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    pub(super) fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.undo_stack.pop() else {
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore(previous, cx);
    }

    pub(super) fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push(self.snapshot());
        self.restore(next, cx);
    }

    // ---- editing ops ----

    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(super) fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.projection.normalize_range(offset..offset).start;
        self.selected_range = offset..offset;
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::CursorMoved);
        cx.notify();
    }

    pub(super) fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.projection.normalize_range(offset..offset).start;
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::CursorMoved);
        cx.notify();
    }

    pub(super) fn previous_boundary(&self, offset: usize) -> usize {
        if let Some(boundary) = self.projection.previous_boundary(offset) {
            return boundary;
        }
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    pub(super) fn next_boundary(&self, offset: usize) -> usize {
        if let Some(boundary) = self.projection.next_boundary(offset) {
            return boundary;
        }
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }

    pub(super) fn previous_word_boundary(&self, offset: usize) -> usize {
        if let Some(boundary) = self.projection.previous_boundary(offset) {
            return boundary;
        }
        self.content
            .split_word_bound_indices()
            .rev()
            .find_map(|(ix, word)| (ix < offset && !word.trim().is_empty()).then_some(ix))
            .unwrap_or(0)
    }

    pub(super) fn next_word_boundary(&self, offset: usize) -> usize {
        if let Some(boundary) = self.projection.next_boundary(offset) {
            return boundary;
        }
        self.content
            .split_word_bound_indices()
            .find_map(|(ix, word)| {
                let end = ix + word.len();
                (end > offset && !word.trim().is_empty()).then_some(end)
            })
            .unwrap_or(self.content.len())
    }

    /// Byte range of the logical line containing `offset`.
    pub(super) fn line_range_at(&self, offset: usize) -> Range<usize> {
        let start = self.content[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len());
        start..end
    }

    pub(super) fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub(super) fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub(super) fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.move_to(prev, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    pub(super) fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.selected_range.end);
            self.move_to(next, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    pub(super) fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.mention_has_selection {
            cx.emit(ComposerInputEvent::MentionNavigate(-1));
            return;
        }
        if let Some(ix) = self.vertical_target(-1.0) {
            self.move_to(ix, cx);
        }
    }

    pub(super) fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.mention_has_selection {
            cx.emit(ComposerInputEvent::MentionNavigate(1));
            return;
        }
        if let Some(ix) = self.vertical_target(1.0) {
            self.move_to(ix, cx);
        }
    }

    pub(super) fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.vertical_target(-1.0) {
            self.select_to(ix, cx);
        }
    }

    pub(super) fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.vertical_target(1.0) {
            self.select_to(ix, cx);
        }
    }

    /// Offset one wrapped line above/below the cursor, keeping its x column.
    /// Clamps to the document edges, matching the platform's behavior on the
    /// first and last line.
    pub(super) fn vertical_target(&self, dir: f32) -> Option<usize> {
        let current = self.point_for_index(self.cursor_offset())?;
        let target_y = f32::from(current.y) + dir * f32::from(self.line_height);
        if target_y < 0.0 {
            return Some(0);
        }
        if target_y >= self.content_height {
            return Some(self.content.len());
        }
        Some(self.index_for_point(point(current.x, px(target_y))))
    }

    pub(super) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    pub(super) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    pub(super) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    pub(super) fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.start, cx);
    }

    pub(super) fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.end, cx);
    }

    pub(super) fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.select_to(line.start, cx);
    }

    pub(super) fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.select_to(line.end, cx);
    }

    pub(super) fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    pub(super) fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    pub(super) fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    pub(super) fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    pub(super) fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.previous_word_boundary(self.cursor_offset());
        self.move_to(prev, cx);
    }

    pub(super) fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_word_boundary(self.cursor_offset());
        self.move_to(next, cx);
    }

    pub(super) fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.previous_word_boundary(self.cursor_offset());
        self.select_to(prev, cx);
    }

    pub(super) fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_word_boundary(self.cursor_offset());
        self.select_to(next, cx);
    }

    /// Opt/Cmd + Delete family. With a live selection these delete the
    /// selection only (platform behavior) — the extend runs off the cursor.
    pub(super) fn delete_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            if self.cursor_offset() == offset {
                return;
            }
            self.select_to(offset, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = self.previous_word_boundary(self.cursor_offset());
        self.delete_to(prev, window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.next_word_boundary(self.cursor_offset());
        self.delete_to(next, window, cx);
    }

    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = self.line_range_at(self.cursor_offset()).start;
        self.delete_to(start, window, cx);
    }

    fn delete_to_line_end(
        &mut self,
        _: &DeleteToLineEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.line_range_at(self.cursor_offset()).end;
        self.delete_to(end, window, cx);
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        } else if let Some(text) = crate::markdown::selection::selected_text() {
            // The composer keeps focus while the user reads the transcript —
            // Cmd+C with no input selection copies the markdown selection.
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub(super) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    pub(super) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // Image data (or copied files) beats text — the original composer's
        // onPaste prevents the default text insert when `clipboardData.files`
        // is non-empty and stages the images instead.
        let mut images: Vec<gpui::Image> = Vec::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => images.push(image.clone()),
                ClipboardEntry::ExternalPaths(files) => {
                    paths.extend(files.paths().iter().cloned());
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if !images.is_empty() {
            cx.emit(ComposerInputEvent::PastedImages(images));
            return;
        }
        if !paths.is_empty() {
            cx.emit(ComposerInputEvent::PastedPaths(paths));
            return;
        }
        if let Some(text) = item.text() {
            // Multiline input: newlines are welcome (unlike the single-line example).
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    pub(super) fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    pub(super) fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(if self.mention_has_selection {
            ComposerInputEvent::MentionAccept
        } else {
            ComposerInputEvent::Submitted
        });
    }

    pub(super) fn mention_tab(&mut self, _: &MentionTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.mention_has_selection {
            cx.emit(ComposerInputEvent::MentionAccept);
        } else {
            cx.propagate();
        }
    }

    pub(super) fn mention_escape(&mut self, _: &MentionEscape, _: &mut Window, cx: &mut Context<Self>) {
        if self.mention_open {
            cx.emit(ComposerInputEvent::MentionDismiss);
        } else {
            cx.propagate();
        }
    }

    // ---- geometry ----

    /// Content-local point for a byte index (y grows down from content top).
    pub(super) fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        self.point_for_display_index(self.projection.raw_to_display(index))
    }

    /// Content-local point for a shaped projection byte index. The icon layer
    /// uses this to occupy its explicit projection slot without inventing a
    /// second coordinate system beside the custom text editor.
    pub(super) fn point_for_display_index(&self, index: usize) -> Option<Point<Pixels>> {
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let line_start = *self.line_starts.get(line_ix)?;
            let line_len = line.len();
            if index < line_start {
                continue;
            }
            if index <= line_start + line_len {
                let local = line.position_for_index(index - line_start, self.line_height)?;
                let y_offset: f32 = self
                    .last_lines
                    .iter()
                    .take(line_ix)
                    .map(|l| f32::from(l.size(self.line_height).height))
                    .sum();
                return Some(point(local.x, local.y + px(y_offset)));
            }
        }
        None
    }

    /// Content-local boxes occupied by a projected byte range, split at every
    /// soft wrap. A caret exactly at a wrap boundary belongs visually to both
    /// rows in GPUI; using the explicit wrap indices lets the range's first
    /// glyph start at x=0 on the new row instead of inheriting the old row's
    /// end caret (which previously caused mention washes to be discarded).
    pub(super) fn bounds_for_display_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut bounds = Vec::new();
        let mut y_offset = px(0.0);
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let line_start = self.line_starts.get(line_ix).copied().unwrap_or(0);
            let local_start = range.start.saturating_sub(line_start).min(line.len());
            let local_end = range.end.saturating_sub(line_start).min(line.len());
            if local_start >= local_end
                || range.end <= line_start
                || range.start >= line_start + line.len()
            {
                y_offset += line.size(self.line_height).height;
                continue;
            }

            let row_ends = line
                .wrap_boundaries()
                .iter()
                .map(|boundary| line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index)
                .chain(std::iter::once(line.len()));
            for (row_ix, row_start, segment) in
                display_row_segments(local_start..local_end, row_ends)
            {
                let row_y = y_offset + self.line_height * row_ix;
                let start_x = if segment.start == row_start {
                    px(0.0)
                } else {
                    line.position_for_index(segment.start, self.line_height)
                        .map(|point| point.x)
                        .unwrap_or(px(0.0))
                };
                if let Some(end_point) = line.position_for_index(segment.end, self.line_height)
                    && end_point.x > start_x
                {
                    bounds.push(Bounds::new(
                        point(start_x, row_y),
                        size(end_point.x - start_x, self.line_height),
                    ));
                }
            }
            y_offset += line.size(self.line_height).height;
        }
        bounds
    }

    /// Byte index closest to a content-local point.
    pub(super) fn index_for_point(&self, position: Point<Pixels>) -> usize {
        if self.display_is_placeholder {
            return 0;
        }
        let mut y = f32::from(position.y);
        if y < 0.0 {
            return 0;
        }
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let height = f32::from(line.size(self.line_height).height);
            let line_start = self.line_starts.get(line_ix).copied().unwrap_or(0);
            if y < height || line_ix + 1 == self.last_lines.len() {
                let local = point(position.x, px(y.min(height - 1.0).max(0.0)));
                let ix = line
                    .closest_index_for_position(local, self.line_height)
                    .unwrap_or_else(|ix| ix);
                return self
                    .projection
                    .display_to_raw((line_start + ix).min(self.projection.display.len()));
            }
            y -= height;
        }
        self.content.len()
    }

    pub(super) fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let local = point(
            position.x - bounds.left(),
            position.y - bounds.top() + px(self.scroll_top),
        );
        self.index_for_point(local)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_mention_tooltip();
        window.focus(&self.focus_handle, cx);
        let intent = press_intent(event.click_count, event.modifiers.shift);
        self.is_selecting = intent.arms_drag();
        self.drag_position = intent.arms_drag().then_some(event.position);
        self.drag_generation = self.drag_generation.wrapping_add(1);
        self.drag_autoscroll_active = false;
        match intent {
            PressIntent::SelectAll => {
                self.move_to(0, cx);
                self.select_to(self.content.len(), cx);
            }
            PressIntent::ExtendSelection => {
                let index = self.index_for_mouse_position(event.position);
                self.select_to(index, cx);
            }
            PressIntent::PlaceCaret => {
                let index = self.index_for_mouse_position(event.position);
                self.move_to(index, cx);
            }
        }
    }

    pub(super) fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
        self.drag_position = None;
        self.drag_generation = self.drag_generation.wrapping_add(1);
        self.drag_autoscroll_active = false;
    }

    pub(super) fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        self.on_mention_pointer_move(event.position, cx);
        if self.is_selecting {
            self.drag_position = Some(event.position);
            let position = self.drag_selection_position(event.position);
            self.select_to(self.index_for_mouse_position(position), cx);
            if self.drag_scroll_delta(event.position) != 0.0 && !self.drag_autoscroll_active {
                self.start_drag_autoscroll(cx);
            }
        }
    }

    pub(super) fn start_drag_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.drag_autoscroll_active = true;
        let generation = self.drag_generation;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(DRAG_SCROLL_FRAME_MS))
                    .await;
                let keep_running = this
                    .update(cx, |input, cx| input.drag_autoscroll_tick(generation, cx))
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn drag_selection_position(&self, position: Point<Pixels>) -> Point<Pixels> {
        let Some(bounds) = self.last_bounds else {
            return position;
        };
        point(
            position.x.clamp(bounds.left(), bounds.right() - px(0.5)),
            position.y.clamp(bounds.top(), bounds.bottom() - px(0.5)),
        )
    }

    pub(super) fn drag_scroll_delta(&self, position: Point<Pixels>) -> f32 {
        let Some(bounds) = self.last_bounds else {
            return 0.0;
        };
        input_drag_scroll_delta(
            f32::from(position.y),
            f32::from(bounds.top()),
            f32::from(bounds.bottom()),
            f32::from(self.line_height),
        )
    }

    pub(super) fn drag_autoscroll_tick(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if !self.is_selecting || self.drag_generation != generation {
            return false;
        }
        let (Some(position), Some(bounds)) = (self.drag_position, self.last_bounds) else {
            self.drag_autoscroll_active = false;
            return false;
        };
        let delta = self.drag_scroll_delta(position);
        if delta == 0.0 {
            self.drag_autoscroll_active = false;
            return false;
        }
        let next = (self.scroll_top + delta).clamp(
            0.0,
            input_max_scroll(self.content_height, f32::from(bounds.size.height)),
        );
        if next == self.scroll_top {
            self.drag_autoscroll_active = false;
            return false;
        }
        self.scroll_top = next;
        let edge_position = self.drag_selection_position(position);
        self.select_to(self.index_for_mouse_position(edge_position), cx);
        // Selection motion normally resumes caret following. During an edge
        // drag the autoscroll loop owns the viewport instead.
        self.follow_cursor = false;
        true
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.last_bounds else {
            return;
        };
        let viewport_height = f32::from(bounds.size.height);
        let delta_y = f32::from(event.delta.pixel_delta(self.line_height).y);
        let next = input_scroll_offset(
            self.scroll_top,
            delta_y,
            self.content_height,
            viewport_height,
        );
        if next == self.scroll_top {
            // Overscroll guard: when the input itself is scrollable (content
            // taller than the viewport), swallow the wheel event even at the
            // scroll boundary so it never chains into the outer transcript
            // list (the native equivalent of `overscroll-behavior: contain`).
            if delta_y != 0.0 && input_max_scroll(self.content_height, viewport_height) > 0.0 {
                cx.stop_propagation();
            }
            return;
        }
        self.invalidate_mention_tooltip();
        self.scroll_top = next;
        self.follow_cursor = false;
        cx.stop_propagation();
        cx.emit(ComposerInputEvent::ViewportChanged);
        cx.notify();
    }

    // ---- utf16 mapping (IME) ----

    pub(super) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    pub(super) fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    pub(super) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(super) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Shape the text at a width; store measured layout; return content height.
    /// Called from the element's measured-layout closure.
    fn layout_text(
        &mut self,
        width: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> f32 {
        // Rebuild this even for an empty draft. Otherwise deleting the final
        // mention can leave its previous paint geometry alive while the
        // placeholder is already being shaped, tinting "Do anything" for a
        // frame (or longer when no subsequent layout is requested).
        self.refresh_projection();
        let (display, is_placeholder) = if self.content.is_empty() {
            (self.placeholder.clone(), true)
        } else {
            (SharedString::from(self.projection.display.clone()), false)
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        self.line_height = px(INPUT_LINE_HEIGHT);

        // Chips read as inline code: the markdown renderer's recipe (mono font
        // + the spectrum's `code_text`) over the rounded `code_wash` beneath.
        let (chip_font, chip_color) = {
            let theme = Theme::of(cx);
            (gpui::font(theme.font_mono.clone()), theme.code_text)
        };
        let run_for = |len: usize, underline: bool, chip: bool| TextRun {
            len,
            font: if chip {
                chip_font.clone()
            } else {
                style.font()
            },
            color: if chip { chip_color } else { style.color },
            // Rounded mention washes are painted explicitly beneath the text;
            // TextRun backgrounds are square and can disappear in wrapped runs.
            background_color: None,
            underline: underline.then_some(UnderlineStyle {
                color: Some(style.color),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: None,
        };
        let runs: Vec<TextRun> = match self.marked_range.as_ref() {
            Some(marked) if !is_placeholder => {
                let start = self.projection.raw_to_display(marked.start);
                let end = self.projection.raw_to_display(marked.end);
                vec![
                    run_for(start, false, false),
                    run_for(end.saturating_sub(start), true, false),
                    run_for(display.len() - end, false, false),
                ]
                .into_iter()
                .filter(|r| r.len > 0)
                .collect()
            }
            _ if is_placeholder => vec![run_for(display.len(), false, false)],
            _ => {
                let mut runs = Vec::new();
                let mut at = 0;
                for (_, chip) in &self.projection.mentions {
                    if at < chip.start {
                        runs.push(run_for(chip.start - at, false, false));
                    }
                    runs.push(run_for(chip.len(), false, true));
                    at = chip.end;
                }
                if at < display.len() {
                    runs.push(run_for(display.len() - at, false, false));
                }
                runs
            }
        };

        let lines = window
            .text_system()
            .shape_text(display, font_size, &runs, Some(width), None)
            .map(|small| small.into_vec())
            .unwrap_or_default();

        // Logical line byte offsets (each shaped line covers one \n-split line).
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        for line in &lines {
            line_starts.push(at);
            at += line.len() + 1; // + '\n'
        }
        if line_starts.is_empty() {
            line_starts.push(0);
        }

        let content_height: f32 = lines
            .iter()
            .map(|l| f32::from(l.size(self.line_height).height))
            .sum();
        let max_line_width: f32 = lines
            .iter()
            .map(|l| f32::from(l.unwrapped_layout.width))
            .fold(0.0, f32::max);

        self.display_is_placeholder = is_placeholder;
        self.last_lines = lines;
        self.line_starts = line_starts;
        self.content_height = content_height.max(INPUT_LINE_HEIGHT);
        self.max_line_width = if is_placeholder { 0.0 } else { max_line_width };
        self.last_width = f32::from(width);
        self.layout_epoch += 1;
        self.content_height
    }

    /// Keep the cursor visible when content exceeds the element height.
    pub(super) fn clamp_scroll(&mut self, element_height: f32) -> bool {
        let previous = self.scroll_top;
        if self.follow_cursor {
            if let Some(cursor) = self.point_for_index(self.cursor_offset()) {
                self.scroll_top = input_scroll_offset_for_cursor(
                    self.scroll_top,
                    f32::from(cursor.y),
                    f32::from(self.line_height),
                    self.content_height,
                    element_height,
                );
            }
        }
        self.scroll_top = self
            .scroll_top
            .clamp(0.0, input_max_scroll(self.content_height, element_height));
        self.scroll_top != previous
    }
}

impl EventEmitter<ComposerInputEvent> for ComposerInput {}

impl Focusable for ComposerInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for ComposerInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self
            .projection
            .normalize_range(self.range_from_utf16(&range_utf16));
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.selected_range = self.projection.normalize_range(self.selected_range.clone());
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = self.projection.normalize_range(range);
        self.invalidate_mention_tooltip();
        // An IME commit is the tail of a composition whose pre-composition
        // snapshot was already taken (`replace_and_mark_text_in_range`);
        // recording here would pin undo to the half-composed text instead.
        if self.marked_range.is_none() {
            self.record_edit(&range, new_text);
        }
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        self.refresh_projection();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = self.projection.normalize_range(range);
        self.invalidate_mention_tooltip();
        // First keystroke of a composition: snapshot the text as it stood
        // before any of it existed, so one undo drops the whole composition.
        if self.marked_range.is_none() {
            self.undo_stack.push(self.snapshot());
            if self.undo_stack.len() > UNDO_LIMIT {
                self.undo_stack.remove(0);
            }
            self.redo_stack.clear();
            self.last_edit = None;
        }
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        self.refresh_projection();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.follow_cursor = true;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self
            .projection
            .normalize_range(self.range_from_utf16(&range_utf16));
        let start = self.point_for_index(range.start)?;
        let origin = point(
            bounds.left() + start.x,
            bounds.top() + start.y - px(self.scroll_top),
        );
        Some(Bounds::new(origin, size(px(2.0), self.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point_in_window: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for_mouse_position(point_in_window);
        Some(self.offset_to_utf16(index))
    }
}

/// The custom element: measured auto-grow layout + shaped-line painting.
pub(super) struct ComposerTextElement {
    pub(super) input: Entity<ComposerInput>,
    /// Max content height before internal scrolling kicks in.
    pub(super) max_content_height: f32,
}

pub(super) struct MentionPathTooltip {
    pub(super) path: SharedString,
    /// Stable for one `Waiting → Visible` promotion; a later activation gets
    /// a new key and therefore exactly one fresh fade-in.
    pub(super) activation: u64,
}

impl Render for MentionPathTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        motion::fade_quick(
            ("file-mention-path-tooltip", self.activation),
            div()
                .h(px(MENTION_TOOLTIP_HEIGHT))
                .max_w(px(480.0))
                .flex()
                .items_center()
                .px(px(8.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface_raised)
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(self.path.clone()),
        )
    }
}

pub(super) struct ComposerTextPrepaint {
    pub(super) cursor: Option<PaintQuad>,
    pub(super) mention_quads: Vec<PaintQuad>,
    pub(super) mention_hits: Vec<MentionHit>,
    pub(super) selection_quads: Vec<PaintQuad>,
    /// Completion preview: window-space origin of the end-of-text caret plus
    /// the suffix to paint there (shaped at paint time — it never joins the
    /// content's own layout, so hit-testing and the caret ignore it).
    pub(super) ghost: Option<(Point<Pixels>, SharedString)>,
}

impl IntoElement for ComposerTextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for ComposerTextElement {
    type RequestLayoutState = ();
    type PrepaintState = ComposerTextPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        let input = self.input.clone();
        let text_style = window.text_style();
        let max_content = self.max_content_height;
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.unwrap_or(match available.width {
                    gpui::AvailableSpace::Definite(width) => width,
                    _ => px(320.0),
                });
                let content_height = input.update(cx, |input, cx| {
                    input.layout_text(width, &text_style, window, cx)
                });
                size(width, px(content_height.min(max_content)))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.input.update(cx, |input, cx| {
            let scrolled = input.clamp_scroll(f32::from(bounds.size.height));
            input.last_bounds = Some(bounds);
            if scrolled {
                cx.emit(ComposerInputEvent::ViewportChanged);
            }
        });
        let input = self.input.read(cx);
        let scroll = px(input.scroll_top);
        let origin = point(bounds.left(), bounds.top() - scroll);
        let selection_color = Theme::of(cx).selection;
        let caret_color = Theme::of(cx).caret;
        // The inline-code recipe: chips use the spectrum wash like `code` spans.
        let mention_color = Theme::of(cx).code_wash;

        let mut mention_quads = Vec::new();
        let mut mention_hits = Vec::new();
        for (mention, display) in &input.projection.mentions {
            let target = MentionTooltipTarget {
                range: mention.range.clone(),
                path: SharedString::from(format!(
                    "{}{}",
                    mention.path,
                    if mention.is_dir { "/" } else { "" }
                )),
            };
            for local_bounds in input.bounds_for_display_range(display.clone()) {
                let chip_bounds = Bounds::new(
                    point(
                        origin.x + local_bounds.origin.x,
                        origin.y + local_bounds.origin.y + px(2.0),
                    ),
                    size(local_bounds.size.width, local_bounds.size.height - px(4.0)),
                );
                mention_quads.push(quad(
                    chip_bounds,
                    px(5.0),
                    mention_color,
                    px(0.0),
                    gpui::transparent_black(),
                    BorderStyle::default(),
                ));
                let above_anchor = chip_bounds.top() - px(MENTION_TOOLTIP_HEIGHT) - px(1.0);
                let anchor_y = if above_anchor >= px(0.0) {
                    above_anchor
                } else {
                    // GPUI positions at anchor + 1px; subtracting one keeps the
                    // below fallback flush so the pointer can enter the popup.
                    chip_bounds.bottom() - px(1.0)
                };
                let visible_bounds = chip_bounds.intersect(&bounds);
                if visible_bounds.size.width == px(0.0) || visible_bounds.size.height == px(0.0) {
                    continue;
                }
                mention_hits.push(MentionHit {
                    target: target.clone(),
                    bounds: visible_bounds,
                    // The fixed-height popup starts at anchor + 1px. Moving
                    // the anchor above the chip therefore yields conventional
                    // above-target placement without cursor tracking.
                    anchor: point(chip_bounds.left(), anchor_y),
                });
            }
        }
        let mut selection_quads = Vec::new();
        let mut cursor = None;
        if input.selected_range.is_empty() || input.display_is_placeholder {
            if let Some(p) = input.point_for_index(input.cursor_offset()) {
                cursor = Some(fill(
                    Bounds::new(
                        point(origin.x + p.x, origin.y + p.y),
                        size(px(2.0), input.line_height),
                    ),
                    caret_color,
                ));
            } else if input.display_is_placeholder {
                cursor = Some(fill(
                    Bounds::new(origin, size(px(2.0), input.line_height)),
                    caret_color,
                ));
            }
        } else if let (Some(start), Some(end)) = (
            input.point_for_index(input.selected_range.start),
            input.point_for_index(input.selected_range.end),
        ) {
            let lh = input.line_height;
            if start.y == end.y {
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(origin.x + end.x, origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
            } else {
                // First visual row, full middle rows, last visual row.
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(bounds.right(), origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
                if end.y > start.y + lh {
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            point(origin.x, origin.y + start.y + lh),
                            point(bounds.right(), origin.y + end.y),
                        ),
                        selection_color,
                    ));
                }
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x, origin.y + end.y),
                        point(origin.x + end.x, origin.y + end.y + lh),
                    ),
                    selection_color,
                ));
            }
        }
        let tooltip = input.visible_mention_tooltip();
        if let Some((_target, anchor, _activation, view)) = tooltip {
            let view = view.into();
            let input = self.input.clone();
            window.set_tooltip(AnyTooltip {
                view,
                mouse_position: anchor,
                check_visible_and_update: Rc::new(move |popup, window, cx| {
                    input.update(cx, |input, _| {
                        input.check_mention_tooltip_visibility(popup, window.mouse_position())
                    })
                }),
            });
        }
        // The ghost only shows where accepting it would insert: a collapsed
        // caret at the end of real (non-placeholder, non-IME) text.
        let ghost = input
            .ghost
            .clone()
            .filter(|g| {
                !g.is_empty()
                    && !input.display_is_placeholder
                    && input.marked_range.is_none()
                    && input.selected_range.is_empty()
                    && input.cursor_offset() == input.content.len()
            })
            .and_then(|g| {
                input
                    .point_for_index(input.content.len())
                    .map(|p| (point(origin.x + p.x, origin.y + p.y), g))
            });
        ComposerTextPrepaint {
            cursor,
            mention_quads,
            mention_hits,
            selection_quads,
            ghost,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        self.input.update(cx, |input, _| {
            input.set_mention_hits(prepaint.mention_hits.clone())
        });
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                input.update(cx, |input, cx| input.on_mouse_move(event, cx));
            }
        });

        // WrappedLine isn't Clone — temporarily take the shaped lines out of the
        // entity for painting, then put them back for mouse mapping.
        let (lines, line_height, scroll) = self.input.update(cx, |input, _| {
            (
                std::mem::take(&mut input.last_lines),
                input.line_height,
                input.scroll_top,
            )
        });

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.mention_quads.drain(..) {
                window.paint_quad(quad);
            }
            for quad in prepaint.selection_quads.drain(..) {
                window.paint_quad(quad);
            }
            let mut y = bounds.top() - px(scroll);
            for line in &lines {
                let height = line.size(line_height).height;
                let _ = line.paint(
                    point(bounds.left(), y),
                    line_height,
                    gpui::TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                );
                y += height;
            }
            if let Some((ghost_origin, ghost)) = prepaint.ghost.take() {
                let style = window.text_style();
                let font_size = style.font_size.to_pixels(window.rem_size());
                let run = TextRun {
                    len: ghost.len(),
                    font: style.font(),
                    color: Theme::of(cx).text_faint,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let line = window
                    .text_system()
                    .shape_line(ghost, font_size, &[run], None);
                // (Clipping comes from the surrounding content mask.)
                let _ = line.paint(
                    ghost_origin,
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            // Caret only when this input is actually focused in an active
            // window (Electron hides it on window deactivation too), and only
            // in the "on" blink phase — solid while typing, ~500ms blink idle.
            if self
                .input
                .update(cx, |input, cx| input.caret_shown(window, cx))
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_lines = lines;
        });
    }
}

impl Render for ComposerInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let text_color = if self.content.is_empty() {
            theme.text_faint
        } else {
            theme.text
        };
        div()
            .key_context(self.key_context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::mention_tab))
            .on_action(cx.listener(Self::mention_escape))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .w_full()
            .text_size(crate::typography::ui_rems(INPUT_TEXT_SIZE))
            .line_height(px(INPUT_LINE_HEIGHT))
            .text_color(text_color)
            .font_family(theme.font_sans.clone())
            .child(ComposerTextElement {
                input: cx.entity(),
                // Internal scrolling once content exceeds the 260px textarea
                // box minus its `pt-4 pb-1` padding.
                max_content_height: TEXTAREA_MAX - TEXTAREA_PAD_V,
            })
    }
}

// ---------------------------------------------------------------------------
// Composer wrapper
// ---------------------------------------------------------------------------

/// Events the shell listens for.
#[derive(Debug, Clone)]
pub enum ComposerEvent {
    /// A prompt was sent optimistically — give the transcript its exact row
    /// identity so it can anchor the prompt at the top with the reply's
    /// reserved space below it.
    Sent { chat_id: String, message_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MentionToken {
    pub(super) range: Range<usize>,
    pub(super) query: String,
}

/// The `@` must begin a token. This intentionally excludes `name@example.com`
/// and ordinary words while allowing punctuation such as `(@src`.
pub(super) fn mention_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let token_start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at + ch.len_utf8()))
        .unwrap_or(0);
    let Some(relative_at) = text[token_start..cursor].rfind('@') else {
        return None;
    };
    let at = token_start + relative_at;
    let valid_boundary = at == 0
        || text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'));
    if text[at + 1..cursor].contains('@') || !valid_boundary {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(cursor + at))
        .unwrap_or(text.len());
    Some(MentionToken {
        range: at..end,
        query: text[at + 1..cursor].to_string(),
    })
}

/// Restart a popup's row stack at the top (fresh open / query / result set).
pub(super) fn reset_scroll_offset(scroll: &gpui::ScrollHandle) {
    scroll.set_offset(gpui::Point::new(px(0.0), px(0.0)));
}

/// The `/` must open the input: slash commands are whole-prompt prefixes
/// (`/compact`, `/goal ship it`), so only the first token triggers, and a
/// query containing another `/` (a typed path) never does.
pub(super) fn slash_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) || !text.starts_with('/') {
        return None;
    }
    let end = text
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at))
        .unwrap_or(text.len());
    // Cursor outside the command token (typing the argument): popup closed.
    if cursor == 0 || cursor > end {
        return None;
    }
    let query = &text[1..cursor];
    if query.contains('/') {
        return None;
    }
    Some(MentionToken {
        range: 0..end,
        query: query.to_string(),
    })
}

/// Slash-command completion state: like [`FileMentionState`] but the
/// candidate list is fetched once per harness (`ListCommands`) and filtered
/// locally per keystroke — no RPC, debounce, or skeleton churn while typing.
#[derive(Debug, Clone, Default)]
pub(super) struct SlashState {
    pub(super) token: Option<MentionToken>,
    /// Indices into the cached command list, filter-ranked for the query.
    pub(super) filtered: Vec<usize>,
    pub(super) active: Option<usize>,
    /// Harness the popup is showing commands for (cache key).
    pub(super) harness: Option<HarnessId>,
    pub(super) request: u64,
    pub(super) loading: bool,
    pub(super) error: Option<SharedString>,
    pub(super) dismissed: Option<(Range<usize>, String)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FileMentionState {
    pub(super) token: Option<MentionToken>,
    pub(super) results: Vec<FileSearchMatch>,
    pub(super) active: Option<usize>,
    pub(super) request: u64,
    pub(super) loading: bool,
    /// Why the last search failed, for the popup. A failure MUST NOT render
    /// as "No matching files": cross-device searches fail for reasons the
    /// user can act on (host daemon too old for `SearchFiles`, device
    /// offline), and the empty state hid them (user report).
    pub(super) error: Option<SharedString>,
    /// Full token text, not just the cursor-relative query: moving within a
    /// dismissed token keeps it closed, while any edit re-enables completion.
    pub(super) dismissed: Option<(Range<usize>, String)>,
}

pub(super) fn mention_response_is_current(state: &FileMentionState, request: u64) -> bool {
    state.request == request && state.token.is_some()
}

/// A failed file search, translated for the popup. `UnknownMethod` is the
/// version-skew case: `SearchFiles` shipped after v0.1.9, so a session hosted
/// by a device on an older daemon answers "unknown method" while the same
/// search works for local sessions.
pub(super) fn mention_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "The session's device runs an older zeron — update it to search its files".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The session's device is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => "File search failed".into(),
    }
}

/// A failed command discovery, translated for the popup.
pub(super) fn slash_error_message(err: &RpcError) -> SharedString {
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
