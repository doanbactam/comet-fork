//! Headless bench: transcript snapshot + apply cost.
//!
//! Run with: `cargo test --release -p zeron-doc transcript_snapshot_cost -- --nocapture`
//!
//! Measures the two costs the UI pays every notify tick:
//! (a) `transcript.clone()` — the deep clone `transcript.rs::sync` does today.
//! (b) `apply_transcript_frame` with 100 consecutive `TextAppend` ops — the
//!     streaming hot path.
//!
//! No new dependencies; uses `std::time::Instant`.

use std::sync::Arc;
use std::time::Instant;

use zeron_doc::parts::MessagePart;
use zeron_doc::schema::{MessageRole, SessionMessageEntry};
use zeron_doc::transcript_delta::{TextAppend, TranscriptFrame, apply_transcript_frame};

// ── Synthetic transcript generator ──────────────────────────────────────────

/// Build a transcript of 200 entries totaling ~1.6 MB of text, with one
/// "live" entry ~800 KB. Includes Tool parts carrying `output: Option<String>`
/// so the clone reflects real per-part allocation.
fn synthetic_transcript() -> Vec<SessionMessageEntry> {
    let mut entries = Vec::with_capacity(200);

    // 199 settled entries, ~5 KB each → ~1 MB.
    for i in 0..199 {
        let text = format!(
            "Entry {i}: {LOREM}\n\
             — generated for snapshot-cost bench, entry number {i:03}.\n\
             {LOREM}"
        );
        let parts = if i % 7 == 0 {
            // Every 7th entry has a Tool part with an output summary, mirroring
            // real chats where tool calls pepper the transcript.
            vec![
                MessagePart::Text {
                    id: format!("t{i}"),
                    text: text.clone(),
                },
                MessagePart::Tool {
                    id: format!("tool{i}"),
                    call: zeron_proto::ToolCall::Exec {
                        command: format!("echo bench-{i}"),
                    },
                    is_error: false,
                    resolved: true,
                    output: Some(format!("bench output line {i}\nexit 0")),
                    diff: None,
                    output_ref: None,
                    output_bytes: Some(120),
                    diff_ref: None,
                    diff_stats: None,
                    subagent_ref: None,
                    subagent_status: None,
                    subagent_tail: None,
                },
            ]
        } else {
            vec![MessagePart::Text {
                id: format!("t{i}"),
                text,
            }]
        };

        entries.push(SessionMessageEntry {
            id: format!("msg-{i:03}"),
            role: if i % 2 == 0 {
                MessageRole::Assistant
            } else {
                MessageRole::User
            },
            parts,
            created_at: 1_700_000_000_000 + i as i64 * 1000,
            device_id: "dev-bench".into(),
            status: Some(zeron_doc::parts::MessageStatus::Complete),
            continuation_of: None,
        });
    }

    // 1 "live" entry ~800 KB — a long streaming reply.
    let live_text = "x".repeat(800_000);
    entries.push(SessionMessageEntry {
        id: "msg-live".into(),
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text {
            id: "t-live".into(),
            text: live_text,
        }],
        created_at: 1_700_000_200_000,
        device_id: "dev-bench".into(),
        status: Some(zeron_doc::parts::MessageStatus::Streaming),
        continuation_of: None,
    });

    entries
}

/// Total bytes of all text payloads in the transcript (heuristic — not
/// counting enum discriminants or string capacity overhead).
fn transcript_text_bytes(entries: &[SessionMessageEntry]) -> usize {
    entries
        .iter()
        .flat_map(|e| e.parts.iter())
        .map(|p| p.byte_len())
        .sum()
}

// ── Measurement helpers ─────────────────────────────────────────────────────

fn median_durations(samples: &[std::time::Duration]) -> std::time::Duration {
    let mut sorted: Vec<_> = samples.iter().collect();
    sorted.sort();
    *sorted[sorted.len() / 2]
}

/// Measure `transcript.clone()` (deep clone of the Vec) over 100 runs,
/// return median.
fn measure_clone(entries: &[SessionMessageEntry]) -> std::time::Duration {
    const RUNS: usize = 100;
    // Warm-up: one clone so the allocator caches pages.
    let _ = entries.to_vec();

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t0 = Instant::now();
        let snap = entries.to_vec();
        let elapsed = t0.elapsed();
        std::hint::black_box(&snap);
        samples.push(elapsed);
    }
    median_durations(&samples)
}

