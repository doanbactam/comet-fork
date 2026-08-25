//! Memory smoke driver (`scripts/mem-smoke.sh` runs it) — the measurement half
//! of `docs/memory-plan.md` phase 0. Against ONE headless engine's IPC:
//!
//! 1. boot baseline RSS (`LocalDevice` barrier);
//! 2. N-chat stream: create + run N mock chats, `ZERON_MOCK_REPEAT` puffs each
//!    reply to ~1MB text; sample RSS after the last settles;
//! 3. reopen latency: drop every doc watch, wait out the LRU idle window,
//!    re-watch each chat and time reset→first frame (p95 asserted in the shell);
//! 4. retention: resident-after-stream ÷ raw text bytes (<3× asserted);
//! 5. idle creep: two RSS samples 10min apart on the quiet engine.
//!
//! Prints one `METRIC <key> <value>` line per number; exit nonzero on failure.

use std::time::{Duration, Instant};

use zeron_rpc::{RpcClient, connect_ws, methods};

/// LRU eviction needs 30s unwatched + unpinned (`EVICT_MIN_IDLE_MS`,
/// engine/src/doc_host.rs) — the reopen step waits this out after dropping
/// every watch so the docs actually leave the warm set.
const EVICT_IDLE_MS: u64 = 31_000;
/// Enough chats that streamed-text cost dominates one-time infra (~10MB:
/// runtime caches, snapshot writer, first-doc setup) and run-to-run allocator
/// jitter — the retention gate divides growth by this workload's text.
const CHATS: usize = 6;
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

fn rss_bytes(pid: u32) -> u64 {
    // statm fields: size resident shared … — field 1 is resident.
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .unwrap_or_else(|err| fail(&format!("read /proc/{pid}/statm: {err}")));
    let pages = statm
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| fail("statm: empty"));
    pages.parse::<u64>().unwrap_or(0) * 4096
}

/// Minimum of `n` samples 1s apart: mimalloc returns pages in bursts, so a
/// single VmRSS reading swings ±20MB on a quiet engine. The floor is the
/// stable quantity (and the honest one — retained memory, not arena timing).
fn rss_floor_bytes(pid: u32, n: u64) -> u64 {
    let mut min = u64::MAX;
    for _ in 0..n {
        min = min.min(rss_bytes(pid));
        std::thread::sleep(Duration::from_secs(1));
    }
    min
}

fn metric(key: &str, value: impl std::fmt::Display) {
    println!("METRIC {key} {value}");
}

/// Subscribe to a watch-stream and poll until `predicate` returns `Some`
/// (resubscribing across lifecycle boundaries — same contract as e2e_driver).
async fn wait_stream<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
    mut predicate: impl FnMut(&serde_json::Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + STEP_TIMEOUT;
    'resubscribe: loop {
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            fail(&format!("{what}: timed out after {}s", STEP_TIMEOUT.as_secs()));
        }
        let mut rx = match client.subscribe(method, params.clone()).await {
            Ok(rx) => rx,
            Err(err) => fail(&format!("{what}: subscribe {method} failed: {err}")),
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                fail(&format!("{what}: timed out after {}s", STEP_TIMEOUT.as_secs()));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(item)) => {
                    if let Some(found) = predicate(&item) {
                        return found;
                    }
                }
                Ok(None) => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue 'resubscribe;
                }
                Err(_) => fail(&format!("{what}: timed out after {}s", STEP_TIMEOUT.as_secs())),
            }
        }
    }
}

fn apply_frame(
    transcript: &mut Vec<zeron_doc::SessionMessageEntry>,
    item: &serde_json::Value,
) {
    let frame: zeron_doc::TranscriptFrame = serde_json::from_value(item.clone())
        .unwrap_or_else(|err| fail(&format!("parse transcript frame: {err}")));
    zeron_doc::apply_transcript_frame(transcript, frame)
        .unwrap_or_else(|err| fail(&format!("apply transcript frame: {err}")));
}

