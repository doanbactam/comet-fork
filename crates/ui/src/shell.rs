//! The app shell (zeron `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is zeron's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360px floor, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Empty, Entity, Focusable as _, IntoElement,
    KeyBinding, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, SharedString, Subscription, Task, Window, WindowControlArea, actions,
    div, prelude::*, px,
};

use gpui_tokio::Tokio;
use zeron_engine::InstanceLock;
use zeron_proto::{AuthState, WorkspaceScope};
use zeron_rpc::methods;

use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, RESIZE, SPLASH_OUT, SPLASH_OUT_QUICK, TAB_SLIDE};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::accounts::AccountsPage;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::harnesses::HarnessesPage;
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    self, JUMP_SLOTS, KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MIN, SIDEBAR_DEFAULT, SIDEBAR_MAX,
    SIDEBAR_MIN, SavePolicy, ShortcutId, SidebarOrganization, SidebarSort, TERMINAL_DEFAULT_HEIGHT,
    UiSettings, badge_combo, jump_hints_visible, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, EngineMode, GatePhase, Indicator, OrgRow,
    format_time_ago, org_name_valid, parse_orgs, sort_memberships,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, Transcript, TranscriptEvent};

mod spaces;
mod tabs;
mod layout;
mod nav;
mod titlebar;
mod chrome;
mod right_pane;
mod resize;
mod auth;
use auth::*;
use resize::*;
use chrome::*;

pub use layout::{resort_offsets, RESORT};
pub use nav::{ChatPanels, NavEntry, NavHistory, RightSurface, Route, SessionPanels, SettingsSection};
pub use titlebar::{
    caption_buttons_width, cluster_buttons_start, cluster_clearance, titlebar_cluster_start,
    titlebar_spacer_width, CLUSTER_BUTTONS_WIDTH, TITLEBAR_ACTION_EDGE_INSET,
    TITLEBAR_ACTION_SLOT_WIDTH, TITLEBAR_CONTROL_GAP, TITLEBAR_GROUP_GAP, TITLEBAR_IDENTITY_GAP,
};
use layout::{
    conversation_width, right_pane_max_width, right_pane_takeover_width, right_panel_content_width,
    sidebar_key_order_changed, stable_panel_content_width, PANE_RESIZE_HITBOX_TOP,
};
use titlebar::{titlebar_new_session_alpha, TITLEBAR_CLUSTER_PAD};

use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(
    shell,
    [
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        NewSession,
        OpenSettings,
        NextSession,
        PrevSession,
        ArchiveSession
    ]
);

#[derive(Clone, Copy)]
enum ChatMenuPage {
    Root,
    Copy,
}

#[derive(Clone)]
struct ChatMenuState {
    chat_id: String,
    position: Point<Pixels>,
    page: ChatMenuPage,
}

/// Interruptible height tween for the sidebar's device/archive disclosures.
/// The rendered element owns the frame clock; this state preserves the current
/// interpolated height when a second click reverses an in-flight transition.
#[derive(Clone, Copy)]
pub(super) struct SidebarDisclosureMotion {
    pub(super) epoch: u64,
    pub(super) from: f32,
    pub(super) to: f32,
    started: std::time::Instant,
}

impl SidebarDisclosureMotion {
    fn new(epoch: u64, from: f32, to: f32) -> Self {
        Self {
            epoch,
            from,
            to,
            started: std::time::Instant::now(),
        }
    }

    fn current(self) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32();
        let raw = if total > 0.0 {
            self.started.elapsed().as_secs_f32() / total
        } else {
            1.0
        };
        motion::lerp(self.from, self.to, motion::COLLAPSE.progress(raw))
    }

    fn animating(self) -> bool {
        self.started.elapsed() < motion::COLLAPSE.total() + spaces::SIDEBAR_DISCLOSURE_TWEEN_GRACE
    }
}

/// Open the session at `slot` (zero-based) of the sidebar's active list. One
/// action carrying the slot, rather than nine near-identical action types.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct JumpSession(pub usize);

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (Settings on every platform; ⌘Q quit, ⌘W
    // close, ⌘M minimize, ⌘H hide on macOS) — these back the native menu
    // key equivalents and must survive keymap re-application.
    crate::app_menus::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.new_session, "mod-n"),
            NewSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.next_session,
                crate::settings::ShortcutId::NextSession.default_combo(),
            ),
            NextSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.prev_session,
                crate::settings::ShortcutId::PrevSession.default_combo(),
            ),
            PrevSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.archive_session, "mod-shift-a"),
            ArchiveSession,
            None,
        ),
        // Fixed: ⌘K summons the add-space palette (the ⌘K chip in its search
        // bar); pressing it again dismisses.
        KeyBinding::new(&platform_combo("mod-k"), AddSpacePalette, None),
    ]);
    // ⌘1..⌘9 open the sidebar's first nine rows. A slot left unbound (an empty
    // combo in a hand-edited file) binds nothing rather than falling back —
    // the user cleared it on purpose.
    cx.bind_keys((0..JUMP_SLOTS).filter_map(|slot| {
        let id = ShortcutId::JumpSession(slot);
        let combo = keymap.get(id);
        if combo.is_empty() {
            return None;
        }
        Some(KeyBinding::new(
            &valid_or_default(combo, id.default_combo()),
            JumpSession(slot),
            None,
        ))
    }));
}

/// Exact active-session row height. Harness identity lives on the title line
/// and the Working glyph lives in the status corner, so neither adds a third
/// line. Compact rows omit the metadata line and its preceding gap entirely;
/// branch / pull-request rows add the exact height of their tallest child.
/// Keeping this calculation beside the renderer's metrics prevents disclosure
/// clips when view options alter the row structure.
pub(super) fn chat_row_height(shows_branch: bool, shows_pull_request: bool) -> f32 {
    let mut metadata_height: f32 = 0.0;
    if shows_branch {
        metadata_height = metadata_height.max(14.0);
    }
    if shows_pull_request {
        metadata_height = metadata_height.max(16.0);
    }
    if metadata_height == 0.0 {
        45.0
    } else {
        47.0 + metadata_height
    }
}
/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;
/// Harness/title geometry follows the row hierarchy: active multi-line cards
/// keep identity close on the standard 8px rhythm, while the one-line archived
/// shelf gives its larger mark a little more separation.
const SIDEBAR_ACTIVE_HARNESS_ICON_SIZE: f32 = 13.0;
const SIDEBAR_ACTIVE_HARNESS_TITLE_GAP: f32 = Theme::SPACE_SM;
const SIDEBAR_ARCHIVED_HARNESS_ICON_SIZE: f32 = 14.0;
const SIDEBAR_ARCHIVED_HARNESS_TITLE_GAP: f32 = 10.0;

/// Ramp height of the sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 24.0;

/// Drag marker for the sidebar resize handle.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    /// Full catalog splash-out (650ms).
    FadingOut,
    /// Fast-path splash-out when Ready arrived quickly (L2).
    FadingOutQuick,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// In-app update lifecycle (macOS bundle installs; see `render_update_strip`).
enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in — one click restarts into it.
    Ready(PathBuf),
    Failed(SharedString),
}

