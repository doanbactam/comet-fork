//! Right pane: terminal dock, surface tabs, takeover expand.

use super::*;

impl Shell {

    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. Still hidden on the
    /// new-session canvas, where the titlebar carries no toggle to close it
    /// again (an earlier user request).
    pub(super) fn right_pane_open(&self, cx: &App) -> bool {
        !self.active_chat.is_empty() && self.panels.get(&self.panel_key(cx)).changes_open
    }


    /// The current chat's terminal flag (per-session, in-memory).
    pub(super) fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }


    pub(super) fn right_target(&self, cx: &App) -> f32 {
        if !self.right_pane_open(cx) {
            0.0
        } else {
            // Manual sizing preserves a usable conversation column. Takeover
            // intentionally consumes it completely. Both ride the sidebar
            // tween so toggling it remains seamless.
            let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            if self.right_pane_expanded {
                right_pane_takeover_width(self.viewport_width, sidebar_now)
            } else {
                self.settings
                    .right_pane_width
                    .min(right_pane_max_width(self.viewport_width, sidebar_now))
            }
        }
    }


    pub(super) fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git gate: the pane hosts terminals too (see `right_pane_open`).
        let from = self.right_target(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let from_main = conversation_width(self.viewport_width, sidebar_now, from);
        let was_expanded = self.right_pane_expanded;
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if !open {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        let to = self.right_target(cx);
        self.right_tween = Some(WidthTween::new(from, to));
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = was_expanded.then(|| {
            WidthTween::new(
                from_main,
                conversation_width(self.viewport_width, sidebar_now, to),
            )
        });
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }


    pub(super) fn right_terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.right_terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new_embedded(self.state.clone(), cx));
        self.right_terminal = Some(terminal.clone());
        terminal
    }


    /// The right pane's surface tabs in the STORED (drag-reorderable) order —
    /// `(surface, title)`; entries whose backing tab/entity is gone are
    /// skipped.
    pub(super) fn right_surface_rows(&self, cx: &App) -> Vec<(RightSurface, SharedString)> {
        let key = self.panel_key(cx);
        let stored: &[RightSurface] = self
            .right_tabs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let terminals: Vec<(u64, SharedString, bool)> = self
            .right_terminal
            .as_ref()
            .map(|t| t.read(cx).tab_summaries(cx))
            .unwrap_or_default();
        stored
            .iter()
            .filter_map(|surface| match surface {
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    // Contextual title (user request): the pane's scope
                    // label, or the pinned commit's subject.
                    .map(|changes| (*surface, changes.read(cx).tab_title())),
                RightSurface::Terminal(tab) => terminals
                    .iter()
                    .find(|(k, _, _)| k == tab)
                    .map(|(_, title, _)| (*surface, title.clone())),
                RightSurface::Subagent(id) => self
                    .subagent_tabs
                    .get(id)
                    .map(|tab| (*surface, tab.title.clone())),
                RightSurface::Picker => None,
            })
            .collect()
    }


    /// Drag-reorder a surface tab within this chat's strip.
    pub(super) fn reorder_right_tabs(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key)
            && from < tabs.len()
            && to < tabs.len()
            && from != to
        {
            let surface = tabs.remove(from);
            tabs.insert(to, surface);
            cx.notify();
        }
    }


    /// Track the hovered drop slot mid-drag (the terminal drawer's
    /// `update_drag_over`, ported: epoch bumps restart the slide tween).
    pub(super) fn update_right_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.right_tab_drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.right_tab_drag = Some(RightTabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }


    /// The surface that actually renders: the stored pick when it still
    /// exists, else the first remaining tab, else the picker. Terminal keys
    /// go stale when their tab closes/exits — never render a dead surface.
    pub(super) fn resolved_right_active(&self, cx: &App) -> RightSurface {
        let picked = self.panels.get(&self.panel_key(cx)).right_active;
        let rows = self.right_surface_rows(cx);
        let exists = match picked {
            RightSurface::Picker => false,
            surface => rows.iter().any(|(s, _)| *s == surface),
        };
        if exists {
            picked
        } else {
            rows.first()
                .map(|(s, _)| *s)
                .unwrap_or(RightSurface::Picker)
        }
    }


    pub(super) fn set_right_active(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        self.panels.update(&key, |p| p.right_active = surface);
        match surface {
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
            }
            RightSurface::Diff(id) => {
                if let Some(changes) = self.diffs.get(&id).cloned() {
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                }
            }
            // The tab's feed (watch or snapshot) runs from open to close —
            // activation needs no revalidation.
            RightSurface::Subagent(_) => {}
            RightSurface::Picker => {}
        }
        cx.notify();
    }


    /// The picker's Git card / the `+` menu's Diff row: every click opens a
    /// FRESH diff tab with its own scope/base selection (multiple diff
    /// panels, user request).
    pub(super) fn add_diff_surface(&mut self, cx: &mut Context<Self>) {
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.register_diff_surface(changes, cx);
    }


    /// A History row click: the commit opens as its own pinned diff tab
    /// (user request).
    pub(super) fn add_commit_diff_surface(
        &mut self,
        commit: zeron_proto::GitHistoryCommit,
        cx: &mut Context<Self>,
    ) {
        let changes = cx.new(|cx| Changes::for_commit(self.state.clone(), commit, cx));
        self.register_diff_surface(changes, cx);
    }


    pub(super) fn register_diff_surface(&mut self, changes: Entity<Changes>, cx: &mut Context<Self>) {
        self.diff_seq += 1;
        let id = self.diff_seq;
        let sub = cx.subscribe(&changes, |this: &mut Self, _, event, cx| match event {
            ChangesEvent::OpenCommit(commit) => {
                this.add_commit_diff_surface(commit.clone(), cx);
            }
        });
        self.diffs.insert(id, changes);
        self.diff_subs.insert(id, sub);
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Diff(id));
        self.set_right_active(RightSurface::Diff(id), cx);
    }


    /// The picker's Terminal card / the `+` menu's Terminal row: every click
    /// opens a fresh embedded terminal tab.
    pub(super) fn add_terminal_surface(&mut self, cx: &mut Context<Self>) {
        let panel = self.right_terminal_panel(cx);
        let opened = panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.open_tab_for_selected(cx)
        });
        if let Some(tab) = opened {
            let key = self.panel_key(cx);
            self.right_tabs
                .entry(key)
                .or_default()
                .push(RightSurface::Terminal(tab));
            self.set_right_active(RightSurface::Terminal(tab), cx);
        }
    }


    /// A spawn chip's "Open subagent": focus the existing tab for that doc,
    /// or open one. `frozen` (subagent done/failed) tries the uploaded
    /// transcript blob first and falls back to the live doc watch; running
    /// subagents watch the doc directly.
    pub(super) fn add_subagent_surface(
        &mut self,
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
        cx: &mut Context<Self>,
    ) {
        // The chip lives in the conversation column — the pane it opens into
        // may still be closed.
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
        if let Some((&id, _)) = self
            .subagent_tabs
            .iter()
            .find(|(_, tab)| tab.doc_id == doc_id)
        {
            self.set_right_active(RightSurface::Subagent(id), cx);
            return;
        }
        self.subagent_seq += 1;
        let id = self.subagent_seq;
        // A live subagent follows its streaming end (main-transcript feel);
        // a frozen one reads top-down.
        let transcript =
            cx.new(|cx| Transcript::for_doc(self.state.clone(), doc_id.clone(), !frozen, cx));
        let events = cx.subscribe(&transcript, Self::on_transcript_event);
        let fetch = if frozen {
            self.spawn_subagent_snapshot_fetch(&chat_id, &doc_id, cx)
        } else {
            self.state
                .update(cx, |s, cx| s.watch_subagent_doc(doc_id.clone(), cx));
            None
        };
        self.subagent_tabs.insert(
            id,
            SubagentTab {
                doc_id,
                title: title.into(),
                transcript,
                _fetch: fetch,
                _events: events,
            },
        );
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Subagent(id));
        self.set_right_active(RightSurface::Subagent(id), cx);
    }


    /// Fetch a finished subagent's frozen transcript blob
    /// (`{chat_id}/{doc_id}`); on ANY failure fall back to watching the doc
    /// — the blob upload is best-effort engine-side.
    pub(super) fn spawn_subagent_snapshot_fetch(
        &self,
        chat_id: &str,
        doc_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.state
                .update(cx, |s, cx| s.watch_subagent_doc(doc_id.to_string(), cx));
            return None;
        };
        let blob_ref = format!("{chat_id}/{doc_id}");
        let state = self.state.clone();
        let doc_id = doc_id.to_string();
        Some(cx.spawn(async move |_, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": blob_ref }),
                Duration::from_secs(20),
            )
            .await;
            let entries: Option<Vec<zeron_doc::SessionMessageEntry>> = reply.ok().and_then(|v| {
                let text = v.get("text")?.as_str()?.to_owned();
                serde_json::from_str(&text).ok()
            });
            state.update(cx, |s, cx| {
                match entries {
                    Some(entries) => s.set_subagent_snapshot(doc_id, entries),
                    None => s.watch_subagent_doc(doc_id, cx),
                }
                cx.notify();
            });
        }))
    }


    /// A surface tab's ✕. The active fallback happens naturally through
    /// [`Self::resolved_right_active`] on the next frame.
    pub(super) fn close_right_surface(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key) {
            tabs.retain(|s| *s != surface);
        }
        match surface {
            RightSurface::Diff(id) => {
                // Dropping the entity tears down its diff watch.
                self.diffs.remove(&id);
                self.diff_subs.remove(&id);
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.close_tab_by_key(tab, window, cx));
            }
            RightSurface::Subagent(id) => {
                // Unwatch drops the watch task — that cancels the engine-side
                // watch and unpins the subagent doc from the engine LRU.
                if let Some(tab) = self.subagent_tabs.remove(&id) {
                    self.state
                        .update(cx, |s, _| s.unwatch_subagent_doc(&tab.doc_id));
                }
            }
            RightSurface::Picker => {}
        }
        self.panels.update(&key, |p| {
            if p.right_active == surface {
                p.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }


    pub(super) fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }


    pub(super) fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }


    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (zeron `sessionPanels`).
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (zeron terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in zeron's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }


    pub(super) fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // No arbitrary percentage ceiling, but retain the chat's usable 300px
        // floor instead of allowing the conversation to collapse to zero.
        let max = right_pane_max_width(viewport, self.sidebar_target());
        self.settings.right_pane_width = if max >= RIGHT_PANE_MIN {
            width.clamp(RIGHT_PANE_MIN, max)
        } else {
            max
        };
        self.right_tween = None;
        self.right_takeover_content_tween = None;
        self.main_takeover_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }


    /// Right-anchored variant for the changes pane. The outer width follows the
    /// existing shell tween, while descendants retain the larger endpoint's
    /// geometry for that 200ms transition. This mirrors the sidebar's stable
    /// inner/clipped outer behavior without changing the center column's
    /// upstream flex layout.
    pub(super) fn right_pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        let takeover_width = self
            .active_tween_endpoints(self.right_takeover_content_tween)
            .map(|_| self.eval_tween(self.right_takeover_content_tween, target));
        let content_width =
            right_panel_content_width(target, self.active_tween_endpoints(tween), takeover_width);
        div()
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(content_width))
                    .child(inner),
            )
            .into_any_element()
    }


    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    pub(super) fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its 5px read as
        // dead air between the seam and the tab bar (user report).
        let inner = div()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(panel))
            .child(handle.absolute().top_0().left_0().right_0());

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    pub(super) fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (zeron's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(crate::typography::ui_rems(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::gradient_spinner(
                    "sending-indicator",
                    &theme,
                    2.5,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right pane — the surface host (t3code RightPanelTabs): hidden by
    /// default, drag-resizable. Content is the ACTIVE surface — the Diff
    /// page (its options row + the lazy [`Changes`] viewer), an embedded
    /// terminal, or the surface picker when no tabs exist.
    pub(super) fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            match self.resolved_right_active(cx) {
                RightSurface::Diff(id) if self.diffs.contains_key(&id) => {
                    let changes = self.diffs.get(&id).cloned().expect("checked");
                    // Idempotent — also covers a persisted-open pane on boot.
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                    // The diff options (scope dropdown, ref selector,
                    // fold-all) moved DOWN from the titlebar band — the
                    // surface tabs own that row now; the expand/close
                    // buttons stayed up there (user request).
                    let controls =
                        changes.update(cx, |changes, cx| changes.render_header_controls(cx));
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(px(36.0))
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(controls),
                        )
                        .child(div().flex_1().min_h_0().child(changes))
                        .into_any_element()
                }
                RightSurface::Terminal(tab) => {
                    let panel = self.right_terminal_panel(cx);
                    // Keep the embedded panel's own active tab aligned with
                    // the resolved surface (fallbacks can move it).
                    let resize_suspended = self.tween_active(self.right_tween);
                    panel.update(cx, |panel, cx| {
                        panel.set_resize_suspended(resize_suspended);
                        panel.select_tab_by_key(tab, cx);
                    });
                    panel.into_any_element()
                }
                RightSurface::Subagent(id) if self.subagent_tabs.contains_key(&id) => {
                    let transcript = self
                        .subagent_tabs
                        .get(&id)
                        .expect("checked")
                        .transcript
                        .clone();
                    // The pane hosts its own jump pill: the conversation
                    // overlay's is bound to the PRIMARY transcript, and this
                    // one anchors to the pane (no composer stack to clear).
                    let pill = transcript.read(cx).jump_button_shown().then(|| {
                        div()
                            .absolute()
                            .bottom(px(16.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(self.jump_pill(
                                "subagent-jump-to-bottom",
                                "subagent-jump-pill",
                                transcript.clone(),
                                cx,
                            ))
                    });
                    // Read-only surface: the transcript fills the pane — no
                    // composer, no status strip.
                    div()
                        .size_full()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(div().flex_1().min_h_0().child(transcript))
                        .children(pill)
                        .into_any_element()
                }
                _ => self.render_surface_picker(cx),
            }
        } else {
            gpui::Empty.into_any_element()
        };
        // Flush panel (user request — the inset card is gone): full window
        // height with a left hairline, glass-friendly like the terminal dock
        // (translucent over the frost; solid otherwise). The resize grabber
        // lives outside this clipped container, on the root layout's seam.
        let panel_bg = if theme.is_glass() {
            bg.opacity(0.4)
        } else {
            bg
        };
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            // In takeover the panel's left edge IS the sidebar seam, which
            // already carries the sidebar tone's right hairline — a second
            // border there doubled up (user report).
            .when(!self.right_pane_expanded, |el| {
                el.border_l_1().border_color(theme.border)
            })
            .bg(panel_bg)
            .overflow_hidden()
            // The titlebar is a glass overlay over the full-height content
            // row; the panel's own chrome starts below it.
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .child(content);
        let target = self.right_target(cx);
        self.right_pane_container(
            self.right_tween,
            target,
            div().h_full().relative().child(panel).into_any_element(),
        )
    }

    /// The right pane's empty state: a compact vertical list of surface rows
    /// (icon + label). The old two-card grid clipped in narrow panes and
    /// wasted short ones.
    pub(super) fn render_surface_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let text = theme.text;
        let muted = theme.text_muted;
        let border = theme.border;
        let border_strong = theme.border_strong;
        let row = |id: &'static str, icon_path: &'static str, title: &'static str| {
            div()
                .id(id)
                .w_full()
                .h(px(44.0))
                .px(px(14.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(border)
                .bg(crate::theme::ink(0.02))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(move |s| s.bg(crate::theme::ink(0.05)).border_color(border_strong))
                .child(icon(icon_path).size(px(15.0)).flex_none().text_color(muted))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text)
                        .child(SharedString::from(title)),
                )
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(280.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        row("surface-card-terminal", icons::TERMINAL, "Terminal").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.add_terminal_surface(cx);
                            }),
                        ),
                    )
                    // Git only where there IS git — the pane itself no
                    // longer gates on it (terminals work anywhere).
                    .when(self.space_git_detected(cx), |el| {
                        el.child(row("surface-card-git", icons::GIT_BRANCH, "Git").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.add_diff_surface(cx);
                            }),
                        ))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_signed_out_restart(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let runtime_change_label = if self.runtime_change_task.is_some() {
            "Stopping engine…"
        } else {
            "Retry local mode"
        };
        let card = div()
            .w(px(380.0))
            .px(px(32.0))
            .py(px(40.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .child(
                icon(icons::ZERON_LOGO)
                    .w(px(31.4))
                    .h(px(36.0))
                    .text_color(theme.text),
            )
            .child(
                div()
                    .mt(px(24.0))
                    .text_size(crate::typography::ui_rems(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Signed out")),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(24.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Zeron removed your credentials but could not finish closing the previous synced workspace. Retry before continuing in local mode.",
                    )),
            )
            .when_some(self.runtime_change_error.clone(), |card, error| {
                card.child(
                    div()
                        .mb(px(16.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                popover::btn_primary(&theme, runtime_change_label)
                    .id("signed-out-quit")
                    .when(self.runtime_change_task.is_some(), |button| {
                        button.opacity(0.6)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_local_runtime_transition(false, cx)
                    })),
            );

        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("signed-out-restart", card)),
            )
            .into_any_element()
    }

    pub(super) fn close_right_plus(&mut self, cx: &mut Context<Self>) {
        if self.right_plus.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.right_plus);
        }
        cx.notify();
    }

    /// The titlebar strip over the right pane: one chip per surface tab
    /// (icon · title · ✕) plus the `+` menu — the t3code RightPanelTabs bar,
    /// living in the top row; the diff options moved into the pane below.
    pub(super) fn render_right_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        /// Fixed chip slot — the terminal drawer's drag mechanics (drop-index
        /// quantisation + slide offsets) assume uniform widths.
        const CHIP_W: f32 = 112.0;
        const CHIP_SLOT: f32 = CHIP_W + 4.0; // + the strip's own gap

        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the strip.
        if self.right_tab_drag.is_some() && !cx.has_active_drag() {
            self.right_tab_drag = None;
        }
        let rows = self.right_surface_rows(cx);
        let count = rows.len();
        let active = self.resolved_right_active(cx);
        let drag = self
            .right_tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        // Fade flags from the LAST frame's scroll state (invisible lag).
        // The EdgeFade scope below fades per-pixel on x for glyphs AND
        // quads/images (fork 5d1f83d) — washes dissolve across the band.
        const FADE_WIDTH: f32 = 36.0;
        let scrolled = -f32::from(self.right_tab_scroll.offset().x);
        let max_scroll = f32::from(self.right_tab_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        // The old session-tab strip's proven scroll shape: the flex row IS
        // the scroller (id + overflow_x_scroll + track_scroll), wrapped in a
        // relative min_w_0 region below; drop math runs in CONTENT
        // coordinates (viewport-relative x plus the scrolled-off width).
        let scroll_for_drag = self.right_tab_scroll.clone();
        let mut strip = div()
            .id("right-surface-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.right_tab_scroll)
            .on_drag_move::<RightTabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<RightTabDrag>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.panel_key != this.panel_key(cx) {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x)
                        - f32::from(event.bounds.left())
                        - f32::from(scroll_for_drag.offset().x);
                    let over = crate::terminal::panel::drop_index(rel_x, CHIP_SLOT, count);
                    this.update_right_tab_drag_over(from, over, cx);
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(move |this, payload: &RightTabDrag, _, cx| {
                if payload.panel_key != this.panel_key(cx) {
                    this.right_tab_drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .right_tab_drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from);
                this.right_tab_drag = None;
                this.reorder_right_tabs(payload.from, to, cx);
            }));
        for (ix, (surface, title)) in rows.into_iter().enumerate() {
            let is_active = surface == active;
            let icon_path = match surface {
                RightSurface::Diff(_) => icons::GIT_BRANCH,
                RightSurface::Subagent(_) => icons::BOT,
                _ => icons::TERMINAL,
            };
            // A live subagent tab swaps its icon for the mini working
            // spinner (the history fetch button's in-flight recipe) — the
            // doc's streaming tail entry IS the run's liveness, so the swap
            // settles by itself when the subagent finishes.
            let subagent_running = match surface {
                RightSurface::Subagent(id) => self.subagent_tabs.get(&id).is_some_and(|tab| {
                    self.state
                        .read(cx)
                        .sub_transcript(&tab.doc_id)
                        .last()
                        .is_some_and(|e| e.status == Some(zeron_doc::MessageStatus::Streaming))
                }),
                _ => false,
            };
            // t3 tab hover: the surface icon swaps IN PLACE for the close ✕
            // (same slot, no width jump) — the ✕ only shows while the tab is
            // hovered (user request).
            let group: SharedString = format!("right-surface-tab-{ix}").into();
            let ghost_title = title.clone();
            let chip = div()
                .id(("right-surface-tab", ix))
                .group(group.clone())
                .h(px(24.0))
                .w(px(CHIP_W))
                .flex_none()
                .pl(px(4.0))
                .pr(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .cursor_pointer()
                // The old session-tab strip's solved carve-out: NOT
                // `.occlude()` — a BlockMouse hitbox ends the hit test,
                // so the scroll container behind the tabs never saw
                // wheel events and an overflowing strip could not be
                // scrolled (tabs tile the whole region). ExceptScroll
                // keeps the titlebar drag-region carve-out and lets the
                // strip scroll.
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .when(is_active, |el| el.bg(crate::theme::wash(0.10)))
                .when(!is_active, |el| {
                    el.hover(|s| s.bg(crate::theme::wash(0.06)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_right_active(surface, cx);
                }))
                // Middle-click closes, like every tab strip.
                .on_mouse_down(
                    gpui::MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        this.close_right_surface(surface, window, cx);
                    }),
                )
                .on_drag(
                    RightTabDrag {
                        panel_key: self.panel_key(cx),
                        from: ix,
                        title: ghost_title,
                    },
                    |payload, _point, _, cx| {
                        let title = payload.title.clone();
                        cx.stop_propagation();
                        cx.new(|_| SurfaceTabGhost { title })
                    },
                )
                .child(
                    // Leading slot: icon normally, ✕ on tab hover — two
                    // stacked layers opacity-swapped by the group hover.
                    div()
                        .id(("right-surface-close", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .relative()
                        .hover(|s| s.bg(crate::theme::wash(0.12)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_right_surface(surface, window, cx);
                        }))
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .group_hover(group.clone(), |s| s.opacity(0.0))
                                .child(if subagent_running {
                                    loaders::mini_glyph_spinner(
                                        format!("subagent-tab-{ix}"),
                                        2.0,
                                        theme.glyph,
                                        cx.entity_id(),
                                        cx,
                                    )
                                    .into_any_element()
                                } else {
                                    icon(icon_path)
                                        .size(px(12.0))
                                        .text_color(if is_active {
                                            theme.text_muted
                                        } else {
                                            theme.text_muted.opacity(0.7)
                                        })
                                        .into_any_element()
                                }),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .opacity(0.0)
                                .group_hover(group.clone(), |s| s.opacity(1.0))
                                .child(
                                    icon(icons::CLOSE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(if is_active {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(title),
                );
            // Sliding transform while a sibling drags over (the terminal
            // drawer's exact recipe): animate 150ms between committed
            // offsets; the dragged tab leaves an invisible spacer — the
            // ghost carries it.
            let wrapped: AnyElement = match drag {
                Some((from, over, epoch, prev_over)) if ix != from => {
                    let target = crate::terminal::panel::slide_offset(ix, from, over) * CHIP_SLOT;
                    let start =
                        crate::terminal::panel::slide_offset(ix, from, prev_over) * CHIP_SLOT;
                    div()
                        .relative()
                        .child(chip.with_animation(
                            ("right-tab-slide", (ix as u64) | ((epoch as u64) << 32)),
                            TAB_SLIDE.animation(),
                            move |el, t| el.left(px(motion::lerp(start, target, t))),
                        ))
                        .into_any_element()
                }
                Some((from, ..)) if ix == from => div()
                    .w(px(CHIP_W))
                    .h(px(24.0))
                    .flex_none()
                    .into_any_element(),
                _ => chip.into_any_element(),
            };
            strip = strip.child(wrapped);
        }
        // The `+` — a small menu offering the two surfaces (t3 "Add panel
        // surface"); mirrors the picker cards.
        let plus_open = self.right_plus.get().is_some();
        let plus_fade = "right-surface-add-fade";
        let mut plus = div()
            .id("right-surface-add")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                plus_fade,
                crate::theme::wash(0.0),
                crate::theme::wash(0.11),
            ))
            .on_hover(motion::hover_listener(plus_fade))
            .block_mouse_except_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.right_plus.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.right_plus.take_press_was_open() {
                    this.close_right_plus(cx);
                } else {
                    this.right_plus.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        if plus_open {
            let closing = self.right_plus.closing_since();
            let menu = popover::popover_card(&theme)
                .w(px(168.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_right_plus(cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            popover::menu_row(&theme, false, "right-plus-terminal")
                                .id("right-plus-terminal-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_terminal_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::TERMINAL)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Terminal")),
                        )
                        .when(self.space_git_detected(cx), |menu| {
                            menu.child(
                                popover::menu_row(&theme, false, "right-plus-diff")
                                    .id("right-plus-diff-row")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.add_diff_surface(cx);
                                        this.close_right_plus(cx);
                                    }))
                                    .child(
                                        icon(icons::GIT_BRANCH)
                                            .size(px(13.0))
                                            .text_color(theme.text_muted),
                                    )
                                    // "Git", not "Git diff" — the surface hosts
                                    // history and per-commit views too (user
                                    // request; matches the picker card).
                                    .child(SharedString::from("Git")),
                            )
                        }),
                )
                .into_any_element();
            plus = plus.relative().child(popover::anchored_menu_below_gap(
                "right-plus-menu",
                menu,
                closing,
                10.0,
            ));
        }
        // The empty-state picker already offers every surface. Show a single
        // Chrome-style add-tab affordance only after at least one tab exists.
        strip = strip.when(count > 0, |strip| strip.child(plus));
        // Edge fades on whichever side hides tabs (flags computed above).
        // Glass: per-glyph EdgeFade scope over the chips' own opacity ramps;
        // opaque: painted gradients in the shell surface tone.
        let glass = theme.is_glass();
        let bar_bg = theme.surface;
        let region = div()
            .relative()
            .min_w_0()
            .size_full()
            .flex()
            .items_center()
            .child(strip)
            .when(fade_left && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });
        if glass {
            crate::edge_fade::edge_faded(FADE_WIDTH, false, false, region)
                .fade_left(fade_left)
                .fade_right(fade_right)
                .into_any_element()
        } else {
            region.into_any_element()
        }
    }

    /// Toggle the changes-panel takeover (the header's expand button, t3code
    /// parity): the panel grows to fill everything right of the sidebar,
    /// hiding the conversation column; toggling back restores the saved
    /// width. Rides the same width tween as open/close so the jump glides.
    pub(super) fn toggle_right_pane_expand(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let from_main = conversation_width(self.viewport_width, sidebar_now, from);
        self.right_pane_expanded = !self.right_pane_expanded;
        let to = self.right_target(cx);
        let right_transition = WidthTween::new(from, to);
        self.right_tween = Some(right_transition);
        self.right_takeover_content_tween = Some(right_transition);
        self.main_takeover_tween = Some(WidthTween::new(
            from_main,
            conversation_width(self.viewport_width, sidebar_now, to),
        ));
        cx.notify();
    }
}
