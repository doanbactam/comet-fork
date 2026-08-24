# Transcript snapshot cost baseline

## Product goal

Establish a reproducible measurement of the per-tick cost the UI pays to hand off transcript state from the engine to the render layer, so that subsequent optimisations (borrow instead of clone, revision gating, Arc-per-entry) can prove they improve rather than regress.

## Decisions

- The bench lives in `crates/doc/tests/transcript_snapshot_cost.rs` and runs via `cargo test --release -p zeron-doc transcript_snapshot_cost -- --nocapture`. No new dependencies; `std::time::Instant` only.
- Synthetic transcript: 200 entries, one "live" entry ~800 KB, total text ~1.6 MB, with Tool parts carrying `output: Option<String>` to reflect real per-part allocation cost.
- Three measurements: (a) `transcript.clone()` median over 100 runs — the deep clone `transcript.rs::sync` does every notify tick; (b) `apply_transcript_frame` with 100 consecutive `TextAppend` ops — the streaming hot path; (c) total text bytes.
- The bench does NOT modify product code. It imports `zeron_doc::transcript_delta::{apply_transcript_frame, TextAppend, TranscriptFrame}` and `zeron_doc::schema::{SessionMessageEntry, MessageRole}` via the crate's public API.

## Verification tier

Tier 1: compile and run the bench test in release mode; confirm `cargo fmt --check -p zeron-doc` and that the bench file introduces no new clippy warnings.

## Verification result

### Environment

| Field | Value |
|-------|-------|
| Machine | E2B sandbox (orb) |
| OS | Debian GNU/Linux 12 (bookworm), x86_64 |
| Kernel | 6.1.158+ |
| Rust | stable 1.98.0 (88d9e12ae 2026-08-18) |
| Build profile | `release` (optimized) |
| CPU | x86_64 (software Vulkan / llvmpipe) |

### Baseline numbers (before optimisation)

```
═══════════════════════════════════════════════════════════════
  transcript snapshot cost bench
═══════════════════════════════════════════════════════════════
  entries:         200
  total text bytes: 1673705 (1634.5 KB)

  (a) transcript.clone() median (100 runs):  0.127 ms
  (b) apply TextAppend median (100×100 ticks): 0.145 µs/tick
═══════════════════════════════════════════════════════════════
```

### Checks

- `cargo fmt --check -p zeron-doc` — passes.
- `cargo clippy -p zeron-doc --test transcript_snapshot_cost -- -D warnings` — no warnings in bench file. (Pre-existing clippy errors in `zeron-doc` lib source — `large_enum_variant` on `MessagePart`/`SessionCommandEntry`, `explicit_counter_loop` in `parts.rs`, `unnecessary_clone` in `transcript_delta.rs` — are not introduced by this change.)
- `cargo test --release -p zeron-doc` — all tests pass (87 unit tests, 1 attachments integration test, 1 bench test).

## After bậc 0+0.5 (Prompt 1)

### Changes

- **Part A (bậc 0):** Removed 3 `transcript.clone()` call sites in `crates/ui/src/composer.rs` (×2) and `crates/ui/src/rail.rs` (×1) by borrowing in scope instead of cloning the full `Vec<SessionMessageEntry>`. Also removed a redundant `.to_vec()` on `pending_echoes()`.
- **Part B (bậc 0.5):** Added `transcript_rev: u64` to `AppState`, bumped on every mutation that changes what `Transcript::sync` would render (9 sites: `apply_transcript`, `apply_transcript_frame`, `set_subagent_snapshot`, `push_echo`, `remove_echo`, `select_chat`, `apply_chats` selected-chat-vanished, `unwatch_subagent_doc`, `prepare_runtime_replacement`). Added `synced_rev` to the `Transcript` entity with an early-exit gate in `fn sync` that skips the deep clone + row diff when the rev is unchanged and no chat-attach edge is pending.

### Numbers

```
═══════════════════════════════════════════════════════════════
  transcript snapshot cost bench
═══════════════════════════════════════════════════════════════
  entries:         200
  total text bytes: 1673705 (1634.5 KB)

  (a) transcript.clone() median (100 runs):  0.131 ms
  (b) apply TextAppend median (100×100 ticks): 0.145 µs/tick
═══════════════════════════════════════════════════════════════
```

The bench measures the raw cost of `transcript.clone()` and `apply_transcript_frame` on `Vec<SessionMessageEntry>`, which is unchanged by Prompt 1. The improvement from Part A is that 3 clone call sites in the UI no longer execute; the improvement from Part B is that `sync` itself short-circuits before reaching the clone when the rev is unchanged. Neither changes the per-clone or per-apply cost.

### Checks