/// One right-pane subagent tab: the doc it shows, its strip title, and the
/// read-only transcript entity whose drop tears the view down.
struct SubagentTab {
    doc_id: String,
    title: SharedString,
    transcript: Entity<Transcript>,
    /// Keeps a frozen-blob fetch alive (it falls back to a live doc watch).
    _fetch: Option<Task<()>>,
    /// Spawn chips INSIDE the subagent transcript open their own tabs.
    _events: Subscription,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Measured height of the bottom chrome stack (status strip + composer +
    /// terminal dock) the full-height transcript scrolls under — written by a
    /// paint-time canvas each frame, read the NEXT frame for the fade inset,
    /// the transcript's bottom clearance, and the jump pill's anchor (the
    /// same one-frame lag every fade here rides).
    bottom_stack: std::rc::Rc<std::cell::Cell<f32>>,
    /// The sidebar's archived accordion (t3code Sidebar): OPEN by default
    /// (user request), session-transient. `archived_shown` pages the
    /// expanded list ("Show more" reveals another page).
    pub(super) archived_open: bool,
    pub(super) archived_shown: usize,
    /// Archived slim row under the pointer — swaps its time label for the
    /// Unarchive affordance and restores the dimmed harness mark (t3code's
    /// settled-row hover).
    pub(super) archived_hover: Option<String>,
    /// Ephemeral collapsed project/device sections, keyed by organization + id.
    pub(super) sidebar_collapsed_groups: std::collections::HashSet<String>,
    /// In-flight disclosure tweens, shared by device groups and Archived.
    pub(super) sidebar_disclosure_motion:
        std::collections::HashMap<String, SidebarDisclosureMotion>,
    /// The jump-hint overlay: true while the held modifiers exactly match a
    /// jump shortcut, which swaps the first nine rows' time-ago for their
    /// key-cap chip (t3code's `showJumpHints`). Frame-transient — window
    /// deactivation clears it, so a chip cannot stick after an app switch
    /// swallows the key-up.
    pub(super) jump_hints: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    /// Embedded terminal host for right-pane Terminal surfaces — a SEPARATE
    /// entity from the bottom drawer's (own PTYs, own grid geometry; one
    /// panel can only size one visible grid at a time).
    right_terminal: Option<Entity<TerminalPanel>>,
    /// The surface-tab strip's `+` menu (Terminal / Git diff rows).
    right_plus: popover::Popup<()>,
    /// Diff surfaces by id — each tab its own [`Changes`] viewer with its own
    /// scope/base pick and diff watch (multiple diff panels, user request).
    diffs: std::collections::HashMap<u64, Entity<Changes>>,
    /// Event hookups for [`Self::diffs`] (History rows opening commit tabs).
    diff_subs: std::collections::HashMap<u64, Subscription>,
    diff_seq: u64,
    /// Subagent transcript surfaces by id — each tab a read-only
    /// [`Transcript`] pinned to its subagent doc.
    subagent_tabs: std::collections::HashMap<u64, SubagentTab>,
    subagent_seq: u64,
    /// Ordered surface tabs per panel key (drag-reorderable; stale entries —
    /// closed terminals/diffs — are skipped at read time).
    right_tabs: std::collections::HashMap<String, Vec<RightSurface>>,
    /// In-flight surface-tab drag (slide animation state).
    right_tab_drag: Option<RightTabDragState>,
    /// Surface-tab strip scroll (the strip overflows horizontally, t3
    /// ScrollArea-style; drag drop-math reads the offset back out).
    right_tab_scroll: gpui::ScrollHandle,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    notifications_page: Option<Entity<NotificationsPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    harnesses_page: Option<Entity<HarnessesPage>>,
    shortcuts_sub: Option<Subscription>,
    notifications_sub: Option<Subscription>,
    /// Session-row context menu, including the Copy submenu.
    chat_menu: popover::Popup<ChatMenuState>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Space-row context menu (dropdown rows): (space id, window position).
    space_menu: popover::Popup<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// The sidebar's space-filter dropdown.
    spaces_menu: popover::Popup<spaces::SpacesMenu>,
    /// Persisted organization/sort/metadata controls beside the project filter.
    sidebar_view_menu: popover::Popup<spaces::SidebarViewMenu>,
    /// Natural-tab-order focus target for the icon-only view-options button.
    sidebar_view_trigger_focus: gpui::FocusHandle,
    /// Chat id whose STATUS CORNER is under the pointer — just that corner
    /// swaps to the archive button (t3code's settle-on-hover); hovering the
    /// row body leaves the status readable.
    chat_status_hover: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, zeron_proto::SessionStatus>,
    user_menu: popover::Popup<()>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Local lifecycle of an in-app update (macOS bundle swap) — the engine's
    /// UpdateStatus stream says WHETHER one exists; this says how far the
    /// download/stage of it has come in this process.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    /// Version whose update strip the user dismissed (advisory installs only —
    /// a newer release shows the strip again).
    update_dismissed: Option<String>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: zeron_update::InstallKind,
    org: Option<OrgGateUi>,
    sync_flow: SyncFlow,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    runtime_change_task: Option<Task<()>>,
    runtime_change_error: Option<SharedString>,
    /// The one-time local→synced import stream (switch wizard progress step).
    import_task: Option<Task<()>>,
    /// Title of the chat the import stream is copying right now.
    import_current: Option<SharedString>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`ZERON_OPEN_DIALOG`, `ZERON_FORCE_GATE`,
    /// `ZERON_DEMO_UPLOAD`) — see [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    debug_upload: Option<String>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    /// Mirrors `right_tween` only for takeover entry/exit, allowing the visible
    /// right-panel contents to resize with their outer frame in that mode.
    right_takeover_content_tween: Option<WidthTween>,
    /// Conversation-width tween used only while entering/leaving right-pane
    /// takeover. Normal right-pane open/close keeps the upstream flex behavior.
    main_takeover_tween: Option<WidthTween>,
    /// Changes-panel takeover (the header's expand button): the panel fills
    /// everything right of the sidebar and the conversation column collapses
    /// to zero. Session-local view state — never persisted, reset on close.
    right_pane_expanded: bool,
    /// Viewport width stamped each frame at render — the expanded panel's
    /// width target and the physical ceiling for free-form resizing
    /// ([`Self::right_target`] has no `Window`).
    viewport_width: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// The caption buttons zeron itself draws on Linux under client-side
    /// decorations, per side, already filtered to what the compositor
    /// supports — `None` off Linux or under server decorations (where the WM
    /// draws real buttons). Re-resolved every frame at the top of `render`.
    linux_captions: Option<gpui::WindowButtonLayout>,
    /// Re-renders when the desktop's button layout changes (GNOME
    /// `button-layout` gsetting). Registered on first paint — [`Shell::new`]
    /// has no window.
    button_layout_sub: Option<Subscription>,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    /// Process/window clock origin for adaptive splash (L2) and boot_stats.
    boot_started: std::time::Instant,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// Clears the jump hints when the window deactivates: a Cmd+Tab away
    /// swallows the key-up, so without this the chips stay on screen for good.
    activation_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    /// The primary transcript's spawn-chip events (subagent tabs).
    _transcript_events: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // Every send glides the prompt to the viewport top and reserves the
        // reply's space below it (notes-app parity).
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
            }
        });
        // Spawn chips open their subagent's transcript as a right-pane tab.
        let transcript_events = cx.subscribe(&transcript, Self::on_transcript_event);
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                            // The connection pill's retry countdown needs the
                            // same per-second refresh while degraded.
                            || matches!(
                                s.connectivity.state,
                                zeron_proto::ConnectivityState::Offline
                                    | zeron_proto::ConnectivityState::Reconnecting
                            )
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = settings::current(cx);
        state.update(cx, |state, cx| {
            state.set_change_requests_visible(settings.sidebar_show_pull_request, cx)
        });
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `ZERON_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("ZERON_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/harnesses") => Route::Settings(SettingsSection::Harnesses),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/notifications") => Route::Settings(SettingsSection::Notifications),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `ZERON_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `ZERON_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("ZERON_OPEN_DIALOG").ok();
        // `ZERON_DEMO_UPLOAD=<pct>:<image path>` fabricates an in-flight image
        // send on the selected chat (echo bubble + frozen thumbnail progress
        // ring) — display-only; a real upload can't be paused for a capture.
        let debug_upload = std::env::var("ZERON_DEMO_UPLOAD").ok();
        let debug_gate = match std::env::var("ZERON_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the zeron engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        Self {
            state,
            transcript,
            composer,
            // Seed with the compact composer stack's rough height so the
            // first frame's clearance isn't zero (the measure corrects it).
            bottom_stack: std::rc::Rc::new(std::cell::Cell::new(120.0)),
            archived_open: true,
            archived_shown: 0,
            archived_hover: None,
            sidebar_collapsed_groups: std::collections::HashSet::new(),
            sidebar_disclosure_motion: std::collections::HashMap::new(),
            jump_hints: false,
            terminal: None,
            right_terminal: None,
            right_plus: popover::Popup::default(),
            diffs: std::collections::HashMap::new(),
            diff_subs: std::collections::HashMap::new(),
            diff_seq: 0,
            subagent_tabs: std::collections::HashMap::new(),
            subagent_seq: 0,
            right_tabs: std::collections::HashMap::new(),
            right_tab_drag: None,
            right_tab_scroll: gpui::ScrollHandle::new(),
            route,
            nav,
            devices_page: None,
            archived_page: None,
            appearance_page: None,
            notifications_page: None,
            shortcuts_page: None,
            accounts_page: None,
            harnesses_page: None,
            shortcuts_sub: None,
            notifications_sub: None,
            chat_menu: popover::Popup::default(),
            rename_dialog: None,
            delete_confirm: None,
            space_menu: popover::Popup::default(),
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            spaces_menu: popover::Popup::default(),
            sidebar_view_menu: popover::Popup::default(),
            sidebar_view_trigger_focus: cx.focus_handle().tab_stop(true),
            chat_status_hover: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            user_menu: popover::Popup::default(),
            sidebar_notice: None,
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_dismissed: None,
            install: zeron_update::detect_install(),
            org: None,
            sync_flow: SyncFlow::Idle,
            mutate_task: None,
            auth_task: None,
            runtime_change_task: None,
            runtime_change_error: None,
            import_task: None,
            import_current: None,
            boot,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            debug_upload,
            sidebar_tween: None,
            right_tween: None,
            right_takeover_content_tween: None,
            main_takeover_tween: None,
            right_pane_expanded: false,
            viewport_width: 1280.0,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            linux_captions: None,
            button_layout_sub: None,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            boot_started: std::time::Instant::now(),
            focus_sub: None,
            activation_sub: None,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        if let Some(notice) = state.update(cx, |state, _| state.take_deep_link_notice()) {
            self.sidebar_notice = Some(notice.into());
        }
        let next_sync_flow = {
            let state = state.read(cx);
            sync_flow_after_auth(self.sync_flow, state.workspace_scope, state.auth.as_ref())
        };
        if next_sync_flow != self.sync_flow {
            self.sync_flow = next_sync_flow;
            if matches!(
                self.sync_flow,
                SyncFlow::RestartPending { .. } | SyncFlow::SwitchOffer { .. }
            ) {
                self.org = None;
            }
        }
        // The in-place local→synced switch: once the replacement runtime is
        // attached and Ready, kick the import (or finish) from here.
        self.drive_sync_switch(cx);
        let signed_out_synced = {
            let state = state.read(cx);
            state.workspace_scope == Some(WorkspaceScope::Synced)
                && matches!(state.auth, Some(AuthState::SignedOut))
        };
        // AuthStatus is shared by every viewport. Whichever viewport owns the
        // embedded runtime drains it; remote viewports request daemon shutdown
        // and all of them independently reattach to the new local runtime.
        if signed_out_synced && self.runtime_change_task.is_none() {
            self.start_local_runtime_transition(false, cx);
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Capture knob: `ZERON_DEMO_UPLOAD=<pct>:<image path>` — once a chat
        // is selected, push a fake sending echo carrying that image as a
        // pending attachment and freeze upload progress at <pct>, so the
        // thumbnail progress ring can be styled/screenshotted (a real upload
        // is too fast to pause).
        if let Some(spec) = self.debug_upload.clone()
            && let Some(chat_id) = state.read(cx).selected_chat.clone()
        {
            self.debug_upload = None;
            if let Some((pct, img_path)) = spec.split_once(':')
                && let Ok(pct) = pct.parse::<u64>()
                && let Ok(att) = crate::attachments::stage_file(std::path::Path::new(img_path))
            {
                let pending_path = format!("pending/{}/{}", att.id, att.name);
                let device_ids: Vec<String> = {
                    let s = state.read(cx);
                    s.selected_chat_row()
                        .map(|c| c.device_id.clone())
                        .into_iter()
                        .chain(s.local_device_id.clone())
                        .chain(Some("local".to_string()))
                        .collect()
                };
                for device_id in &device_ids {
                    crate::attachments::seed_attachment(
                        device_id,
                        &pending_path,
                        &att.name,
                        att.image.clone(),
                    );
                }
                let text = crate::attachments::with_attachments(
                    "Here is the screenshot of the bug.",
                    std::slice::from_ref(&pending_path),
                );
                let echo = zeron_doc::SessionMessageEntry {
                    id: "demo-upload-echo".into(),
                    role: zeron_doc::MessageRole::User,
                    parts: vec![zeron_doc::MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: chrono::Utc::now().timestamp_millis(),
                    device_id: "local".into(),
                    status: None,
                    continuation_of: None,
                };
                state.update(cx, |s, cx| {
                    s.push_echo(&chat_id, echo);
                    s.begin_upload_progress(
                        100,
                        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pct)),
                    );
                    cx.notify();
                });
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent. Desktop banners
        // (`notify::post`) ride the SAME edges and gates behind their own
        // settings flag — one detector, two outputs, so the banner can never
        // fire where the chime wouldn't.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        //
        // SEND-PENDING-GATED too (`AppState::send_pending`): a send whose
        // queued command the host hasn't executed yet can still surface a
        // phantom Working→Idle (a stale Working row crossing the 45s gate on
        // the send's own re-render, or a late old Idle row) — the done-chime
        // stays quiet for that chat until the host acks, while the baseline
        // keeps tracking silently so the ghost edge never fires later. The
        // question chime is NOT gated: an instant AwaitingInput ack should
        // still ring.
        {
            let now = Utc::now();
            type Ping = (String, zeron_proto::SessionStatus, bool, Option<String>);
            let sessions: Vec<Ping> = {
                let state = state.read(cx);
                state
                    .sessions
                    .iter()
                    .map(|s| {
                        use zeron_proto::view::Indicator;
                        let status = match zeron_proto::view::effective_indicator(Some(s), now) {
                            Indicator::Working => zeron_proto::SessionStatus::Working,
                            Indicator::AwaitingInput => zeron_proto::SessionStatus::AwaitingInput,
                            Indicator::Errored => zeron_proto::SessionStatus::Errored,
                            Indicator::None => zeron_proto::SessionStatus::Idle,
                        };
                        let send_pending = state.send_pending(&s.chat_id, now);
                        let title = state
                            .chats
                            .iter()
                            .find(|c| c.id == s.chat_id)
                            .and_then(|c| c.title.clone());
                        (s.chat_id.clone(), status, send_pending, title)
                    })
                    .collect()
            };
            // Background-only banners: `active_window()` is app-level (any
            // Zeron window being key), so a ping for a *background chat* in a
            // focused app still stays a chime — you're already looking at
            // Zeron; the sidebar dot carries the rest.
            let app_focused = cx.active_window().is_some();
            for (chat_id, status, send_pending, title) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                    && !(send_pending && sound == crate::sound::Sound::Done)
                {
                    if self.settings.sound_enabled {
                        crate::sound::play(sound);
                    }
                    if self.settings.notifications_enabled
                        && !(self.settings.notifications_background_only && app_focused)
                    {
                        let title = title.unwrap_or_else(|| "New session".into());
                        let body = match sound {
                            crate::sound::Sound::Done => "Run finished",
                            crate::sound::Sound::Request => "Waiting on your input",
                        };
                        crate::notify::post(&title, body);
                    }
                }
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // canvas defaults (project AND its device) follow it, even
                // over a remembered "no project" opt-out. Otherwise the last
                // selected project stands, unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(&exists);
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        self.settings.last_space_id.clone().filter(&exists)
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // zeron's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.right_takeover_content_tween = None;
            self.main_takeover_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                crate::boot_stats::mark_engine_ready();
                if self.splash == SplashPhase::Visible {
                    let ready_after = crate::boot_stats::elapsed_since_start()
                        .unwrap_or_else(|| self.boot_started.elapsed());
                    // L2: skip or shorten splash when the engine is already Ready.
                    // Reduced motion always skips the fade veil.
                    let reduced = motion::reduced_motion(cx);
                    let (next, hold) = if reduced || ready_after <= motion::SPLASH_SKIP_READY {
                        (SplashPhase::Gone, Duration::ZERO)
                    } else if ready_after <= motion::SPLASH_QUICK_READY {
                        (
                            SplashPhase::FadingOutQuick,
                            SPLASH_OUT_QUICK.total() + Duration::from_millis(30),
                        )
                    } else {
                        (
                            SplashPhase::FadingOut,
                            SPLASH_OUT.total() + Duration::from_millis(30),
                        )
                    };
                    self.splash = next;
                    if next == SplashPhase::Gone {
                        crate::boot_stats::mark_splash_gone();
                        cx.notify();
                    } else {
                        self.splash_task = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(hold).await;
                            this.update(cx, |shell, cx| {
                                shell.splash = SplashPhase::Gone;
                                crate::boot_stats::mark_splash_gone();
                                cx.notify();
                            })
                            .ok();
                        }));
                    }
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => {
                self.splash = SplashPhase::Gone;
                crate::boot_stats::mark_splash_gone();
                cx.notify();
            }
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    /// Spawn-chip events from the primary transcript AND from subagent-tab
    /// transcripts (nested spawns open their own tabs).
    fn on_transcript_event(
        &mut self,
        _: Entity<Transcript>,
        event: &TranscriptEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TranscriptEvent::OpenSubagent {
                chat_id,
                doc_id,
                title,
                frozen,
            } => {
                self.add_subagent_surface(
                    chat_id.clone(),
                    doc_id.clone(),
                    title.clone(),
                    *frozen,
                    cx,
                );
            }
        }
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    /// Publish this view's working copy to the central settings store. The
    /// store owns the single debounce task and the only production writer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        self.settings.ui_font_family = crate::typography::requested(cx);
        self.settings.ui_font_size = crate::typography::font_size(cx);
        settings::replace(self.settings.clone(), SavePolicy::Debounced, cx);
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    /// Close the user menu through the exit animation (no-op when closed).
    fn close_user_menu(&mut self, cx: &mut Context<Self>) {
        if self.user_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.user_menu);
            cx.notify();
        }
    }

    /// Close the session-row context menu through the exit animation.
    fn close_chat_menu(&mut self, cx: &mut Context<Self>) {
        if self.chat_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.chat_menu);
            cx.notify();
        }
    }

    fn open_chat_copy_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = self.chat_menu.open_mut() {
            menu.page = ChatMenuPage::Copy;
            cx.notify();
        }
    }

    fn copy_zeron_conversation_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let link = {
            let state = self.state.read(cx);
            crate::links::workspace_locator(
                state.workspace_scope,
                state.auth.as_ref(),
                state.local_device_id.as_deref(),
            )
            .map(|workspace| crate::links::zeron_conversation_link(chat_id, &workspace))
        };
        if let Some(link) = link {
            cx.write_to_clipboard(ClipboardItem::new_string(link));
            self.sidebar_notice = Some("Zeron conversation link copied".into());
        } else {
            self.sidebar_notice = Some("Conversation link is not ready yet".into());
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_harness_conversation_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let link = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(crate::links::harness_conversation_link);
        if let Some(link) = link {
            cx.write_to_clipboard(ClipboardItem::new_string(link.url));
            self.sidebar_notice = Some(format!("{} copied", link.label).into());
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_harness_session_id(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let id = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.harness_session_id.clone());
        if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(id));
            self.sidebar_notice = Some("Harness session ID copied".into());
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        // Recreate per visit: the page's ListHarnesses load re-probes which
        // CLIs are installed, so installing one shows up on the next open.
        if section == SettingsSection::Harnesses {
            self.harnesses_page = None;
        }
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Harnesses => {
                if self.harnesses_page.is_none() {
                    let state = self.state.clone();
                    self.harnesses_page = Some(cx.new(|cx| HarnessesPage::new(state, cx)));
                }
                match &self.harnesses_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let page = cx.new(|cx| {
                        NotificationsPage::new(
                            self.settings.sound_enabled,
                            self.settings.notifications_enabled,
                            self.settings.notifications_background_only,
                            cx,
                        )
                    });
                    // Persist the flags whenever the page flips one.
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::Changed {
                                sound,
                                desktop,
                                background_only,
                            } = *event;
                            this.settings.sound_enabled = sound;
                            this.settings.notifications_enabled = desktop;
                            this.settings.notifications_background_only = background_only;
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.set_chat_archived(chat_id, true, cx);
    }

    /// The Archive session shortcut. With no chat open, or with an already
    /// archived one, it does nothing — the shortcut archives, it never
    /// unarchives.
    fn archive_selected_chat(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self
            .state
            .read(cx)
            .archivable_selected_chat()
            .map(str::to_string)
        else {
            return;
        };
        self.archive_chat(chat_id, cx);
    }

    pub(super) fn set_chat_archived(
        &mut self,
        chat_id: String,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
            cx,
        );
        cx.notify();
    }

    /// A jump shortcut: open the sidebar row at `slot`. A slot past the end of
    /// a short list does nothing. Reads the DISPLAYED order — sort and
    /// grouping view options permute the list, and the chip on a row must
    /// name the key that opens it.
    fn jump_to_session(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(chat_id) = self.sidebar_visible_order(cx).into_iter().nth(slot) else {
            return;
        };
        // Same path a click on that row takes.
        self.open_chat(chat_id, cx);
    }

    /// Whether an overlay that owns the keyboard is up — the add-space
    /// palette or a composer picker popover (model selector, traits, repo,
    /// branch…). Session-nav shortcuts (cycle/jump/archive) go quiet
    /// underneath one: gpui runs a matched binding before any `on_key_down`,
    /// so an unguarded jump would switch sessions UNDER the open popover,
    /// stranding it over a session the user never picked.
    pub(super) fn overlay_owns_keyboard(&self, cx: &App) -> bool {
        self.add_space.is_some() || self.composer.read(cx).pickers().read(cx).is_open()
    }

    /// Track the held modifiers so the sidebar can show its jump hints. Only a
    /// change in visibility repaints — modifier traffic is otherwise constant.
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let mods = &event.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        // No hints while an overlay owns the keyboard — the jumps they
        // advertise are suppressed there.
        let visible = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && jump_hints_visible(&self.settings.keymap, primary, mods.alt, mods.shift);
        self.set_jump_hints(visible, cx);
    }

    pub(super) fn set_jump_hints(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.jump_hints != visible {
            self.jump_hints = visible;
            cx.notify();
        }
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    // ---- org gate ----

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `ZERON_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    fn tween_active(&self, tween: Option<WidthTween>) -> bool {
        tween.is_some_and(|tween| {
            !self.reduced_motion
                && tween.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
        })
    }

    fn active_tween_endpoints(&self, tween: Option<WidthTween>) -> Option<(f32, f32)> {
        tween
            .filter(|transition| {
                !self.reduced_motion
                    && transition.started.elapsed() < RESIZE.total().mul_f32(motion::speed_scale())
            })
            .map(|transition| (transition.from, transition.to))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Which caption buttons zeron itself must draw on Linux: under
    /// client-side decorations (the Wayland default) nobody else will —
    /// without these the window has NO minimize/maximize/close at all.
    /// Server-side decorations (X11 WMs, KDE with SSD) already draw real
    /// buttons, so `None` there. The desktop's layout (GNOME's
    /// `button-layout` gsetting via `cx.button_layout()`) decides side and
    /// order — min/max/close on the right by default; controls the
    /// compositor can't do (e.g. minimize on some Wayland compositors) drop
    /// out, close always stays.
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The sidebar is part of the resolved theme. A second fixed-Zeron
        // palette here made imported families look split in half and froze
        // activity/glyph personality independently of the selected variant.
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation. The content row spans the
        // full window height (the titlebar overlays it), so the column pads
        // itself below the chrome.
        self.pane_container(
            self.sidebar_tween,
            target,
            div()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .child(inner)
                .into_any_element(),
        )
    }

    /// Settings-mode sidebar (zeron settings-sidebar.tsx): window-control
    /// strip, "Settings" heading, icon section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::Harnesses => icons::WIDGET,
            SettingsSection::Agents => icons::KEY_MINIMALISTIC,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Notifications => icons::BELL,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(crate::typography::ui_rems(13.0))
                                .when(selected, |el| {
                                    // Same tokens as the main sidebar's session
                                    // rows — the two sidebars must feel alike.
                                    el.bg(crate::theme::glass_selected_bg())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom (zeron settings-sidebar.tsx).
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (zeron settings-sidebar.tsx),
                            // not the straight history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row: context + status on line one, harness + title on line
    /// two, and source metadata below. Working uses the live thread glyph in
    /// the status corner. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        change_request: Option<zeron_proto::ChangeRequestSummary>,
        harness: Option<zeron_proto::HarnessId>,
        status: zeron_proto::ChatIndicator,
        selected: bool,
        archived: bool,
        // This row's jump combo while the hint overlay is up. It takes the
        // corner outright — above hover and above the status word — so all
        // nine chips appear together instead of leaving a hole on whichever
        // row is busy or under the pointer.
        jump_label: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Activity, not position (t3code Sidebar): status is a small colored
        // word + glyph in the row's top-right corner — Working animates the
        // composer-strip spinner, Done wears a check; Idle rows show the
        // relative time instead. Hovering the ROW swaps the corner for the
        // ARCHIVE button (UNARCHIVE on rows in the sidebar's archived
        // accordion), t3code's settle-on-hover.
        let corner_hovered = self.chat_status_hover.as_deref() == Some(id.as_str());
        // Send-truth overrides: a send unadopted past the grace window is
        // FAILED (explicit, with the transcript's retry affordance); a send
        // whose delivery path is degraded is QUEUED, not Working — the
        // pending pill tells the truth instead of faking a spinner.
        let (queued, undelivered) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            (
                state.send_queued(&id, now),
                state.send_undelivered(&id, now),
            )
        };
        let status_color = if undelivered {
            theme.danger
        } else if queued {
            theme.warning
        } else {
            spaces::status_dot_color(status, theme)
        };
        let status_label: Option<&'static str> = if undelivered {
            Some("Failed")
        } else if queued {
            Some("Queued")
        } else {
            match status {
                zeron_proto::ChatIndicator::Working => Some("Working"),
                zeron_proto::ChatIndicator::AwaitingInput => Some("Input"),
                zeron_proto::ChatIndicator::Errored => Some("Failed"),
                zeron_proto::ChatIndicator::Completed => Some("Done"),
                zeron_proto::ChatIndicator::Idle => None,
            }
        };
        let shows_metadata = branch.is_some() || change_request.is_some();
        let queued = queued && !undelivered;
        let working = status == zeron_proto::ChatIndicator::Working && !queued && !undelivered;
        let corner_body: AnyElement = if let Some(label) = jump_label {
            // The jump hint replaces the status/time corner while the modifier
            // is held, cut to the sidebar PR badge's exact cloth
            // (`pull_request_badge`, Sidebar surface): pinned 16px, px 4,
            // rounded 4, borderless 0.08-fill with 0.85 text of one tone —
            // neutral here — and the label in the badge's mono at 10 MEDIUM.
            // Any other geometry reads as a second badge system on the row.
            {
                let tone = theme.text_muted;
                div()
                    .h(px(16.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .bg(tone.opacity(0.08))
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tone.opacity(0.85))
                    .font_family(theme.font_mono.clone())
                    .child(label)
                    .into_any_element()
            }
        } else if corner_hovered {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .h(px(18.0))
                // The pill's padding bleeds right into the row's padding so
                // its TEXT right-aligns exactly where the status word/time
                // sits — the swap moves pixels around the label, not it.
                // 4px: what's left of the row's 8px padding then equals the
                // 4px of air above the pill (18px tall on the 14px line,
                // 6px row padding minus the 2px overflow).
                .px(px(4.0))
                .mr(px(-4.0))
                .rounded(px(5.0))
                .bg(crate::theme::wash(0.10))
                .hover(|s| s.bg(crate::theme::wash(0.18)))
                .child(
                    icon(if archived {
                        icons::ARCHIVE_UP_MINIMALISTIC
                    } else {
                        icons::ARCHIVE_MINIMALISTIC
                    })
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(if archived {
                            "Unarchive"
                        } else {
                            "Archive"
                        })),
                )
                .into_any_element()
        } else {
            match status_label {
                Some(label) => {
                    // Glyph slot: Working wears the preset's animated pixel
                    // glyph beside its label, Done wears the check, and the
                    // remaining statuses use a compact dot.
                    let glyph: AnyElement = if status == zeron_proto::ChatIndicator::Completed {
                        icon(icons::CHECK)
                            .size(px(11.0))
                            .flex_none()
                            .text_color(status_color)
                            .into_any_element()
                    } else if working {
                        loaders::mini_glyph_spinner(
                            format!("chat-working-{id}"),
                            2.0,
                            theme.glyph,
                            cx.entity_id(),
                            cx,
                        )
                        .into_any_element()
                    } else {
                        div()
                            .size(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(status_color)
                            .into_any_element()
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(glyph)
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(status_color)
                                .child(SharedString::from(label)),
                        )
                        .into_any_element()
                }
                None => div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(time_ago.clone())
                    .into_any_element(),
            }
        };
        // One stable wrapper across both states (identity keeps the hover
        // from flickering as the content swaps); the swap is driven by the
        // ROW's hover (user request — corner-only felt undiscoverable), but
        // archiving only clicks on the corner itself, so the row's own click
        // stays the selector.
        let corner: AnyElement = {
            let archive_id = id.clone();
            div()
                .id(SharedString::from(format!("chat-corner-{id}")))
                .flex_none()
                // Pin the corner to line 1's text height so the archive pill
                // (taller, padded) overflows vertically instead of growing the
                // row — the swap must not shift the card's content.
                // NO occlude: the ROW's hover drives the swap, and an
                // occluding corner un-hovered the row underneath it —
                // pill mounts, steals the pointer, row un-hovers, pill
                // unmounts, repeat (user-reported flicker). The pill's
                // stop_propagation click is separation enough.
                .h(px(14.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(corner_hovered, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_chat_archived(archive_id.clone(), !archived, cx);
                    }))
                })
                .child(corner_body)
                .into_any_element()
        };
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let select_id = id.clone();
        let menu_id = id.clone();
        // Hover fades over transition-colors (zeron session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A selected row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if selected { selected_wash } else { hover };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            // No selection ring (user request) — the wash alone marks the
            // active row.
            // Row hover drives BOTH the wash blend and the corner's
            // status→Archive swap (one listener — gpui allows a single
            // hover listener per element).
            .on_hover({
                let fade_hover = motion::hover_listener(fade_key.clone());
                let hover_id = id.clone();
                cx.listener(move |this, hovered: &bool, window, cx| {
                    fade_hover(hovered, window, cx);
                    if *hovered {
                        if this.chat_status_hover.as_deref() != Some(hover_id.as_str()) {
                            this.chat_status_hover = Some(hover_id.clone());
                            cx.notify();
                        }
                    } else if this.chat_status_hover.as_deref() == Some(hover_id.as_str()) {
                        this.chat_status_hover = None;
                        cx.notify();
                    }
                })
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu.open(ChatMenuState {
                        chat_id: menu_id.clone(),
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            )
            // Line 1: "project @ device", status word / time-ago right.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .line_height(px(14.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    .child(div().text_color(subline).child(corner)),
            )
            // Line 2: harness identity belongs directly with the title,
            // instead of floating as unrelated metadata below it.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(subline).opacity(0.8)),
                            )
                        },
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(17.0))
                            .child(title),
                    ),
            )
            // Line 3 is structural, not reserved whitespace: compact states
            // omit it completely when both Branch and Pull request are hidden.
            .when(shows_metadata, |row| {
                row.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .when_some(branch, |el, branch| {
                            el.child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(subline),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .line_height(px(14.0))
                                    .text_color(subline)
                                    .child(branch),
                            )
                        })
                        // Stable invisible spring keeps the optional PR badge
                        // pinned right without changing no-PR paint.
                        .child(div().flex_1().min_w_0())
                        .when_some(change_request, |el, summary| {
                            el.child(crate::change_requests::pull_request_badge(
                                format!("chat-pr-{id}").into(),
                                summary,
                                crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                theme,
                            ))
                        }),
                )
            })
            .into_any_element()
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    /// The global connection line. `None` while healthy (`Connected`) or on
    /// local profiles (`Disabled`) — and the engine's degrade grace means it
    /// only exists during REAL outages, never join/wake blips. No surface,
    /// no border (v0.2.12 feedback): a bare spinner + faint caption while
    /// reconnecting; an amber dot only when the OS says offline. The
    /// transport error belongs in logs, not the sidebar.
    fn render_connection_pill(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        use zeron_proto::ConnectivityState as S;
        let conn = self.state.read(cx).connectivity.clone();
        let (label, glyph): (SharedString, AnyElement) = match conn.state {
            S::Disabled | S::Connected => return None,
            S::Offline => (
                "Offline — sends are saved".into(),
                div()
                    .size(px(5.0))
                    .rounded_full()
                    .bg(theme.warning)
                    .into_any_element(),
            ),
            S::Reconnecting => (
                "Reconnecting…".into(),
                loaders::mini_mono_spinner(
                    "connection-spinner",
                    2.0,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element(),
            ),
        };
        Some(
            crate::motion::fade_in(
                "connection-pill",
                div()
                    .id("connection-pill")
                    .mx(px(Theme::SPACE_SM + 4.0))
                    .mb(px(Theme::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child(label),
                    ),
            )
            .into_any_element(),
        )
    }

    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (user, workspace_scope) = {
            let state = self.state.read(cx);
            (state.auth_user().cloned(), state.workspace_scope)
        };

        // Keyed rows: (stable key, estimated height, element) — the key + height
        // list drives the §1.6 resort FLIP diff below (attention-bucket
        // promotions glide; cleared rows just go).
        let keyed: Vec<(String, f32, AnyElement)> = self.render_active_rows(theme, cx);

        // Resort glide (§1.6 View Transitions parity): when the ORDER of a live
        // list changes (new activity resort, grouping flip), surviving rows
        // glide from their old y to the new one — layout is already at the new
        // position; the offset is a paint-only relative inset animated to 0
        // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
        // just go (matching the original). First fill and chat switches (which
        // don't reorder) never animate.
        let order: Vec<(String, f32)> = keyed.iter().map(|(k, h, _)| (k.clone(), *h)).collect();
        if self.sidebar_prev_order != order {
            let key_order_changed = sidebar_key_order_changed(&self.sidebar_prev_order, &order);
            if !self.sidebar_prev_order.is_empty() {
                // A disclosure already animates its own body height. Applying
                // FLIP offsets when only keyed heights change double-counts
                // that movement, leaving gaps and momentary overlaps between
                // the first group, following groups, and Archived.
                let offsets = if key_order_changed {
                    resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP)
                } else {
                    std::collections::HashMap::new()
                };
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                    .map(|(k, _)| k.clone())
                    .collect();
                if key_order_changed && (!offsets.is_empty() || !new_keys.is_empty()) {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }
        let epoch = self.resort_epoch;
        let list_items: Vec<AnyElement> = keyed
            .into_iter()
            .map(|(key, _, element)| {
                if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                    let id = SharedString::from(format!("resort-{epoch}-{key}"));
                    div()
                        .child(element)
                        .with_animation(id, RESORT.animation(), move |el, t| {
                            el.relative().top(px(dy * (1.0 - t)))
                        })
                        .into_any_element()
                } else if self.sidebar_new_keys.contains(&key) {
                    let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                    motion::fade_quick(id, div().child(element)).into_any_element()
                } else {
                    element
                }
            })
            .collect();

        // t3code's archived accordion, below the active list.
        let archived_section = self.render_archived_section(theme, cx);

        let (user_line, trigger_subline, menu_identity): (
            SharedString,
            Option<SharedString>,
            SharedString,
        ) = match workspace_scope {
            Some(WorkspaceScope::Local) => {
                let line = if matches!(self.sync_flow, SyncFlow::RestartPending { .. }) {
                    "Sync ready after restart"
                } else {
                    "Local only"
                };
                (line.into(), None, "Stored on this device".into())
            }
            Some(WorkspaceScope::Development) => (
                "Development".into(),
                Some("Local development runtime".into()),
                "Authentication disabled".into(),
            ),
            Some(WorkspaceScope::Synced) | None => {
                let line: SharedString = user
                    .as_ref()
                    .map(|u| u.name.clone().unwrap_or_else(|| u.email.clone()).into())
                    .unwrap_or_else(|| SharedString::from("Not signed in"));
                let email = user
                    .as_ref()
                    .map(|u| SharedString::from(u.email.clone()))
                    .unwrap_or_else(|| line.clone());
                (line, Some("Alpha".into()), email)
            }
        };
        let user_menu =
            self.render_user_menu(user_line.clone(), trigger_subline, menu_identity, theme, cx);

        // The space filter lives ABOVE the scroll region (fixed) so its
        // dropdown can float without being clipped by the list's overflow.
        let filter_row = self.render_spaces_filter(theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            .child(filter_row)
            // The (filtered) Sessions list scrolls inside an EdgeFade scope —
            // a true per-glyph gradient at active overflow edges. Glass-safe
            // (no painted overlay can fade content over see-through blur) and
            // equivalent on opaque themes: alpha→0 reveals the surface tone
            // underneath, same as the gradient overlays it replaced. Overflow
            // is read at PAINT time via the scroll handle — render-time gating
            // rode the previous frame's offset, so the last frame of a content
            // shrink (row archived while scrolled) left a phantom fade stuck
            // over an unscrollable list (user report).
            .child(
                crate::edge_fade::edge_faded(
                    SIDEBAR_GLASS_FADE_BAND,
                    true,
                    true,
                    div().relative().flex_1().min_h_0().child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            // No "Sessions" header (user request) — the list
                            // is the whole column; a little air stands in.
                            .pt(px(4.0))
                            .child(if !list_items.is_empty() {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .children(list_items)
                                    .into_any_element()
                            } else {
                                div()
                                    .px(px(Theme::SPACE_SM))
                                    .pb(px(Theme::SPACE_SM))
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from("No sessions yet"))
                                    .into_any_element()
                            })
                            .children(archived_section),
                    ),
                )
                .fade_overflow_y(&self.sidebar_scroll),
            )
            // Global connection pill (durable-by-design UI truth): appears
            // whenever the edge posture is degraded; hidden while healthy —
            // appearing IS the signal.
            .when_some(self.render_connection_pill(theme, cx), |el, pill| {
                el.child(pill)
            })
            // Update strip (above the user menu; below the lists).
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
    }

    /// Update strip: shown above the user menu whenever the engine's
    /// UpdateStatus stream reports a newer release. On a macOS bundle install
    /// it drives the whole flow — click to download, then click to restart into
    /// the staged bundle. Elsewhere (managed/source installs) it is advisory
    /// (`zeron update`); click dismisses it for that version.
    fn render_update_strip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.state.read(cx).update.clone()?;
        if !status.update_available {
            return None;
        }
        let latest = status.latest_version.clone()?;
        if self.update_dismissed.as_deref() == Some(latest.as_str()) {
            return None;
        }
        let mac_app = matches!(self.install, zeron_update::InstallKind::MacApp { .. });

        let (label, clickable): (SharedString, bool) = if mac_app {
            match &self.update_flow {
                UpdateFlow::Idle => (format!("Update available — v{latest}").into(), true),
                UpdateFlow::Downloading => (format!("Downloading v{latest}…").into(), false),
                UpdateFlow::Ready(_) => ("Update ready — restart to apply".into(), true),
                UpdateFlow::Failed(message) => (format!("Update failed: {message}").into(), true),
            }
        } else {
            (
                format!("Update available — v{latest} · run `zeron update`").into(),
                true,
            )
        };
        let failed = matches!(self.update_flow, UpdateFlow::Failed(_));
        let tone = if failed { theme.danger } else { theme.accent };
        // Follow the selected spectrum with a low-emphasis glass tint rather
        // than painting the bright text accent as a solid slab.
        let (chip_bg, chip_bg_hover) = if failed {
            (theme.danger.opacity(0.14), theme.danger.opacity(0.22))
        } else {
            (theme.accent_wash, theme.accent.opacity(0.16))
        };

        let mut strip = div()
            .id("update-strip")
            .mx(px(Theme::SPACE_SM))
            // No bottom margin: the user-menu block below carries its own
            // SPACE_SM padding — doubling it read as a hole (user report).
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(chip_bg)
            .flex()
            .flex_row()
            .items_center()
            .text_size(crate::typography::ui_rems(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(div().flex_1().min_w_0().child(label));
        if clickable {
            strip = strip
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.on_update_strip_click(cx)));
        }
        Some(strip.into_any_element())
    }

    /// Idle → download; Ready → swap + relaunch; Failed → retry; advisory
    /// installs → dismiss for this version.
    fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.install, zeron_update::InstallKind::MacApp { .. }) {
            self.update_dismissed = self
                .state
                .read(cx)
                .update
                .as_ref()
                .and_then(|s| s.latest_version.clone());
            cx.notify();
            return;
        }
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Idle | UpdateFlow::Failed(_) => self.begin_update_download(cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
        }
    }

    /// Fetch the manifest and stage the new Zeron desktop bundle under the data dir
    /// (tokio — reqwest); the strip flips to "restart to apply" when done.
    fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let edge_url = self.boot.edge_url.clone();
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            let manifest = zeron_update::fetch_latest(&edge_url).await?;
            zeron_update::stage_mac_app(&edge_url, &manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_flow = match outcome {
                    Ok(staged) => UpdateFlow::Ready(staged),
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        UpdateFlow::Failed(message.into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let zeron_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match zeron_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                zeron_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed(format!("{err:#}").into());
                cx.notify();
            }
        }
    }

    /// Floating layers owned by the shell: context menus, edit dialogs, and
    /// the local-to-synced account lifecycle.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some(menu_state) = self.chat_menu.get().cloned() {
            let chat_id = menu_state.chat_id;
            let position = menu_state.position;
            let chat_menu_closing = self.chat_menu.closing_since();
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(216.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_chat_menu(cx);
                }))
                .flex()
                .flex_col();
            let menu = match menu_state.page {
                ChatMenuPage::Root => menu
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                            .id("chat-menu-rename")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename_chat(rename_id.clone(), cx)
                            }))
                            .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                            .child(SharedString::from("Rename…")),
                    )
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                            .id("chat-menu-archive")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.archive_chat(archive_id.clone(), cx)
                            }))
                            .child(
                                icon(icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Archive")),
                    )
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-copy-{chat_id}"))
                            .id("chat-menu-copy")
                            .on_click(cx.listener(|this, _, _, cx| this.open_chat_copy_menu(cx)))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(div().flex_1().child(SharedString::from("Copy")))
                            .child(
                                icon(icons::ALT_ARROW_RIGHT)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted.opacity(0.7)),
                            ),
                    )
                    .child(popover::menu_separator())
                    .child(
                        popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                            .id("chat-menu-delete")
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_chat_menu(cx);
                                this.delete_confirm = Some(delete_id.clone());
                                cx.notify();
                            }))
                            .child(
                                icon(icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.danger),
                            )
                            .child(SharedString::from("Delete…")),
                    ),
                ChatMenuPage::Copy => {
                    let chat = self
                        .state
                        .read(cx)
                        .chats
                        .iter()
                        .find(|chat| chat.id == chat_id)
                        .cloned();
                    let harness_link = chat
                        .as_ref()
                        .and_then(crate::links::harness_conversation_link);
                    let session_id = chat
                        .as_ref()
                        .and_then(|chat| chat.harness_session_id.as_deref())
                        .is_some_and(|id| !id.trim().is_empty());
                    let zeron_id = chat_id.clone();
                    let harness_id = chat_id.clone();
                    let session_chat_id = chat_id.clone();
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-copy-back-{chat_id}"))
                            .id("chat-copy-back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(menu) = this.chat_menu.open_mut() {
                                    menu.page = ChatMenuPage::Root;
                                    cx.notify();
                                }
                            }))
                            .child(
                                icon(icons::ALT_ARROW_LEFT)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Back")),
                    )
                    .child(popover::menu_separator())
                    .child(
                        popover::menu_row(&theme, false, format!("chat-copy-zeron-{chat_id}"))
                            .id("chat-copy-zeron")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_zeron_conversation_link(&zeron_id, cx)
                            }))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Zeron conversation link")),
                    )
                    .when_some(harness_link, |menu, link| {
                        menu.child(
                            popover::menu_row(
                                &theme,
                                false,
                                format!("chat-copy-harness-{chat_id}"),
                            )
                            .id("chat-copy-harness")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_harness_conversation_link(&harness_id, cx)
                            }))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(link.label)),
                        )
                    })
                    .when(session_id, |menu| {
                        menu.child(
                            popover::menu_row(
                                &theme,
                                false,
                                format!("chat-copy-session-{chat_id}"),
                            )
                            .id("chat-copy-session")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_harness_session_id(&session_chat_id, cx)
                            }))
                            .child(
                                icon(icons::COPY)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Harness session ID")),
                        )
                    })
                }
            }
            .into_any_element();
            overlays.push(popover::menu_at(
                "chat-context-menu",
                position,
                menu,
                chat_menu_closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        if let Some(sync) = self.render_sync_overlay(viewport, cx) {
            overlays.push(sync);
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let theme = Theme::of(cx);
        let fade_key = format!("pane-resize-{id}");
        let highlight = motion::hover_blend(
            &fade_key,
            theme.border_strong.opacity(0.0),
            theme.border_strong,
        );
        let clear = highlight.opacity(0.0);
        div()
            .id(id)
            .absolute()
            .top(px(PANE_RESIZE_HITBOX_TOP))
            .bottom_0()
            .w(px(12.0))
            .flex_none()
            .cursor_col_resize()
            .on_hover(motion::hover_listener(fade_key))
            // Codex-style seam feedback: the existing 1px panel border stays
            // visible at rest; hover adds a stronger center highlight that
            // fades back into that border toward both ends.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(6.0))
                    .w(px(1.0))
                    .flex()
                    .flex_col()
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(clear, 0.0),
                        gpui::linear_color_stop(highlight, 1.0),
                    )))
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(highlight, 0.0),
                        gpui::linear_color_stop(clear, 1.0),
                    ))),
            )
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar). Settings never
        // underlaps: pad below the overlaid titlebar.
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let no_project = self.state.read(cx).no_project;

        // Content outlet: selected chat → transcript; nothing selected → a
        // bare canvas (the composer stack carries the affordances); no spaces
        // at all → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else if !has_spaces && !no_project {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::ZERON_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(crate::typography::ui_rems(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a project to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(crate::typography::ui_rems(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A project is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a project")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas: intentionally bare (user request — no logo, no
            // helper line). The device + project selectors live above the
            // composer pill (composer.rs renders them via
            // `render_target_selectors`).
            div().size_full().into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. GPUI derives the veil's visibility from the
        // active payload type: an internal drag such as a pane resize must
        // never be able to resurrect stale external-file hover state.
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The terminal dock is NOT glass the transcript may slide
                    // under: with the dock's translucent fill, transcript text
                    // ghosted through the grid (user report). The underlay
                    // ends at the dock's top instead, riding the same height
                    // tween the dock animates with; `stack_h` below is only
                    // the chrome that still overlaps the transcript (status
                    // strip + composer).
                    let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                    let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .bottom(px(term_h))
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                true,
                                true,
                                div().size_full().child(outlet),
                            )
                            // Fully faded BY the titlebar's bottom edge (the
                            // title text is opaque — overlap read as collision),
                            // ramping in the band just below it.
                            .inset_top(Theme::TITLEBAR_HEIGHT)
                            .band_top(Theme::TRANSCRIPT_FADE_BAND)
                            .band_bottom(bottom_band),
                        )
                        .children(self.render_jump_to_bottom(stack_h, cx))
                },
            )
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts), composer, terminal dock. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(status)
                    .when(has_spaces, |el| el.child(self.composer.clone()))
                    .child(self.render_terminal_container(cx))
            })
            .child(
                div()
                    .invisible()
                    .absolute()
                    .inset_0()
                    .bg(theme.scrim().opacity(0.4 / 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child("Drop images to attach")
                    .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.visible())
                    .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                        let paths = paths.paths().to_vec();
                        this.composer
                            .update(cx, |composer, cx| composer.add_paths(paths, cx));
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    /// `stack_h` is the measured bottom chrome stack the full-height
    /// transcript scrolls under — the pill anchors just above it (the -14
    /// carries the old status-strip overlap).
    fn render_jump_to_bottom(
        &mut self,
        stack_h: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom(px(stack_h - 14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(self.jump_pill("jump-to-bottom", "jump-pill", self.transcript.clone(), cx))
                .into_any_element(),
        )
    }

    /// The jump pill itself — shared between the conversation overlay and
    /// the subagent pane so both read as one control. `anim_key`/`hover_key`
    /// must be distinct per instance (they key global animation state).
    ///
    /// Glass-forward like the composer pill it floats near: a backdrop blur
    /// under the floating-card tint ([`Theme::glass_overlay`]), hover
    /// brightening via the standard glass wash painted OVER the tint —
    /// mixing the tint TOWARD the wash would thin the pill on hover, the
    /// exact see-through regression the old opaque pill's comment warned
    /// about. Opaque appearances keep the raised-surface treatment
    /// (`frosted` passes through there anyway).
    fn jump_pill(
        &self,
        anim_key: &'static str,
        hover_key: &'static str,
        transcript: Entity<Transcript>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx);
        let glass = theme.is_glass();
        let base = if glass {
            theme.glass_overlay()
        } else {
            motion::hover_blend(hover_key, theme.surface_raised, theme.surface_raised_hover)
        };
        let wash = if glass {
            motion::hover_blend(hover_key, gpui::transparent_black(), theme.glass_hover())
        } else {
            gpui::transparent_black()
        };
        let pill = div()
            .id(anim_key)
            .h(px(30.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .cursor_pointer()
            .bg(base)
            .on_hover(motion::hover_listener(hover_key))
            .on_click(cx.listener(move |_, _, _, cx| {
                transcript.update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
            }))
            .child(
                // The hover wash rides an inner full-height layer so it
                // composites over the tint (a div has one bg).
                div()
                    .h_full()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(11.0))
                    .pr(px(13.0))
                    .bg(wash)
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("↓")),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Scroll to bottom")),
                    ),
            );
        // Frost OUTSIDE the entry animation (the composer pill's exact
        // composition): one scene layer — blur, then the pill's quads, then
        // glyphs — so the pill always composes over the transcript content
        // scrolling under it, and never loses its washes to the kind-sorted
        // draw order (frost.rs module docs).
        crate::frost::frosted(15.0, 16.0, motion::dialog_in(anim_key, pill)).into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (zeron styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.viewport_width = f32::from(window.viewport_size().width);
        // Appearance actions persist independently of the shell. Mirror the
        // globals before any later debounced settings save can overwrite them.
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        let theme = Theme::of(cx);
        // The shell tone (zeron `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let (workspace_scope, auth) = {
            let state = self.state.read(cx);
            (state.workspace_scope, state.auth.clone())
        };
        self.sync_flow = sync_flow_after_auth(self.sync_flow, workspace_scope, auth.as_ref());
        let restart_required = self.sync_flow == SyncFlow::SignedOutRestartRequired;
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Linux CSD: (re-)resolve which caption buttons we draw and on which
        // side — decorations can flip server↔client at runtime and the
        // desktop's button layout is user configuration.
        self.linux_captions = Self::resolve_linux_captions(window, cx);
        if cfg!(target_os = "linux") && self.button_layout_sub.is_none() {
            self.button_layout_sub =
                Some(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        if self.activation_sub.is_none() {
            self.activation_sub = Some(cx.observe_window_activation(
                window,
                |this: &mut Shell, window, cx| {
                    if !window.is_window_active() {
                        this.set_jump_hints(false, cx);
                    }
                },
            ));
        }

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if !restart_required
            && matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat)
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(crate::typography::ui_rems(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (zeron __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            // New session works from anywhere — `open_new_session` routes back
            // to chat itself, so Settings is not a dead spot.
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(cx)))
            // Native Settings menu item and the platform convention (Cmd+, on
            // macOS, Ctrl+, elsewhere) always land on the default section.
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(SettingsSection::Devices, cx)
            }))
            // Chat-scoped, unlike new-session — `cycle_session` holds the guard
            // and says why.
            .on_action(cx.listener(|this, _: &NextSession, _, cx| this.cycle_session(true, cx)))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| this.cycle_session(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            // Chat-scoped like the panel toggles: Settings has no current
            // session to archive. Quiet under an open popover, like the other
            // session-nav shortcuts.
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if matches!(this.route, Route::Chat) && !this.overlay_owns_keyboard(cx) {
                    this.archive_selected_chat(cx)
                }
            }))
            // A jump routes back to chat itself, so Settings is not a dead
            // spot — the same call a click on that sidebar row makes. But an
            // open picker/palette owns the keyboard: no jumping underneath
            // it. The MODEL menu advertises these same slots on its rows and
            // this matched binding beats its key handler to the dispatch —
            // forward the slot instead of eating it.
            .on_action(cx.listener(|this, jump: &JumpSession, _, cx| {
                let pickers = this.composer.read(cx).pickers().clone();
                let handled = pickers.update(cx, |pickers, cx| {
                    pickers.jump_model_slot(jump.0, cx)
                });
                if !handled && !this.overlay_owns_keyboard(cx) {
                    this.jump_to_session(jump.0, cx)
                }
            }))
            .on_modifiers_changed(
                cx.listener(|this, event, _, cx| this.on_modifiers_changed(event, cx)),
            )
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }));

        let render_gate = if restart_required {
            GatePhase::Loading
        } else {
            gate.clone()
        };
        let root = match &render_gate {
            GatePhase::Ready => {
                // Focus is a sync signal: on the rising edge of window
                // activation, nudge every open room to verify liveness — a
                // broadcast-deaf socket (accepted writes, runtime pongs,
                // nothing delivered; 2026-08-04 incident) then heals within
                // seconds of the user looking at the app rather than waiting
                // out the background probe cadence.
                let window_active = window.is_window_active();
                if window_active && !self.was_window_active {
                    self.state.update(cx, |s, cx| s.probe_sync(cx));
                }
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `ZERON_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                // Stamped for `right_target` — the expanded changes panel
                // sizes itself to the viewport.
                self.viewport_width = viewport;
                let main_target_width =
                    conversation_width(viewport, self.sidebar_target(), self.right_target(cx));
                let main_transition = self.active_tween_endpoints(self.main_takeover_tween);
                let main_content_width =
                    stable_panel_content_width(main_target_width, main_transition);
                let main_width = (main_content_width - 10.0).max(0.0);
                self.composer.update(cx, |composer, cx| {
                    composer.set_available_width(main_width, cx)
                });
                // Clearance excludes the terminal dock: the transcript
                // viewport ends at the dock's top (see the underlay in
                // `render_main`), so only the chrome above it overlaps.
                let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx);
                    t.set_bottom_clearance(stack_h, cx);
                });

                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (zeron __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                let right_open = on_chat && self.right_pane_open(cx);
                // Takeover mode derives its width from the viewport, so a
                // manual drag handle would fight the expanded target.
                let right_handle = (right_open
                    && !self.right_pane_expanded
                    && !self.tween_active(self.right_tween))
                .then(|| {
                    self.resize_handle(
                        "right-pane-resize",
                        || RightPaneResize,
                        |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                        cx,
                    )
                    // A forgiving transparent hit target centered on the
                    // seam; the panel's 1px border remains the visual divider.
                    .left(px(-6.0))
                });
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // Copied out (not held) — `render_title_bar` needs `cx` mutable.
                let border_color = Theme::of(cx).border;
                // No inset cards (user request): the conversation column sits
                // flush and unbordered, the transcript directly on the frost
                // glass; the changes pane is a flush left-bordered glass panel
                // (built inside `render_right_pane`).
                let main = if main_transition.is_some() {
                    div()
                        .h_full()
                        .w(px(main_content_width))
                        .flex_none()
                        .flex()
                        .child(main)
                        .into_any_element()
                } else {
                    main
                };
                let card: AnyElement = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .child(main)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (zeron
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.left(px(-6.0)));
                // Keep the right resize target outside the pane's
                // overflow-hidden width container. This mirrors the sidebar
                // seam and lets the target straddle both adjacent panes.
                let right_seam: AnyElement = if let Some(handle) = right_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .relative()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                // The content row spans the FULL window height — the titlebar
                // overlays it (glass, no fill), so the transcript can scroll
                // under the header and fade out at its edge. Columns that
                // must NOT underlap (sidebar content, the changes panel,
                // settings) pad themselves down by the titlebar height.
                let page = div()
                    .size_full()
                    .relative()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(card)
                            .child(right_seam)
                            .child(right),
                    )
                    .child(div().absolute().top_0().left_0().right_0().child(title_bar))
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::OrgGate => {
                let card = self.render_org_gate(cx);
                root.child(card)
            }
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };
        let root = if restart_required {
            let restart = self.render_signed_out_restart(cx);
            root.child(restart)
        } else {
            root
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(
                    &theme,
                    false,
                    false,
                    cx.entity_id(),
                    cx,
                ))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(
                    &theme,
                    true,
                    false,
                    cx.entity_id(),
                    cx,
                ))
            }
            SplashPhase::FadingOutQuick => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(
                    &theme,
                    true,
                    true,
                    cx.entity_id(),
                    cx,
                ))
            }
            SplashPhase::Gone => root,
        };

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above the splash and every auth/org/error gate as well as
        // the full application. Gate pages also need a drag surface because
        // they do not render the unified tabs/settings titlebar — on Windows
        // the native `Drag` control area, on Linux the explicit
        // `start_window_move` strip (the control-area hit-test is inert
        // there); macOS drags gate windows natively.
        let root = if (!restart_required && matches!(gate, GatePhase::Ready))
            || cfg!(target_os = "macos")
        {
            root
        } else {
            root.child(
                self.titlebar_drag_region(
                    "gate-titlebar-drag",
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(Theme::TITLEBAR_HEIGHT)),
                    cx,
                ),
            )
        };
        root.children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::CHAT_PANEL_MIN;

    #[test]
    fn every_default_shortcut_binds_on_this_platform() {
        // `apply_keymap` silently falls back on an unparseable combo, so a
        // default gpui cannot parse would ship as a dead shortcut.
        for id in crate::settings::ShortcutId::ALL {
            let combo = platform_combo(id.default_combo());
            assert!(
                Keystroke::parse(&combo).is_ok(),
                "{} default {combo:?} does not parse",
                id.label()
            );
        }
    }

    #[test]
    fn right_pane_ceiling_preserves_the_chat_floor() {
        assert_eq!(right_pane_max_width(1200.0, 256.0), 644.0);
        assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
        // The chat floor wins over the right pane's preferred 360px minimum
        // when the whole window is unusually narrow.
        assert_eq!(right_pane_max_width(800.0, 256.0), 244.0);
        assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
    }

    #[test]
    fn right_pane_takeover_consumes_the_chat_column() {
        assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
        assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
    }

    #[test]
    fn right_pane_takeover_control_reverses_direction() {
        assert_eq!(tabs::right_pane_expand_icon(false), icons::EXPAND_ARROWS);
        assert_eq!(tabs::right_pane_expand_icon(true), icons::COLLAPSE_ARROWS);
    }

    #[test]
    fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
        assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
    }

    #[test]
    fn new_session_action_lives_in_the_titlebar_only_when_useful() {
        assert_eq!(titlebar_new_session_alpha(true, true), 1.0);
        assert_eq!(titlebar_new_session_alpha(true, false), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, true), 0.0);
        assert_eq!(titlebar_new_session_alpha(false, false), 0.0);
    }

    #[test]
    fn right_panel_content_keeps_the_larger_width_only_during_transition() {
        assert_eq!(right_panel_content_width(520.0, None, None), 520.0);
        assert_eq!(
            right_panel_content_width(0.0, Some((520.0, 0.0)), None),
            520.0
        );
        assert_eq!(
            right_panel_content_width(760.0, Some((520.0, 760.0)), None),
            760.0
        );
        assert_eq!(
            right_panel_content_width(1064.0, Some((520.0, 1064.0)), Some(760.0)),
            760.0
        );

        let conversation = conversation_width(1320.0, 256.0, 520.0);
        let takeover = conversation_width(1320.0, 256.0, 1064.0);
        assert_eq!(conversation, 544.0);
        assert_eq!(takeover, 0.0);
        assert_eq!(
            stable_panel_content_width(takeover, Some((conversation, takeover))),
            conversation
        );
        assert_eq!(
            stable_panel_content_width(conversation, Some((takeover, conversation))),
            conversation
        );
    }

    #[tokio::test]
    async fn remote_shutdown_waits_for_ipc_release() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(listener);
        });

        wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
            .await
            .unwrap();
        release.await.unwrap();
    }

    #[tokio::test]
    async fn signed_out_synced_runtime_stops_and_reboots_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session.json"),
            r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let boot = EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: zeron_proto::HarnessId::Mock,
        };
        let synced = crate::state::EngineHandle::bootstrap(boot.clone())
            .await
            .expect("saved session opens its synced profile");
        assert_eq!(synced.engine_info().workspace_scope, WorkspaceScope::Synced);

        synced
            .client()
            .call(methods::SIGN_OUT, serde_json::json!({}))
            .await
            .expect("sign out clears credentials");
        stop_synced_runtime(synced, port, dir.path())
            .await
            .expect("synced runtime drains and releases ownership");

        assert!(!dir.path().join("session.json").exists());
        let local = crate::state::EngineHandle::bootstrap(boot)
            .await
            .expect("same process can continue locally");
        assert_eq!(local.engine_info().workspace_scope, WorkspaceScope::Local);
        local.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_shutdown_waits_for_engine_lock_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let lock_released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_by_task = lock_released.clone();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(lock);
            released_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
            .await
            .unwrap();
        assert!(lock_released.load(std::sync::atomic::Ordering::SeqCst));
        release.await.unwrap();
    }

    #[tokio::test]
    async fn remote_shutdown_times_out_while_ipc_remains_open() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let error = wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_millis(100))
            .await
            .unwrap_err();

        assert!(error.contains("did not finish stopping"));
        drop(listener);
    }

    #[test]
    fn account_actions_follow_the_attached_workspace_scope() {
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Local), SyncFlow::Idle),
            Some(AccountMenuAction::EnableSync)
        );
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
            Some(AccountMenuAction::SignOut)
        );
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Development), SyncFlow::Idle),
            None
        );
    }

    #[test]
    fn local_sign_in_offers_the_in_place_switch() {
        let signed_in = AuthState::SignedIn {
            user: zeron_proto::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };

        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Enabling,
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: true }
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Idle,
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: true },
            "another viewport derives the pending switch from AuthStatus"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SwitchOffer { notice_open: false },
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: false },
            "shared auth updates do not reopen a postponed wizard"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::RestartPending { notice_open: false },
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::RestartPending { notice_open: false },
            "the quit fallback survives shared auth updates too"
        );
        assert_eq!(
            account_menu_action(
                Some(WorkspaceScope::Local),
                SyncFlow::SwitchOffer { notice_open: false },
            ),
            Some(AccountMenuAction::RestartPending)
        );
        for notice_open in [true, false] {
            assert_eq!(
                sync_flow_after_auth(
                    SyncFlow::SwitchOffer { notice_open },
                    Some(WorkspaceScope::Local),
                    Some(&AuthState::SignedOut),
                ),
                SyncFlow::Idle,
                "revoked credentials cancel the pending switch"
            );
        }
    }

    #[test]
    fn import_summary_errors_are_a_failure_not_a_success() {
        // Clean summary → done with counts.
        let clean = serde_json::json!({
            "kind": "summary", "importedChats": 2, "skippedChats": 1, "errors": []
        });
        assert_eq!(import_summary_outcome(&clean), Ok((2, 1)));

        // Any error means the wizard must NOT say "all set" — partial
        // migrations surface as an explicit failure with the first cause.
        let partial = serde_json::json!({
            "kind": "summary", "importedChats": 1, "skippedChats": 0,
            "errors": ["chat c2: journal copy failed"]
        });
        let message = import_summary_outcome(&partial).expect_err("errors must fail");
        assert!(message.contains("journal copy failed"), "{message}");
        assert!(message.contains("1 imported"), "{message}");

        let many = serde_json::json!({
            "kind": "summary", "importedChats": 0, "skippedChats": 0,
            "errors": ["a", "b", "c"]
        });
        let message = import_summary_outcome(&many).expect_err("errors must fail");
        assert!(message.contains("3 failures"), "{message}");

        // A summary missing the errors field entirely (older engine) is
        // treated as clean rather than failing every import.
        let legacy = serde_json::json!({ "kind": "summary", "importedChats": 4 });
        assert_eq!(import_summary_outcome(&legacy), Ok((4, 0)));
    }

    #[test]
    fn spaces_only_local_work_still_gets_the_import_offer() {
        assert_eq!(local_work_phrase(0, 0), None, "nothing to bring");
        assert_eq!(local_work_phrase(2, 0).as_deref(), Some("the 2 sessions"));
        assert_eq!(
            local_work_phrase(0, 1).as_deref(),
            Some("the 1 project"),
            "a projects-only profile must be offered the import, not a bare switch"
        );
        assert_eq!(
            local_work_phrase(1, 2).as_deref(),
            Some("the 1 session and 2 projects")
        );
    }

    #[test]
    fn dismissed_import_failure_stays_reachable_on_a_synced_runtime() {
        let signed_in = AuthState::SignedIn {
            user: zeron_proto::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };

        // "Later" postpones the failure notice; it must not evaporate.
        let dismissed = SyncFlow::ImportFailed { notice_open: false };
        assert_eq!(
            sync_flow_after_auth(dismissed, Some(WorkspaceScope::Synced), Some(&signed_in)),
            dismissed,
            "a postponed import failure survives auth/scope updates"
        );

        // …and the account menu on the SYNCED runtime still exposes the
        // re-entry point. This is the whole point: after the switch there is
        // no local runtime left to re-derive an offer from, so this menu row
        // is the only path back to the retry dialog.
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), dismissed),
            Some(AccountMenuAction::RestartPending),
            "retry must remain reachable after dismissal"
        );
        assert_eq!(
            account_menu_action(
                Some(WorkspaceScope::Synced),
                SyncFlow::ImportFailed { notice_open: true },
            ),
            Some(AccountMenuAction::RestartPending)
        );

        // Resolving the failure restores the normal synced menu.
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
            Some(AccountMenuAction::SignOut)
        );
    }

    #[test]
    fn switch_lifecycle_survives_the_runtime_replacement_window() {
        let signed_in = AuthState::SignedIn {
            user: zeron_proto::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };
        for flow in [
            SyncFlow::Switching { import: true },
            SyncFlow::Importing { done: 1, total: 3 },
            SyncFlow::ImportDone {
                imported: 3,
                skipped: 0,
            },
            SyncFlow::ImportFailed { notice_open: true },
            SyncFlow::ImportFailed { notice_open: false },
        ] {
            // Local (before the stop), detached (mid-replacement), and synced
            // (replacement runtime up): the driver owns these states — auth
            // and scope edges must never reset them.
            assert_eq!(
                sync_flow_after_auth(flow, Some(WorkspaceScope::Local), Some(&signed_in)),
                flow
            );
            assert_eq!(sync_flow_after_auth(flow, None, None), flow);
            assert_eq!(
                sync_flow_after_auth(flow, Some(WorkspaceScope::Synced), Some(&signed_in)),
                flow
            );
        }
    }

    #[test]
    fn synced_sign_out_blocks_every_viewport_and_cannot_switch_accounts() {
        let signed_in_as_another_user = AuthState::SignedIn {
            user: zeron_proto::UserProfile {
                id: "user-2".into(),
                email: "other@example.com".into(),
                name: None,
            },
            org_id: Some("org-2".into()),
        };

        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SigningOut,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            SyncFlow::SignedOutRestartRequired,
            "the viewport that requested sign-out is blocked by AuthStatus"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Idle,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            SyncFlow::SignedOutRestartRequired,
            "another viewport observing the same runtime is also blocked"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SignedOutRestartRequired,
                Some(WorkspaceScope::Synced),
                Some(&signed_in_as_another_user),
            ),
            SyncFlow::SignedOutRestartRequired,
            "new credentials cannot reopen the previous account's store"
        );
    }

    #[test]
    fn titlebar_cluster_matches_zeron_window_controls() {
        // zeron window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
        assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
        assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
        assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
        assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
        assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
        assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
        assert_eq!(
            TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
            titlebar_cluster_start(false),
            "the rendered row padding and spacer must land on the declared cluster start"
        );
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 0, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
    }

    #[test]
    fn linux_caption_controls_reserve_titlebar_space() {
        // 24px buttons on the cluster's 2px rhythm.
        assert_eq!(caption_buttons_width(0), 0.0);
        assert_eq!(caption_buttons_width(1), 24.0);
        assert_eq!(caption_buttons_width(3), 76.0);
        // Right-side captions (the Linux default: minimize,maximize,close):
        // content pads past the 10px edge inset + the button row.
        assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
        // GNOME-vanilla ":close" — a single right button.
        assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
        // Left-side captions ("close:…" layouts) shift the app cluster right
        // by the button row + one 2px gap.
        assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
        assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
        assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
        // macOS ignores the Linux caption count entirely.
        assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..92; a 16px-padded header needs 84 more px to
        // put content at 92 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
        assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
        // Linux with a left-side close caption: everything shifts one slot.
        assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 0, 16.0),
            88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 0, 16.0),
            12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: zeron sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        // Everything closed until explicitly opened (user request — the
        // brief default-open popped the pane on every visited session).
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        assert_eq!(panels.get("a").right_active, RightSurface::Picker);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true,
                ..Default::default()
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
        // The right pane round-trips back closed.
        assert!(!panels.toggle_changes("a"));
        assert!(!panels.get("a").changes_open);
    }

    #[test]
    fn session_panels_update_tracks_right_surfaces() {
        let mut panels = SessionPanels::default();
        panels.update("a", |p| p.right_active = RightSurface::Diff(3));
        assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
        // Other chats keep the picker default.
        assert_eq!(panels.get("b").right_active, RightSurface::Picker);
        panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
        assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn sidebar_chat_height_tracks_visible_metadata() {
        assert_eq!(chat_row_height(false, false), 45.0);
        assert_eq!(chat_row_height(true, false), 61.0);
        assert_eq!(chat_row_height(false, true), 63.0);
        assert_eq!(chat_row_height(true, true), 63.0);
    }

    #[test]
    fn sidebar_harness_geometry_reflects_row_hierarchy() {
        assert_eq!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP, Theme::SPACE_SM);
        assert!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP < SIDEBAR_ARCHIVED_HARNESS_TITLE_GAP);
        assert!(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE < SIDEBAR_ARCHIVED_HARNESS_ICON_SIZE);
    }

    #[test]
    fn sidebar_height_change_is_not_a_reorder() {
        let open = keys(&[("first-group", 105.0), ("second-group", 240.0)]);
        let collapsed = keys(&[("first-group", 40.0), ("second-group", 240.0)]);
        assert!(!sidebar_key_order_changed(&open, &collapsed));

        let reordered = keys(&[("second-group", 240.0), ("first-group", 40.0)]);
        assert!(sidebar_key_order_changed(&collapsed, &reordered));
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — zeron's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    #[test]
    fn sidebar_disclosure_motion_lands_exactly_on_its_target() {
        let mut tween = SidebarDisclosureMotion::new(1, 240.0, 0.0);
        tween.started = std::time::Instant::now() - motion::COLLAPSE.total().mul_f32(2.0);
        assert_eq!(tween.current(), 0.0);
        assert!(!tween.animating());
    }
}