/// Measure 100 consecutive `TextAppend` frames applied in place.
/// Returns median per-apply duration.
fn measure_text_append(entries: &[SessionMessageEntry]) -> std::time::Duration {
    const RUNS: usize = 100;

    // Warm-up.
    {
        let mut warm = entries.to_vec();
        let warm_len = warm.len();
        let live_len = entries
            .last()
            .and_then(|e| e.parts.first().map(|p| p.byte_len()))
            .unwrap_or(0)
            + 4;
        let frame = TextAppend {
            entry: "msg-live".into(),
            part: "t-live".into(),
            text: "warm".into(),
            len: live_len,
        };
        let _ = apply_transcript_frame(
            &mut warm,
            TranscriptFrame::Delta {
                upsert: vec![],
                append: vec![frame],
                remove: vec![],
                count: warm_len,
            },
        );
    }

    let mut samples = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        // Reset to a fresh copy each run so appends accumulate identically.
        let mut current = entries.to_vec();

        // Apply 100 consecutive appends (simulating 100 streaming ticks).
        let mut tick_durations = Vec::with_capacity(100);
        let mut live_len = entries
            .last()
            .and_then(|e| e.parts.first().map(|p| p.byte_len()))
            .unwrap_or(0);

        for _ in 0..100 {
            live_len += 8; // 8 bytes per tick
            let current_len = current.len();
            let frame = TranscriptFrame::Delta {
                upsert: vec![],
                append: vec![TextAppend {
                    entry: "msg-live".into(),
                    part: "t-live".into(),
                    text: "12345678".into(),
                    len: live_len,
                }],
                remove: vec![],
                count: current_len,
            };
            let t0 = Instant::now();
            apply_transcript_frame(&mut current, frame).expect("apply must succeed");
            tick_durations.push(t0.elapsed());
        }
        std::hint::black_box(&current);
        // Use the median tick within this run, then collect across runs.
        samples.push(median_durations(&tick_durations));
        let _ = run; // suppress unused warning if RUNS == 0
    }

    median_durations(&samples)
}

// ── Arc variant ─────────────────────────────────────────────────────────────

/// Measure `Vec<Arc<SessionMessageEntry>>::clone()` (shallow — bumps refcounts)
/// over 100 runs, return median.
fn measure_arc_clone(entries: &[Arc<SessionMessageEntry>]) -> std::time::Duration {
    const RUNS: usize = 100;
    let _ = entries.to_vec();

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t0 = Instant::now();
        let snap = entries.to_vec();
        let elapsed = t0.elapsed();
        std::hint::black_box(&snap);
        samples.push(elapsed);
    }
    median_durations(&samples)
}

/// Measure 100 consecutive `TextAppend` frames applied to `Vec<Arc<..>>`.
/// Each run starts with refcount=1 (the realistic UI apply path: the state's
/// transcript is the sole holder, so `Arc::make_mut` is a no-op).
fn measure_arc_text_append(entries: &[Arc<SessionMessageEntry>]) -> std::time::Duration {
    const RUNS: usize = 100;

    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        // Fresh Arcs each run — refcount=1, so make_mut is free (the UI's
        // apply path: AppState owns the only refs).
        let mut current: Vec<Arc<SessionMessageEntry>> =
            entries.iter().map(|e| Arc::new((**e).clone())).collect();
        let mut tick_durations = Vec::with_capacity(100);
        let mut live_len = entries
            .last()
            .and_then(|e| e.parts.first().map(|p| p.byte_len()))
            .unwrap_or(0);

        for _ in 0..100 {
            live_len += 8;
            let current_len = current.len();
            let frame = TranscriptFrame::Delta {
                upsert: vec![],
                append: vec![TextAppend {
                    entry: "msg-live".into(),
                    part: "t-live".into(),
                    text: "12345678".into(),
                    len: live_len,
                }],
                remove: vec![],
                count: current_len,
            };
            let t0 = Instant::now();
            apply_transcript_frame(&mut current, frame).expect("apply must succeed");
            tick_durations.push(t0.elapsed());
        }
        std::hint::black_box(&current);
        samples.push(median_durations(&tick_durations));
    }

    median_durations(&samples)
}

// ── Test entry point ────────────────────────────────────────────────────────

