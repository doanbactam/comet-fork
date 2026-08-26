//! Sign-in, org gate, local↔synced profile transitions.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncFlow {
    Idle,
    Enabling,
    Canceling,
    /// Signed in on a local runtime: the wizard's choice step (bring local
    /// work / start fresh / later). `notice_open: false` = postponed, badge
    /// in the account menu.
    SwitchOffer {
        notice_open: bool,
    },
    /// Stopping the local runtime and bootstrapping the synced one in-place.
    Switching {
        import: bool,
    },
    /// The one-time import stream is running on the new synced runtime.
    Importing {
        done: usize,
        total: usize,
    },
    /// Import finished; the success step stays until dismissed.
    ImportDone {
        imported: usize,
        skipped: usize,
    },
    /// The import stream reported errors or died early. Explicit retry step —
    /// structural idempotence makes re-running safe (only missing rows copy).
    /// Details ride `runtime_change_error`. `notice_open: false` = postponed:
    /// the dialog is hidden but the failure stays pending, reachable through
    /// the account menu — dismissal must never discard the only retry
    /// entry point (under Synced scope the menu otherwise offers just
    /// Sign out, and the local rows would be unreachable).
    ImportFailed {
        notice_open: bool,
    },
    RestartPending {
        notice_open: bool,
    },
    SignOutConfirm,
    SigningOut,
    SignedOutRestartRequired,
}

