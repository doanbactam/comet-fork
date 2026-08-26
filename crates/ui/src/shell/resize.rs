//! Pane resize drag markers and width tweens.

use super::*;

pub(super) struct SidebarResize;
/// Drag marker for the right-pane resize handle.
pub(super) struct RightPaneResize;

/// The dragged surface-tab payload (strip reorder).
pub(super) struct RightTabDrag {
    pub(super) panel_key: String,
    pub(super) from: usize,
    pub(super) title: SharedString,
}

/// Live drag-over state for the surface-tab strip — the terminal drawer's
/// [`crate::terminal::panel`] DragState, ported: `epoch` keys the 150ms
/// slide-animation restarts as the hovered slot changes.
pub(super) struct RightTabDragState {
    pub(super) from: usize,
    pub(super) over: usize,
    pub(super) epoch: usize,
    pub(super) prev_over: usize,
}

/// Ghost chip following the pointer while a surface tab drags.
pub(super) struct SurfaceTabGhost {
    pub(super) title: SharedString,
}

impl Render for SurfaceTabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .w(px(112.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}
/// Drag marker for the terminal-panel height handle.
pub(super) struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
pub(super) struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
pub(super) struct WidthTween {
    pub(super) from: f32,
    pub(super) to: f32,
    pub(super) started: std::time::Instant,
}

impl WidthTween {
    pub(super) fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}