/// Wait for the assistant entry of `message_id` to reach `complete` while
/// accumulating frames into `transcript`.
async fn await_completion(
    client: &RpcClient,
    chat_id: &str,
    transcript: &mut Vec<zeron_doc::SessionMessageEntry>,
) {
    wait_stream(client, methods::WATCH_DOC_MESSAGES, serde_json::json!({ "chatId": chat_id }), "assistant completion", |item| {
        apply_frame(transcript, item);
        transcript
            .iter()
            .any(|e| e.role == zeron_doc::MessageRole::Assistant && e.status == Some(zeron_doc::MessageStatus::Complete))
            .then_some(())
    })
    .await;
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let ipc_port: u16 = args
        .next()
        .unwrap_or_else(|| "27801".into())
        .parse()
        .expect("ipc port");
    let pid: u32 = args
        .next()
        .unwrap_or_else(|| fail("usage: mem_driver <ipc_port> <engine_pid>"))
        .parse()
        .expect("engine pid");
    // ZERON_MOCK_REPEAT is read by the ENGINE (mock.rs puffs each reply with
    // it); the driver only needs the env present, not the value.

    let client = connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .unwrap_or_else(|err| fail(&format!("connect ipc :{ipc_port}: {err}")));

    // ── 1. Boot baseline: engine answering RPC = assembled and serving ────────
    client
        .call(methods::ENGINE_READY, serde_json::json!({}))
        .await
        .unwrap_or_else(|err| fail(&format!("EngineReady: {err}")));
    let baseline = rss_bytes(pid);
    metric("baseline_bytes", baseline);

    // ── 2. Stream N chats through the mock harness ─────────────────────────────
    let mut chat_bytes = Vec::new();
    let space_id = uuid::Uuid::new_v4().to_string();
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace",
                "spaceId": space_id,
                "deviceId": local_device(&client).await,
                "path": "/tmp",
            }),
        )
        .await
        .unwrap_or_else(|err| fail(&format!("createSpace: {err}")));

    for i in 0..CHATS {
        let chat_id = uuid::Uuid::new_v4().to_string();
        client
            .call(
                methods::MUTATE,
                serde_json::json!({
                    "op": "createChat",
                    "chatId": chat_id,
                    "spaceId": space_id,
                    "config": {
                        "harness": "mock",
                        "model": null,
                        "reasoning": null,
                        "sandbox": "workspace-write",
                    },
                }),
            )
            .await
            .unwrap_or_else(|err| fail(&format!("createChat #{i}: {err}")));
        let message_id = uuid::Uuid::new_v4().to_string();
        client
            .call(
                methods::QUEUE_COMMAND,
                serde_json::json!({
                    "chatId": chat_id,
                    "command": {
                        "kind": "run",
                        "messageId": message_id,
                        "request": {
                            "prompt": format!("mem-smoke run {i}"),
                            "model": null,
                            "reasoning": null,
                            "cwd": "/tmp",
                            "sandbox": "workspace-write",
                            "autoApprove": true,
                            "resume": null,
                        },
                    },
                }),
            )
            .await
            .unwrap_or_else(|err| fail(&format!("QueueCommand #{i}: {err}")));

        // Watch from BEFORE the run lands so nothing is missed; hold a receiver
        // per chat only until its turn completes, then drop it (unwatch →
        // evictable). Frames accumulate in `transcript` for the byte count.
        let mut transcript = Vec::new();
        await_completion(&client, &chat_id, &mut transcript).await;
        let raw: usize = transcript
            .iter()
            .flat_map(|e| e.parts.iter())
            .map(|p| match p {
                zeron_doc::MessagePart::Text { text, .. }
                | zeron_doc::MessagePart::Reasoning { text, .. } => text.len(),
                _ => 0,
            })
            .sum();
        println!("CHAT {i} raw_text_bytes={raw}");
        // Per-chat RSS checkpoint: splits growth into first-chat infrastructure
        // (runtime caches, buffers) vs marginal per-chat retention.
        metric(&format!("cum_rss_after_chat_{i}"), rss_bytes(pid));
        chat_bytes.push(raw);
    }

    let total_raw: usize = chat_bytes.iter().sum();
    if total_raw == 0 {
        fail("no transcript text streamed — mock script empty?");
    }

    // Give the last snapshot debounce / eviction tick a beat, then sample the
    // RSS floor (allocator page-return jitter makes point samples unstable).
    tokio::time::sleep(Duration::from_secs(5)).await;
    let after_stream = rss_floor_bytes(pid, 5);
    metric("after_stream_bytes", after_stream);
    metric("raw_text_bytes", total_raw);
    // Informational: the §8 "<3×" ratio reads cleanly only when streaming cost
    // dominates; the GATE below uses an absolute growth budget instead — see
    // scripts/mem-smoke.sh (one-time infra + by-design warm pins don't scale
    // with text bytes, so a small-denominator ratio fails healthy code).
    metric(
        "growth_bytes",
        after_stream.saturating_sub(baseline),
    );
    metric("retention_ratio_pct", after_stream.saturating_sub(baseline) * 100 / total_raw as u64);
    metric("per_chat_avg_bytes", total_raw / CHATS);

    // ── 3. Reopen latency: unwatch everything, age out the LRU, re-watch ──────
    // Receivers were dropped per-chat already; wait past EVICT_MIN_IDLE_MS.
    tokio::time::sleep(Duration::from_millis(EVICT_IDLE_MS)).await;
    // Sampled after the idle window: parked sessions reaped (ZERON_SESSION_IDLE_
    // SECS) and unwatched docs LRU-evicted. If RSS doesn't fall back here, the
    // growth is RETAINED, not just warm-pinned or churn watermark.
    metric("post_evict_bytes", rss_bytes(pid));
    let mut latencies_ms = Vec::new();
    for chat_row in list_chat_rows(&client).await {
        let start = Instant::now();
        let mut transcript = Vec::new();
        wait_stream(
            &client,
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_row }),
            "reopen first frame",
            |item| {
                apply_frame(&mut transcript, item);
                (!transcript.is_empty()).then_some(())
            },
        )
        .await;
        latencies_ms.push(start.elapsed().as_millis() as u64);
    }
    if latencies_ms.len() < CHATS as usize {
        fail("reopen pass saw fewer chats than created");
    }
    latencies_ms.sort_unstable();
    let p95 = latencies_ms[(latencies_ms.len() as f64 * 0.95).ceil() as usize - 1];
    metric("reopen_p95_ms", p95);
    metric("reopened_docs", latencies_ms.len());

    // ── 4. Idle creep is sampled by the SHELL around a quiet window ────────────
    metric("post_reopen_bytes", rss_bytes(pid));
    println!("PASS: mem smoke measurements complete");
}

async fn local_device(client: &RpcClient) -> String {
    client
        .call(methods::LOCAL_DEVICE, serde_json::json!({}))
        .await
        .ok()
        .and_then(|v| v.get("deviceId").and_then(|d| d.as_str().map(str::to_string)))
        .unwrap_or_else(|| fail("LocalDevice: no deviceId"))
}

async fn list_chat_rows(client: &RpcClient) -> Vec<String> {
    let mut rows = Vec::new();
    wait_stream(client, methods::WATCH_CHATS, serde_json::json!({}), "chat rows", |item| {
        if let Some(list) = item.as_array() {
            for chat in list {
                if let Some(id) = chat.get("id").and_then(|v| v.as_str()) {
                    rows.push(id.to_string());
                }
            }
            return Some(());
        }
        None
    })
    .await;
    rows
}
