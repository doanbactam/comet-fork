//! Stick-to-bottom spring (mugen §1e).

use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Stick-to-bottom spring (mugen §1e — same constants as its DEFAULT_SPRING,
// which follows the shape of stackblitz/use-stick-to-bottom)
// ---------------------------------------------------------------------------

/// Retains velocity frame-to-frame (higher = more glide).
pub const SPRING_DAMPING: f32 = 0.7;
/// Pull toward the target (higher = snappier).
pub const SPRING_STIFFNESS: f32 = 0.05;
/// Inertia (higher = slower to start/stop).
pub const SPRING_MASS: f32 = 1.25;
/// Reference frame for the fixed-timestep integration (60fps).
pub const SPRING_FRAME_MS: f32 = 1000.0 / 60.0;
/// Cap on simulated frames per tick — a hitch catches up instead of teleporting.
pub const SPRING_MAX_CATCHUP_FRAMES: f32 = 8.0;
/// EMA rate for the feed-forward target-growth estimate.
pub const SPRING_GROWTH_EMA: f32 = 0.12;
/// While streaming, chase up to this many px above the true bottom (keeps the
/// growing tail visible instead of hugging a moving edge).
pub const SPRING_CHASE_MAX_LEAD: f32 = 32.0;
/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;

/// A live stream already resting at the end should keep that end anchored as
/// its measured height grows. This is deliberately narrower than `pinned`:
/// users gliding back toward the bottom keep the normal spring behavior.
pub(super) fn should_anchor_live_stream(
    pinned: bool,
    distance_from_bottom: f32,
    streaming: bool,
) -> bool {
    pinned && streaming && distance_from_bottom <= AT_BOTTOM_PX
}

/// Keep the spring loop warm this long after landing, so a streaming pause
/// resumes at cruise instead of re-accelerating from zero.
pub const SPRING_SETTLE_GRACE_MS: u64 = 500;
/// Teleport when farther than this many viewports from the end; glide the rest.
pub const GLIDE_MAX_VIEWPORTS: f32 = 2.5;
/// A freshly-sent prompt rests this far below the transcript viewport's top.
/// The titlebar overlays the full-height list, so its height is part of the
/// inset; the extra 10px matches the first row's breathing room.
pub(crate) const OWN_SEND_TOP_INSET_PX: f32 = Theme::TITLEBAR_HEIGHT + 10.0;
/// Epsilon of extra height under the reservation. The runway ends AT the
/// app's bottom — this is not scroll room (24px of it read as a janky
/// overshoot-and-fight zone, user report) — it exists only to keep the held
/// layout out of gpui's shorter-than-viewport regime, where a bottom-aligned
/// list reports no item bounds (sizing goes blind) and position becomes a
/// function of content height instead of the hold. Two pixels of travel is
/// below perception.
pub(super) const OWN_SEND_SCROLL_SLACK_PX: f32 = 2.0;
/// Per-60fps-frame fraction of the remaining entry glide retained (~90%
/// covered in ~230ms, ease-out).
pub(super) const OWN_SEND_GLIDE_RETAIN: f32 = 0.85;
/// The entry glide snaps to the absolute hold within this error.
pub(super) const OWN_SEND_GLIDE_SNAP_PX: f32 = 1.0;

/// The reservation a held turn still needs: the room under the prompt's
/// top-inset position (`usable` = viewport minus inset and bottom chrome)
/// not yet consumed by the turn's own content. Zero once the reply has
/// filled the reserved space — the notes-app `minHeight` analogue.
pub(super) fn own_turn_reservation(usable: f32, turn_height: f32) -> f32 {
    (usable - turn_height).max(0.0)
}

/// Pure stick-to-bottom spring stepper — the mugen `tick()` integration:
/// velocity relaxes toward `(damping·v + stiffness·diff)/mass` per 60fps
/// sub-frame, position advances by `v + target_vel` where `target_vel` is a
/// feed-forward EMA of target growth px/frame, and the chase point sits up to
/// [`SPRING_CHASE_MAX_LEAD`] px above the true bottom proportional to growth.
#[derive(Debug, Clone, Copy)]
pub struct StickSpring {
    /// Spring velocity, px per 60fps frame.
    velocity: f32,
    /// Feed-forward: smoothed target growth, px per 60fps frame.
    target_vel: f32,
    /// Target observed at the previous tick (`None` = fresh/parked).
    last_target: Option<f32>,
}

impl Default for StickSpring {
    fn default() -> Self {
        Self::new()
    }
}

impl StickSpring {
    pub fn new() -> Self {
        Self {
            velocity: 0.0,
            target_vel: 0.0,
            last_target: None,
        }
    }

    /// Park the spring (drops all state; the next tick starts cold).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Residual motion below mugen's settle thresholds (`v < .05 && targetVel
    /// < .05`)?
    pub fn is_idle(&self) -> bool {
        self.velocity < 0.05 && self.target_vel < 0.05
    }

    #[cfg(test)]
    pub(crate) fn target_vel(&self) -> f32 {
        self.target_vel
    }

    /// Advance one tick. `pos`/`target` are scroll offsets in px (larger =
    /// closer to the bottom); `frames` is elapsed time in 60fps frames
    /// (clamped by the caller to [`SPRING_MAX_CATCHUP_FRAMES`]). Returns the
    /// new position: never overshoots `target`, monotone while approaching,
    /// and snaps exactly once within 0.5px.
    pub fn step(&mut self, mut pos: f32, target: f32, mut frames: f32) -> f32 {
        let grew = self.last_target.map_or(0.0, |last| target - last);
        self.last_target = Some(target);
        if grew < -1.0 {
            // Target shrank (row collapse/removal) — growth estimate is stale.
            self.target_vel = 0.0;
        } else {
            let observed = grew.max(0.0) / frames.max(0.25);
            self.target_vel += SPRING_GROWTH_EMA * (observed - self.target_vel);
        }
        let chase = target - (self.target_vel * 9.0).min(SPRING_CHASE_MAX_LEAD);
        let mut v = self.velocity;
        while frames > 0.0 {
            let h = frames.min(1.0);
            frames -= h;
            let diff = (chase - pos).max(0.0);
            v += h * ((SPRING_DAMPING * v + SPRING_STIFFNESS * diff) / SPRING_MASS - v);
            pos = (pos + (v + self.target_vel) * h).min(target);
        }
        self.velocity = v;
        if target - pos <= 0.5 { target } else { pos }
    }
}

