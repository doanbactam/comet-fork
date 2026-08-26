//! Headed cold-start milestones (L5).
//!
//! Timestamps are milliseconds since [`mark_process_start`]. Events are logged
//! once each at `info` so a cold launch leaves a readable trail without an env
//! gate; set `ZERON_BOOT_STATS=0` to silence.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static LOGGED_READY: AtomicBool = AtomicBool::new(false);
static LOGGED_SPLASH_GONE: AtomicBool = AtomicBool::new(false);
static LOGGED_FIRST_TRANSCRIPT: AtomicBool = AtomicBool::new(false);
static LOGGED_WINDOW: AtomicBool = AtomicBool::new(false);

fn enabled() -> bool {
    !std::env::var("ZERON_BOOT_STATS").is_ok_and(|v| v == "0")
}

fn elapsed_ms() -> Option<u128> {
    EPOCH.get().map(|t0| t0.elapsed().as_millis())
}

/// Call once at the top of [`crate::run_app`] — the process→Ready clock origin.
pub fn mark_process_start() {
    let _ = EPOCH.get_or_init(Instant::now);
}

/// Main window opened (first paint may still be the splash).
pub fn mark_window_open() {
    if !enabled() || LOGGED_WINDOW.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(ms) = elapsed_ms() {
        tracing::info!(event = "window_open", ms, "boot_stats");
    }
}

/// Engine attached and [`crate::state::ConnectionStatus::Ready`].
pub fn mark_engine_ready() {
    if !enabled() || LOGGED_READY.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(ms) = elapsed_ms() {
        tracing::info!(event = "engine_ready", ms, "boot_stats");
    }
}

/// Splash overlay removed — the shell is interactable without the fade veil.
pub fn mark_splash_gone() {
    if !enabled() || LOGGED_SPLASH_GONE.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(ms) = elapsed_ms() {
        tracing::info!(event = "splash_gone", ms, "boot_stats");
    }
}

/// First `WatchDocMessages` reset for the boot-selected (or user-selected) chat.
pub fn mark_first_transcript_frame() {
    if !enabled() || LOGGED_FIRST_TRANSCRIPT.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(ms) = elapsed_ms() {
        tracing::info!(event = "first_transcript_frame", ms, "boot_stats");
    }
}

/// Milliseconds since process start, for adaptive splash decisions.
pub fn elapsed_since_start() -> Option<std::time::Duration> {
    EPOCH.get().map(Instant::elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_monotonic_once_marked() {
        mark_process_start();
        let a = elapsed_since_start().expect("epoch");
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = elapsed_since_start().expect("epoch");
        assert!(b >= a);
    }
}