impl SyncFlow {
    /// States the in-place switch driver owns end-to-end — auth/scope edges
    /// must not reset them while the runtime is being replaced under the UI.
    pub(super) fn is_switch_lifecycle(self) -> bool {
        matches!(
            self,
            SyncFlow::Switching { .. }
                | SyncFlow::Importing { .. }
                | SyncFlow::ImportDone { .. }
                | SyncFlow::ImportFailed { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountMenuAction {
    EnableSync,
    SyncInProgress,
    /// Postponed switch wizard (or legacy restart fallback) — reopen it.
    RestartPending,
    SignOut,
}

const RUNTIME_CHANGE_TIMEOUT: Duration = Duration::from_secs(10);
const RUNTIME_CHANGE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Wait until a stopped daemon can no longer win the next bootstrap probe and
/// has released the data directory for the replacement runtime.
pub(super) async fn wait_for_remote_engine_shutdown(
    ipc_port: u16,
    data_dir: &std::path::Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let port_closed = !matches!(
            tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
            )
            .await,
            Ok(Ok(_))
        );
        if port_closed && InstanceLock::holder(data_dir).is_none() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the daemon did not finish stopping within {} seconds",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(RUNTIME_CHANGE_POLL_INTERVAL).await;
    }
}

/// Stop the engine that owns the synced profile and wait until a local runtime
/// can safely acquire both its IPC port and data-directory lock.
pub(super) async fn stop_synced_runtime(
    engine: crate::state::EngineHandle,
    ipc_port: u16,
    data_dir: &std::path::Path,
) -> Result<(), String> {
    let stop_error = if matches!(engine.mode(), EngineMode::Remote { .. }) {
        engine
            .client()
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    engine.shutdown().await;
    match wait_for_remote_engine_shutdown(ipc_port, data_dir, RUNTIME_CHANGE_TIMEOUT).await {
        Ok(()) => Ok(()),
        Err(error) => match stop_error {
            Some(stop_error) => Err(format!("{stop_error}; {error}")),
            None => Err(error),
        },
    }
}

/// What an import-summary stream item means for the wizard: `Ok((imported,
/// skipped))` only when the engine reported zero errors; otherwise the
/// user-facing failure message. Pure so the partial-failure path is testable.
pub(super) fn import_summary_outcome(item: &serde_json::Value) -> Result<(usize, usize), String> {
    let count = |key: &str| item.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let errors: Vec<&str> = item
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if errors.is_empty() {
        return Ok((count("importedChats"), count("skippedChats")));
    }
    let first = errors.first().copied().unwrap_or("unknown error");
    Err(if errors.len() == 1 {
        format!("{} imported, 1 failure: {first}", count("importedChats"))
    } else {
        format!(
            "{} imported, {} failures — first: {first}",
            count("importedChats"),
            errors.len()
        )
    })
}

/// The offer step's description of what a switch would bring along, or `None`
/// when the local profile holds nothing importable. Spaces count as work:
/// a projects-only profile must get the import choice too.
pub(super) fn local_work_phrase(chats: usize, spaces: usize) -> Option<String> {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (chats, spaces) {
        (0, 0) => None,
        (c, 0) => Some(format!("the {}", plural(c, "session"))),
        (0, s) => Some(format!("the {}", plural(s, "project"))),
        (c, s) => Some(format!(
            "the {} and {}",
            plural(c, "session"),
            plural(s, "project")
        )),
    }
}

pub(super) fn account_menu_action(scope: Option<WorkspaceScope>, flow: SyncFlow) -> Option<AccountMenuAction> {
    match scope {
        Some(WorkspaceScope::Local) => match flow {
            SyncFlow::Idle => Some(AccountMenuAction::EnableSync),
            SyncFlow::Enabling | SyncFlow::Canceling => Some(AccountMenuAction::SyncInProgress),
            SyncFlow::SwitchOffer { .. } | SyncFlow::RestartPending { .. } => {
                Some(AccountMenuAction::RestartPending)
            }
            SyncFlow::ImportFailed { .. } => Some(AccountMenuAction::RestartPending),
            SyncFlow::Switching { .. }
            | SyncFlow::Importing { .. }
            | SyncFlow::ImportDone { .. } => Some(AccountMenuAction::SyncInProgress),
            SyncFlow::SignOutConfirm
            | SyncFlow::SigningOut
            | SyncFlow::SignedOutRestartRequired => None,
        },
        Some(WorkspaceScope::Synced) => match flow {
            SyncFlow::SignedOutRestartRequired => None,
            // A pending import failure must stay reachable: this is the only
            // surface that can reopen the retry dialog on a synced runtime.
            SyncFlow::ImportFailed { .. } => Some(AccountMenuAction::RestartPending),
            _ if flow.is_switch_lifecycle() => Some(AccountMenuAction::SyncInProgress),
            _ => Some(AccountMenuAction::SignOut),
        },
        Some(WorkspaceScope::Development) | None => None,
    }
}

pub(super) fn sync_flow_after_auth(
    flow: SyncFlow,
    scope: Option<WorkspaceScope>,
    auth: Option<&AuthState>,
) -> SyncFlow {
    match scope {
        Some(WorkspaceScope::Local) => match (flow, auth) {
            // The in-place switch owns its own lifecycle once started.
            (flow, _) if flow.is_switch_lifecycle() => flow,
            // AuthStatus belongs to the runtime, not to the Shell that opened
            // the browser. Every attached viewport must advertise the pending
            // profile switch once any of them completes sign-in.
            (SyncFlow::SwitchOffer { .. }, Some(AuthState::SignedOut)) => SyncFlow::Idle,
            (SyncFlow::RestartPending { .. }, Some(AuthState::SignedOut)) => SyncFlow::Idle,
            (SyncFlow::Canceling, Some(AuthState::SignedIn { .. })) => flow,
            (SyncFlow::SwitchOffer { .. }, Some(AuthState::SignedIn { .. })) => flow,
            (SyncFlow::RestartPending { .. }, Some(AuthState::SignedIn { .. })) => flow,
            (_, Some(AuthState::SignedIn { .. })) => SyncFlow::SwitchOffer { notice_open: true },
            _ => flow,
        },
        Some(WorkspaceScope::Synced) => match auth {
            // AuthStatus is shared by every viewport attached to the runtime.
            // Once a synced store loses its credentials, every Shell must stop:
            // letting another viewport sign in would authenticate a new account
            // while the engine still serves the previous account's fixed store.
            Some(AuthState::SignedOut) => SyncFlow::SignedOutRestartRequired,
            _ => match flow {
                SyncFlow::SignOutConfirm
                | SyncFlow::SigningOut
                | SyncFlow::SignedOutRestartRequired => flow,
                flow if flow.is_switch_lifecycle() => flow,
                _ => SyncFlow::Idle,
            },
        },
        Some(WorkspaceScope::Development) => SyncFlow::Idle,
        None => flow,
    }
}

/// The "Create your workspace" gate (feature-inventory §1.2 OrgGate).
pub(super) struct OrgGateUi {
    name_input: Entity<ComposerInput>,
    orgs: Loadable<Vec<OrgRow>>,
    submitting: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _events: Subscription,
}

impl Shell {

    pub(super) fn request_sign_out(&mut self, cx: &mut Context<Self>) {
        self.close_user_menu(cx);
        if self.state.read(cx).workspace_scope != Some(WorkspaceScope::Synced) {
            return;
        }
        self.sync_flow = SyncFlow::SignOutConfirm;
        cx.notify();
    }


    pub(super) fn confirm_sign_out(&mut self, cx: &mut Context<Self>) {
        self.start_local_runtime_transition(true, cx);
    }


    pub(super) fn start_local_runtime_transition(&mut self, sign_out: bool, cx: &mut Context<Self>) {
        if self.runtime_change_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            self.sync_flow = SyncFlow::SignedOutRestartRequired;
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::SigningOut;
        self.runtime_change_error = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let shutdown_dir = data_dir.clone();
        let transition = Tokio::spawn(cx, async move {
            if sign_out {
                engine
                    .client()
                    .call(methods::SIGN_OUT, serde_json::json!({}))
                    .await
                    .map_err(|error| format!("Sign out failed: {error}"))?;
            }
            stop_synced_runtime(engine, ipc_port, &shutdown_dir).await
        });
        let state = self.state.clone();
        let boot = self.boot.clone();
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match transition.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(()) => {
                        shell.sync_flow = SyncFlow::Idle;
                        shell.runtime_change_error = None;
                        shell.org = None;
                        shell.route = Route::Chat;
                        shell.space_boot_applied = false;
                        state.update(cx, |state, cx| state.prepare_runtime_replacement(cx));
                        AppState::bootstrap(state.clone(), boot, cx);
                    }
                    Err(error) => {
                        shell.sync_flow = SyncFlow::SignedOutRestartRequired;
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn cancel_auth_setup(&mut self, cx: &mut Context<Self>) {
        let local = self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let pending_auth = self.auth_task.take();
        let pending_org = self.org.as_mut().and_then(|org| org.task.take());
        if local {
            self.sync_flow = SyncFlow::Canceling;
        }
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            // Do not race SignOut against an exchange or organization write
            // that can still persist a session after credentials were cleared.
            if let Some(task) = pending_auth {
                task.await;
            }
            if let Some(task) = pending_org {
                task.await;
            }
            let result = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.org = None;
                        if local {
                            shell.sync_flow = SyncFlow::Idle;
                        }
                    }
                    Err(err) => {
                        if local {
                            shell.sync_flow = SyncFlow::Enabling;
                        }
                        shell.sidebar_notice =
                            Some(format!("Could not cancel sign-in: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn postpone_sync_restart(&mut self, cx: &mut Context<Self>) {
        match self.sync_flow {
            SyncFlow::RestartPending { .. } => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: false };
            }
            SyncFlow::SwitchOffer { .. } => {
                self.sync_flow = SyncFlow::SwitchOffer { notice_open: false };
            }
            SyncFlow::ImportFailed { .. } => {
                self.sync_flow = SyncFlow::ImportFailed { notice_open: false };
            }
            _ => return,
        }
        cx.notify();
    }


    pub(super) fn reopen_sync_notice(&mut self, cx: &mut Context<Self>) {
        self.close_user_menu(cx);
        match self.sync_flow {
            SyncFlow::RestartPending { .. } => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            }
            SyncFlow::SwitchOffer { .. } => {
                self.sync_flow = SyncFlow::SwitchOffer { notice_open: true };
            }
            SyncFlow::ImportFailed { .. } => {
                self.sync_flow = SyncFlow::ImportFailed { notice_open: true };
            }
            _ => return,
        }
        cx.notify();
    }


    /// The wizard's choice step chose a path: stop the local runtime, boot the
    /// synced one in-place (mirror of the sign-out transition), then let
    /// [`Self::drive_sync_switch`] run the import once the runtime is ready.
    /// Failure falls back to the quit-and-reopen dialog — the local profile is
    /// untouched, so the old path is always a safe exit.
    pub(super) fn start_synced_switch(&mut self, import: bool, cx: &mut Context<Self>) {
        if self.runtime_change_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::Switching { import };
        self.runtime_change_error = None;
        self.import_current = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let transition = Tokio::spawn(cx, async move {
            stop_synced_runtime(engine, ipc_port, &data_dir).await
        });
        let state = self.state.clone();
        let boot = self.boot.clone();
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match transition.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(()) => {
                        // Keep `Switching { import }`: the state observer sees
                        // the replacement runtime reach Ready and advances the
                        // wizard from there.
                        shell.org = None;
                        shell.route = Route::Chat;
                        shell.space_boot_applied = false;
                        state.update(cx, |state, cx| state.prepare_runtime_replacement(cx));
                        AppState::bootstrap(state.clone(), boot, cx);
                    }
                    Err(error) => {
                        shell.sync_flow = SyncFlow::RestartPending { notice_open: true };
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }


    /// Advance the in-place switch when the replacement runtime lands: Ready +
    /// Synced starts the import stream (or finishes immediately when the user
    /// chose a fresh start); a runtime that comes back non-synced fell out of
    /// the swap — surface the quit fallback rather than pretend.
    pub(super) fn drive_sync_switch(&mut self, cx: &mut Context<Self>) {
        let SyncFlow::Switching { import } = self.sync_flow else {
            return;
        };
        if self.runtime_change_task.is_some() {
            return; // still stopping the local runtime
        }
        let (ready, scope) = {
            let state = self.state.read(cx);
            (
                matches!(state.connection, ConnectionStatus::Ready),
                state.workspace_scope,
            )
        };
        if !ready {
            if let ConnectionStatus::Failed(error) = &self.state.read(cx).connection {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
                self.runtime_change_error = Some(error.clone().into());
                cx.notify();
            }
            return;
        }
        match scope {
            Some(WorkspaceScope::Synced) => {
                if import {
                    self.spawn_local_import(cx);
                } else {
                    self.sync_flow = SyncFlow::Idle;
                    cx.notify();
                }
            }
            Some(_) => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
                self.runtime_change_error =
                    Some("The synced workspace did not come up — restart to finish.".into());
                cx.notify();
            }
            None => {}
        }
    }


    /// Subscribe to the engine's one-time import stream and mirror its
    /// progress into the wizard.
    pub(super) fn spawn_local_import(&mut self, cx: &mut Context<Self>) {
        if self.import_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            self.runtime_change_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::Importing { done: 0, total: 0 };
        self.runtime_change_error = None;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let stream = Tokio::spawn(cx, async move {
            let mut items = engine
                .client()
                .subscribe(methods::IMPORT_LOCAL_WORKSPACE, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())?;
            while let Some(item) = items.recv().await {
                let _ = tx.send(item);
            }
            Ok::<(), String>(())
        });
        self.import_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let item = rx.recv().await;
                let ended = item.is_none();
                this.update(cx, |shell, cx| {
                    if let Some(item) = &item {
                        shell.apply_import_event(item, cx);
                    }
                    if ended {
                        shell.import_task = None;
                        shell.import_current = None;
                        // A stream that died before its summary is a failure —
                        // offer the in-place retry (idempotent).
                        if matches!(shell.sync_flow, SyncFlow::Importing { .. }) {
                            shell.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                            shell.runtime_change_error =
                                Some("The import stream ended before it finished.".into());
                        }
                        cx.notify();
                    }
                })
                .ok();
                if ended {
                    break;
                }
            }
            if let Ok(Err(error)) = stream.await {
                this.update(cx, |shell, cx| {
                    shell.import_task = None;
                    if matches!(shell.sync_flow, SyncFlow::Importing { .. }) {
                        shell.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                })
                .ok();
            }
        }));
        cx.notify();
    }


    pub(super) fn apply_import_event(&mut self, item: &serde_json::Value, cx: &mut Context<Self>) {
        match item.get("kind").and_then(|k| k.as_str()) {
            Some("start") => {
                let total = item.get("chats").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                self.sync_flow = SyncFlow::Importing { done: 0, total };
            }
            Some("chat") => {
                let index = item.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let total = item.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                self.import_current = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|t| SharedString::from(t.to_string()));
                self.sync_flow = SyncFlow::Importing { done: index, total };
            }
            Some("summary") => {
                self.import_current = None;
                // A summary with errors is a FAILED import, however normally
                // the stream ended — never present a partial migration as
                // complete (the engine keeps collecting per-item failures
                // precisely so this can be surfaced).
                match import_summary_outcome(item) {
                    Ok((imported, skipped)) => {
                        self.sync_flow = SyncFlow::ImportDone { imported, skipped };
                    }
                    Err(message) => {
                        self.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                        self.runtime_change_error = Some(message.into());
                    }
                }
            }
            _ => return,
        }
        cx.notify();
    }