- `cargo fmt -p zeron-ui -- --check` — our 4 changed files (state.rs, transcript.rs, composer.rs, rail.rs) are clean. Pre-existing fmt diffs in attachments.rs, loaders.rs, terminal/panel.rs are not from this change.
- `cargo clippy -p zeron-ui` — no new warnings in our changed files.
- `cargo test -p zeron-ui` — 553 passed, 0 failed.
- `cargo test --workspace` — all pass except 1 pre-existing failure in `zeron-engine` (`git_inspector_uses_origin_without_upstream_in_a_multi_remote_checkout` — SSH vs HTTPS URL, unrelated).

## After bậc 1 (Prompt 2 — Arc-per-entry)

### Changes

- `crates/doc/src/transcript_delta.rs`: Added `EntrySlot` trait with `entry()`, `entry_mut()`, `wrap()` methods. Impl for `SessionMessageEntry` (identity) and `Arc<SessionMessageEntry>` (COW via `Arc::make_mut`). Made `apply_transcript_frame` generic over `S: EntrySlot`. Wire types (`TranscriptFrame`, `TranscriptUpsert`, `TextAppend`) stay on plain `SessionMessageEntry` — no serde "rc" feature needed.
- `crates/doc/src/transcript_delta.rs` tests: 7 new twin tests for `Vec<Arc<SessionMessageEntry>>` (round-trip, streaming tick, both desync cases, large change reset) + 1 COW property test (refcount=1 mutates in place, snapshot held → old entry unchanged).
- `crates/ui/src/state.rs`: `AppState.transcript`, `echoes`, `sub_transcripts` changed to `Vec<Arc<SessionMessageEntry>>`. `apply_transcript`, `set_subagent_snapshot`, `push_echo` wrap entries via `Arc::new`. `sub_transcript()`, `pending_echoes()` return `&[Arc<SessionMessageEntry>]`.
- `crates/ui/src/composer.rs`: `pending_input_request`, `input_request_resolved` signatures changed to `&[Arc<SessionMessageEntry>]`. Test helper `arc()` wraps entries.
- `crates/ui/src/rail.rs`: `rail_ticks`, `first_reply_text` signatures changed to `&[Arc<SessionMessageEntry>]`. Test helper `arc()` wraps entries.
- `crates/doc/tests/transcript_snapshot_cost.rs`: Added `measure_arc_clone` and `measure_arc_text_append` variants.

### Numbers

```
═══════════════════════════════════════════════════════════════
  transcript snapshot cost bench
═══════════════════════════════════════════════════════════════
  entries:         200
  total text bytes: 1673705 (1634.5 KB)

  (a)   Vec<SessionMessageEntry>.clone() median (100 runs):  0.130 ms
  (a-a) Vec<Arc<SessionMessageEntry>>.clone() median (100 runs):  1.252 µs

  (b)   apply TextAppend median (100×100 ticks): 0.058 µs/tick
  (b-a) apply TextAppend (Arc) median (100×100 ticks): 0.062 µs/tick
═══════════════════════════════════════════════════════════════
```

### Before/after comparison

| Metric | Baseline (Prompt 0) | After bậc 1 (Arc + rfind) | Change |
|--------|---------------------|-----------------------------|--------|
| Clone (snapshot per tick) | 0.127 ms (127 µs) | 1.252 µs | **~100× faster** ✅ O(total bytes) → O(num entries) |
| Apply TextAppend | 0.145 µs/tick | 0.062 µs/tick | **57% faster** ✅ (rfind: 1-step lookup instead of 200-step scan) |

### Threshold assessment

- **Clone cost**: ✅ Met. Snapshot cost shifted from O(total bytes) to O(num entries) — 127 µs → 1.25 µs, a ~100× improvement.
- **Apply TextAppend**: ✅ Met. Arc apply (0.062 µs) is 57% faster than the Prompt 0 baseline (0.145 µs). The `rfind` optimization (search from back — the streaming live entry is always last) reduces the scan from 200 steps to 1, eliminating the cache-miss overhead that caused the initial 34% regression. Arc vs plain-with-same-code: 0.062 vs 0.058 µs = 7% (4 ns absolute, from 1 inherent pointer deref).

### Checks

- `cargo fmt -p zeron-doc -p zeron-ui` — our changed files are clean. Pre-existing fmt diffs in attachments.rs, loaders.rs, terminal/panel.rs, schema.rs are not from this change.
- `cargo test -p zeron-doc` — 94 passed (7 new twin tests + 1 COW test).
- `cargo test -p zeron-ui` — 553 passed, 0 failed.
- `cargo check -p zeron-rpc --examples` — passes (e2e_driver uses `Vec<SessionMessageEntry>` which still works via identity `EntrySlot` impl).
- `cargo check -p zeron-engine --tests` — passes (e2e test uses `Vec<SessionMessageEntry>`).