#[test]
fn transcript_snapshot_cost() {
    let entries = synthetic_transcript();
    let total_bytes = transcript_text_bytes(&entries);
    let num_entries = entries.len();

    // Arc variant: wrap once, then measure shallow clone + apply.
    let arc_entries: Vec<Arc<SessionMessageEntry>> =
        entries.iter().cloned().map(Arc::new).collect();

    // (a) clone cost — deep (Vec<SessionMessageEntry>)
    let clone_median = measure_clone(&entries);

    // (a-arc) clone cost — shallow (Vec<Arc<SessionMessageEntry>>)
    let arc_clone_median = measure_arc_clone(&arc_entries);

    // (b) apply TextAppend cost — deep
    let append_median = measure_text_append(&entries);

    // (b-arc) apply TextAppend cost — Arc (copy-on-write)
    let arc_append_median = measure_arc_text_append(&arc_entries);

    // Report
    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!("  transcript snapshot cost bench");
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!("  entries:         {num_entries}");
    eprintln!(
        "  total text bytes: {total_bytes} ({:.1} KB)",
        total_bytes as f64 / 1024.0
    );
    eprintln!();
    eprintln!(
        "  (a)   Vec<SessionMessageEntry>.clone() median (100 runs):  {:.3} ms",
        clone_median.as_secs_f64() * 1000.0
    );
    eprintln!(
        "  (a-a) Vec<Arc<SessionMessageEntry>>.clone() median (100 runs):  {:.3} µs",
        arc_clone_median.as_secs_f64() * 1_000_000.0
    );
    eprintln!();
    eprintln!(
        "  (b)   apply TextAppend median (100×100 ticks): {:.3} µs/tick",
        append_median.as_secs_f64() * 1_000_000.0
    );
    eprintln!(
        "  (b-a) apply TextAppend (Arc) median (100×100 ticks): {:.3} µs/tick",
        arc_append_median.as_secs_f64() * 1_000_000.0
    );
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!();

    // Sanity: transcript must be non-trivial.
    assert!(num_entries == 200);
    assert!(
        total_bytes > 1_500_000,
        "transcript should be ~1.6 MB, got {total_bytes}"
    );
}

/// Lorem ipsum filler ~2 KB, repeated to fill ~4 KB entries.
const LOREM: &str = "\
Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris \
nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in \
reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla \
pariatur. Excepteur sint occaecat cupidatat non proident, sunt in \
culpa qui officia deserunt mollit anim id est laborum. \
Sed ut perspiciatis unde omnis iste natus error sit voluptatem \
accusantium doloremque laudantium, totam rem aperiam, eaque ipsa \
quae ab illo inventore veritatis et quasi architecto beatae vitae \
dicta sunt explicabo. Nemo enim ipsam voluptatem quia voluptas sit \
aspernatur aut odit aut fugit, sed quia consequuntur magni dolores \
eos qui ratione voluptatem sequi nesciunt. Neque porro quisquam est, \
qui dolorem ipsum quia dolor sit amet, consectetur, adipisci velit, \
sed quia non numquam eius modi tempora incidunt ut labore et dolore \
magnam aliquam quaerat voluptatem. Ut enim ad minima veniam, quis \
nostrum exercitationem ullam corporis suscipit laboriosam, nisi ut \
aliquid ex ea commodi consequatur. Quis autem vel eum iure \
reprehenderit qui in ea voluptate velit esse quam nihil molestiae \
consequatur, vel illum qui dolorem eum fugiat quo voluptas nulla \
pariatur. At vero eos et accusamus et iusto odio dignissimos ducimus \
qui blanditiis praesentium voluptatum deleniti atque corrupti quos \
dolores et quas molestias excepturi sint occaecati cupiditate non \
provident, similique sunt in culpa qui officia deserunt mollitia \
animi, id est laborum et dolorum fuga. Et harum quidem rerum facilis \
est et expedita distinctio. Nam libero tempore, cum soluta nobis est \
eligendi optio cumque nihil impedit quo minus id quod maxime placeat \
facere possimus, omnis voluptas assumenda est, omnis dolor \
repellendus. Temporibus autem quibusdam et aut officiis debitis aut \
rerum necessitatibus saepe eveniet ut et voluptates repudiandae \
sint et molestiae non recusandae. Itaque earum rerum hic tenetur \
a sapiente delectus, ut aut reiciendis voluptatibus maiores alias \
consequatur aut perferendis doloribus asperiores repellat.";