    pub(super) fn quit_for_runtime_change(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        if engine.mode() == EngineMode::InProcess {
            cx.quit();
            return;
        }
        if self.runtime_change_task.is_some() {
            return;
        }

        self.runtime_change_error = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let shutdown = Tokio::spawn(cx, async move {
            engine
                .client()
                .call(methods::STOP_ENGINE, serde_json::json!({}))
                .await
                .map_err(|err| err.to_string())?;
            wait_for_remote_engine_shutdown(ipc_port, &data_dir, RUNTIME_CHANGE_TIMEOUT).await
        });
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match shutdown.await {
                Ok(result) => result,
                Err(err) => Err(err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(_) => cx.quit(),
                    Err(err) => {
                        shell.runtime_change_error = Some(format!(
                            "Could not stop the remote engine: {err}. Run `zeron daemon stop`, then quit and reopen Zeron."
                        ).into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let scope = self.state.read(cx).workspace_scope;
        if scope == Some(WorkspaceScope::Development) {
            return;
        }
        self.close_user_menu(cx);
        if scope == Some(WorkspaceScope::Local) {
            self.sync_flow = SyncFlow::Enabling;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                    cx.notify();
                }
                Err(err) => {
                    if scope == Some(WorkspaceScope::Local) && shell.sync_flow == SyncFlow::Enabling
                    {
                        shell.sync_flow = SyncFlow::Idle;
                    }
                    shell.sidebar_notice = Some(format!("Sign in failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        let name_input = cx.new(|cx| ComposerInput::new("Workspace name", cx));
        let events = cx.subscribe(&name_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.create_org(cx);
            }
        });
        self.org = Some(OrgGateUi {
            name_input,
            orgs: Loadable::Idle,
            submitting: false,
            error: None,
            task: None,
            _events: events,
        });
        self.load_orgs(cx);
    }


    pub(super) fn load_orgs(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.orgs = Loadable::Loading;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ORGS, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.orgs = match result {
                        Ok(value) => Loadable::Ready(sort_memberships(parse_orgs(&value))),
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn create_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        let name = org.name_input.read(cx).text().trim().to_string();
        if !org_name_valid(&name) {
            org.error = Some("Enter a workspace name".into());
            cx.notify();
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CREATE_ORG, serde_json::json!({ "name": name }))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                    // Success: the AuthStatus stream flips to SignedIn and the
                    // gate falls away on its own.
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }


    pub(super) fn select_org(&mut self, organization_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SELECT_ORG,
                    serde_json::json!({ "organizationId": organization_id }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }


    /// Scope-aware sidebar identity and account menu. Local runtimes advertise
    /// their storage boundary and offer sync; synced runtimes offer sign-out.
    pub(super) fn render_user_menu(
        &mut self,
        user_line: SharedString,
        trigger_subline: Option<SharedString>,
        menu_identity: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu.is_open();
        let action = account_menu_action(self.state.read(cx).workspace_scope, self.sync_flow);
        // Bottom-of-sidebar identity: avatar circle + scope/account label and
        // its secondary status line.
        let initial: SharedString = user_line
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            // user-menu.tsx trigger: hover `bg-white/[0.04]`, open state
            // (`data-[state=open]`) the slightly stronger `bg-white/[0.06]`;
            // the hover wash fades over `transition-colors`.
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.user_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                // A press that found the menu open closes it (the card's
                // mouse-down-out already began the close) — never reopen.
                if this.user_menu.take_press_was_open() {
                    this.close_user_menu(cx);
                } else {
                    this.user_menu.open(());
                }
                cx.notify();
            }))
            .child(
                // Avatar: white circle, initial in near-black (zeron user-menu.tsx).
                div()
                    .size(px(28.0))
                    .flex_none()
                    .rounded_full()
                    .bg(theme.text)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.bg)
                    .child(initial),
            )
            .child(
                // Name with an optional status line underneath — no chip on the right.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .line_height(px(17.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .truncate()
                            .child(user_line.clone()),
                    )
                    .when_some(trigger_subline, |identity, subline| {
                        identity.child(
                            div()
                                .text_size(crate::typography::ui_rems(11.0))
                                .line_height(px(15.0))
                                .text_color(theme.text_muted)
                                .child(subline),
                        )
                    }),
            );
        if self.user_menu.get().is_some() {
            let closing = self.user_menu.closing_since();
            // user-menu.tsx content: `w-[--radix-dropdown-menu-trigger-width]`
            // (exactly as wide as the trigger row — sidebar minus its p-2
            // gutters), `flex-col gap-0.5`, then: one small muted email line
            // (`px-2 pb-1 pt-1.5 text-[11px] text-muted-foreground/70`),
            // the action selected by the runtime scope, then "Settings".
            let menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_user_menu(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .truncate()
                        .child(menu_identity),
                )
                .when_some(action, |menu, action| {
                    let row = match action {
                        AccountMenuAction::EnableSync => {
                            popover::menu_row(theme, false, "user-menu-enable-sync")
                                .id("user-menu-enable-sync")
                                .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                                .child(
                                    icon(icons::GLOBAL)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Enable sync"))
                                .into_any_element()
                        }
                        AccountMenuAction::SyncInProgress => {
                            popover::menu_row(theme, false, "user-menu-sync-progress")
                                .id("user-menu-sync-progress")
                                .opacity(0.6)
                                .child(
                                    icon(icons::GLOBAL)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Sync setup in progress"))
                                .into_any_element()
                        }
                        AccountMenuAction::RestartPending => {
                            popover::menu_row(theme, false, "user-menu-sync-restart")
                                .id("user-menu-sync-restart")
                                .on_click(cx.listener(|this, _, _, cx| this.reopen_sync_notice(cx)))
                                .child(
                                    icon(icons::RESTART)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Finish sync setup"))
                                .into_any_element()
                        }
                        AccountMenuAction::SignOut => {
                            popover::menu_row(theme, false, "user-menu-signout")
                                .id("user-menu-signout")
                                .on_click(cx.listener(|this, _, _, cx| this.request_sign_out(cx)))
                                .child(
                                    icon(icons::LOGOUT_2)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Sign out"))
                                .into_any_element()
                        }
                    };
                    menu.child(row).child(popover::menu_separator())
                })
                .child(
                    popover::menu_row(theme, false, "user-menu-settings")
                        .id("user-menu-settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsSection::Devices, cx)
                        }))
                        .child(
                            icon(icons::SETTINGS_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Settings")),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_above(
                "user-menu-popover",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }


    pub(super) fn render_sync_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let needs_org = matches!(
            self.state.read(cx).auth.as_ref(),
            Some(AuthState::NeedsOrganization { .. })
        );
        let remote_engine = self
            .state
            .read(cx)
            .engine()
            .is_some_and(|engine| matches!(engine.mode(), EngineMode::Remote { .. }));
        let runtime_change_label = if self.runtime_change_task.is_some() {
            "Stopping engine…"
        } else if remote_engine {
            "Stop daemon and quit"
        } else {
            "Quit Zeron"
        };

        if self.sync_flow == SyncFlow::Enabling && needs_org {
            return Some(self.render_org_gate(cx));
        }

        let signed_in_email: Option<SharedString> = match self.state.read(cx).auth.as_ref() {
            Some(AuthState::SignedIn { user, .. }) => Some(SharedString::from(user.email.clone())),
            _ => None,
        };
        // Spaces count as local work too: a projects-only profile must get
        // the import choice, not a bare "Switch now".
        let (local_chats, local_spaces) = {
            let state = self.state.read(cx);
            (state.chats.len(), state.spaces.len())
        };
        let work_phrase = local_work_phrase(local_chats, local_spaces);

        let card = match self.sync_flow {
            SyncFlow::Enabling => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Enable sync"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Finish signing in in your browser. Zeron will keep using this local workspace until you quit and reopen.",
                    )),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "sync-enable-cancel")
                                .id("sync-enable-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_auth_setup(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Open browser again")
                                .id("sync-enable-open-browser")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_sign_in(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::Canceling => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Canceling sync setup…"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Removing the partial sign-in before returning to your local workspace.",
                    )),
                )
                .into_any_element(),
            // ── in-place switch wizard ────────────────────────────────────
            SyncFlow::SwitchOffer { notice_open: true } => {
                let has_local_work = work_phrase.is_some();
                let body: SharedString = match (&signed_in_email, &work_phrase) {
                    (Some(email), Some(phrase)) => format!(
                        "You're signed in as {email}. Bring {phrase} from this device into your synced workspace, or start it fresh."
                    )
                    .into(),
                    (Some(email), None) => format!(
                        "You're signed in as {email}. Zeron can switch to your synced workspace now."
                    )
                    .into(),
                    (None, Some(phrase)) => format!(
                        "Bring {phrase} from this device into your synced workspace, or start it fresh."
                    )
                    .into(),
                    (None, None) => "Zeron can switch to your synced workspace now.".into(),
                };
                let mut actions = div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Later", "sync-switch-later")
                            .id("sync-switch-later")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.postpone_sync_restart(cx)
                            })),
                    );
                if has_local_work {
                    actions = actions
                        .child(
                            popover::btn_ghost(&theme, "Start fresh", "sync-switch-fresh")
                                .id("sync-switch-fresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_synced_switch(false, cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Bring my work")
                                .id("sync-switch-import")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_synced_switch(true, cx)
                                })),
                        );
                } else {
                    actions = actions.child(
                        popover::btn_primary(&theme, "Switch now")
                            .id("sync-switch-now")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_synced_switch(false, cx)
                            })),
                    );
                }
                popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "Sync is ready"))
                    .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, body)))
                    .child(actions)
                    .into_any_element()
            }
            SyncFlow::Switching { import } => popover::dialog_card(&theme)
                .child(popover::dialog_title(
                    &theme,
                    "Switching to your synced workspace…",
                ))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    if import {
                        "Handing the engine over to your account. Your local sessions come along next."
                    } else {
                        "Handing the engine over to your account."
                    },
                )))
                .into_any_element(),
            SyncFlow::Importing { done, total } => {
                let fraction = if total == 0 {
                    0.0
                } else {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                };
                let label: SharedString = if total == 0 {
                    "Looking for local sessions…".into()
                } else {
                    format!("Importing session {} of {total}", (done + 1).min(total)).into()
                };
                let mut card = popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "Bringing your work over"))
                    .child(
                        div()
                            .mt(px(6.0))
                            .child(popover::dialog_body(&theme, label)),
                    );
                if let Some(current) = self.import_current.clone() {
                    card = card.child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted)
                            .overflow_hidden()
                            .child(current),
                    );
                }
                card.child(
                    // Determinate progress: a hairline track with an accent fill.
                    div()
                        .mt(px(14.0))
                        .h(px(4.0))
                        .w_full()
                        .rounded(px(2.0))
                        .bg(theme.border)
                        .child(
                            div()
                                .h_full()
                                .rounded(px(2.0))
                                .bg(theme.accent_strong)
                                .w(gpui::relative(fraction.max(0.04))),
                        ),
                )
                .into_any_element()
            }
            SyncFlow::ImportDone { imported, skipped } => {
                let body: SharedString = match (imported, skipped) {
                    (0, 0) => "Your synced workspace is ready.".into(),
                    (n, 0) => format!(
                        "{n} session{} moved into your synced workspace.",
                        if n == 1 { "" } else { "s" },
                    )
                    .into(),
                    (n, s) => format!(
                        "{n} session{} imported, {s} already present.",
                        if n == 1 { "" } else { "s" },
                    )
                    .into(),
                };
                popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "You're all set"))
                    .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, body)))
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .child(
                                popover::btn_primary(&theme, "Continue")
                                    .id("sync-switch-done")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sync_flow = SyncFlow::Idle;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            SyncFlow::ImportFailed { notice_open: true } => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Import didn't finish"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    "Anything already imported is kept; retrying only copies what's missing.",
                )))
                .when_some(self.runtime_change_error.clone(), |card, error| {
                    card.child(
                        div()
                            .mt(px(10.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(17.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Later", "import-failed-dismiss")
                                .id("import-failed-dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.postpone_sync_restart(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Retry import")
                                .id("import-failed-retry")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.spawn_local_import(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::RestartPending { notice_open: true } => popover::dialog_card(&theme)
                .child(popover::dialog_title(
                    &theme,
                    "Sync needs a restart",
                ))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        if remote_engine {
                            "Zeron is using a background daemon. Stop it and quit Zeron, then reopen to start the synced workspace. Existing local sessions stay on this device and will not be uploaded."
                        } else {
                            "Quit and reopen Zeron to start the synced workspace. Existing local sessions stay on this device and will not be uploaded."
                        },
                    )),
                )
                .when_some(self.runtime_change_error.clone(), |card, error| {
                    card.child(
                        div()
                            .mt(px(10.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(17.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Later", "sync-restart-later")
                                .id("sync-restart-later")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.postpone_sync_restart(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, runtime_change_label)
                                .id("sync-restart-quit")
                                .when(self.runtime_change_task.is_some(), |button| {
                                    button.opacity(0.6)
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.quit_for_runtime_change(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::SignOutConfirm => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Sign out?"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Zeron will remove your credentials, close the synced workspace, and continue in local mode.",
                    )),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "signout-cancel")
                                .id("signout-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sync_flow = SyncFlow::Idle;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Sign out")
                                .id("signout-confirm")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_sign_out(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::SigningOut => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Signing out…"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Removing account credentials and closing the synced workspace.",
                    )),
                )
                .into_any_element(),
            SyncFlow::Idle
            | SyncFlow::SwitchOffer { notice_open: false }
            | SyncFlow::ImportFailed { notice_open: false }
            | SyncFlow::RestartPending { notice_open: false }
            | SyncFlow::SignedOutRestartRequired => return None,
        };

        Some(popover::modal("sync-lifecycle-dialog", viewport, card))
    }


    pub(super) fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (zeron Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(14.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card (zeron App.tsx Gate): centered card on the grid —
            // logo, "Log in to Zeron", copy, full-width white Log in button.
            _ => div()
                .w(px(360.0))
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
                        .child(SharedString::from("Log in to Zeron")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens your browser to finish logging in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(crate::typography::ui_rems(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Log in")),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (zeron App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            GatePhase::SignIn => "gate-card-signin",
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }


    /// Organization onboarding used by the synced gate and, for a local
    /// runtime, only after the user explicitly starts the sync opt-in.
    pub(super) fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let local_setup = self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local);
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let submitting = org.submitting;
        let error = org.error.clone();
        let name_input = org.name_input.clone();
        let orgs = org.orgs.clone();

        let email: Option<SharedString> = self
            .state
            .read(cx)
            .auth_user()
            .map(|u| u.email.clone().into());

        let memberships: AnyElement =
            match &orgs {
                Loadable::Idle | Loadable::Loading => div()
                    .mt(px(24.0))
                    .child(popover::skeleton_rows(
                        "org-skeleton",
                        &theme,
                        2,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element(),
                Loadable::Error(message) => div()
                    .mt(px(24.0))
                    .child(
                        popover::error_row(&theme, message).child(
                            div()
                                .id("orgs-retry")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()))
                                .on_click(cx.listener(|this, _, _, cx| this.load_orgs(cx)))
                                .child(SharedString::from("Retry")),
                        ),
                    )
                    .into_any_element(),
                Loadable::Ready(rows) if rows.is_empty() => Empty.into_any_element(),
                Loadable::Ready(rows) => div()
                    .mt(px(24.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb(px(8.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Or continue in a workspace you belong to",
                            )),
                    )
                    .child(div().flex().flex_col().gap(px(4.0)).children(
                        rows.iter().enumerate().map(|(ix, row)| {
                            let org_id = row.organization_id.clone();
                            div()
                                .id(("org-row", ix))
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .text_size(crate::typography::ui_rems(13.0))
                                .text_color(theme.text)
                                .when(submitting, |el| el.opacity(0.5))
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::wash(0.11)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_org(org_id.clone(), cx);
                                }))
                                .child(SharedString::from(row.name.clone()))
                        }),
                    ))
                    .into_any_element(),
            };

        // zeron App.tsx OrgGate: w-400 card on the grid — logo, headline,
        // explainer (+ signed-in email), name form with a white Create button,
        // then existing memberships and the account escape hatch.
        let blurb: SharedString = match email {
            Some(email) => format!(
                "Zeron is organized around workspaces — create one for yourself or your team. Signed in as {email}."
            )
            .into(),
            None => {
                "Zeron is organized around workspaces — create one for yourself or your team."
                    .into()
            }
        };
        let card = div()
            .w(px(400.0))
            .px(px(32.0))
            .py(px(36.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                icon(icons::ZERON_LOGO)
                    .w(px(24.4))
                    .h(px(28.0))
                    .text_color(theme.text),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .text_size(crate::typography::ui_rems(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Create your workspace")),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(24.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(blurb),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .px(px(12.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .text_size(crate::typography::ui_rems(13.0))
                            .child(name_input),
                    )
                    .child(
                        div()
                            .id("create-org")
                            .h(px(36.0))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .bg(theme.text)
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.on_solid)
                            .when(submitting, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| this.create_org(cx)))
                            .child(SharedString::from(if submitting {
                                "Creating…"
                            } else {
                                "Create"
                            })),
                    ),
            )
            .child(memberships)
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(16.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.danger_muted.opacity(0.9)) // red-300
                        .child(message),
                )
            })
            .child(
                div().mt(px(24.0)).flex().flex_row().child(
                    div()
                        .id("org-signout")
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_auth_setup(cx)))
                        .child(SharedString::from(if local_setup {
                            "Cancel sync setup"
                        } else {
                            "Use a different account"
                        })),
                ),
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
                    .child(motion::fade_in("org-gate-card", card)),
            )
            .into_any_element()
    }
}
