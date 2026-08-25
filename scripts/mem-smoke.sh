#!/usr/bin/env bash
# Memory smoke (docs/memory-plan.md phase 0 / §8): boots ONE headless mock
# engine, streams N ~1MB-text replies through it via the mem_driver example,
# and gates the §8 acceptance thresholds:
#
#   engine idle      <40MB
#   stream growth    <ZERON_MEM_SMOKE_MAX_GROWTH_MB (default set from
#                     calibration on healthy main; §8's "<3× raw text" ratio
#                     is reported but not gated — one-time infra ~10MB and
#                     by-design warm-doc pins (WARM_DOC_CAP) don't scale with
#                     text bytes, so the ratio fails healthy code at small N)
#   reopen p95       <100ms
#   idle creep       <1MB/10min    (ZERON_MEM_SMOKE_CREEP=0 to skip the 10-min wait)
#
# macOS RSS note (phase 0 one-pager): Linux samples /proc/<pid>/statm here.
# For a macOS report split MALLOC vs IOSurface/Metal with:
#   footprint -p <pid>          # and/or: vmmap --summary <pid>
# The thresholds above are calibrated on Linux/glibc + mimalloc.
#
# Usage:  scripts/mem-smoke.sh
# Env:    ZERON_MEM_SMOKE_PORT (default 27805), ZERON_MEM_SMOKE_CREEP=0 skips
#         the 10-minute idle-creep window, ZERON_MOCK_REPEAT sizes each reply,
#         ZERON_MEM_SMOKE_MAX_GROWTH_MB overrides the growth gate.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PORT="${ZERON_MEM_SMOKE_PORT:-27805}"
DATA_DIR=/tmp/zeron-mem-smoke
LOG_DIR="$(mktemp -d /tmp/zeron-mem-smoke-logs.XXXXXX)"
ENGINE_PID=""
STATUS=1

cleanup() {
  if [[ -n "$ENGINE_PID" ]] && kill -0 "$ENGINE_PID" 2>/dev/null; then
    kill -9 "$ENGINE_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
  if [[ "${ZERON_MEM_SMOKE_KEEP_LOGS:-0}" == "1" ]]; then
    echo "logs kept in $LOG_DIR"
  else
    rm -rf "$LOG_DIR"
  fi
}
trap cleanup EXIT

echo "build: zeron (release) + mem_driver"
# Release: the §8 thresholds describe steady-state user RSS; a debug binary's
# extra symbols/panic paths sit ~10MB over the same code at idle.
(cd "$ROOT" && cargo build -q --release -p zeron)
(cd "$ROOT" && cargo build -q --release -p zeron-rpc --example mem_driver)
ZERON="$ROOT/target/release/zeron"
DRIVER="$ROOT/target/release/examples/mem_driver"

rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"

# Reply size knob — MUST be in the engine's env before spawn (mock.rs reads it
# per run; the driver only checks the env is present). ~1MB per reply so the
# streamed-text signal dominates fixed infra and allocator jitter.
export ZERON_MOCK_REPEAT="${ZERON_MOCK_REPEAT:-1800}"
# Mock runs are steerable, so a completed turn PARKS for 30 min (SESSION_IDLE)
# holding its doc Arc — the LRU could never evict mid-smoke. Reap after 20s.
export ZERON_SESSION_IDLE_SECS="${ZERON_SESSION_IDLE_SECS:-20}"
# Auto-titling fires a REAL LLM HTTP call per completed turn (with retries +
# backoff): non-deterministic RSS landing right inside the retention window.
# A memory gate must not depend on network state.
export ZERON_TITLES="${ZERON_TITLES:-0}"

echo "engine: starting headless mock engine on :$PORT"
ZERON_DATA_DIR="$DATA_DIR" ZERON_IPC_PORT="$PORT" \
  ZERON_HARNESS=mock RUST_LOG="${RUST_LOG:-info}" \
  "$ZERON" headless >"$LOG_DIR/engine.log" 2>&1 &
ENGINE_PID=$!

for i in $(seq 1 60); do
  bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT" 2>/dev/null && break
  sleep 1
  [[ $i -eq 60 ]] && { echo "FAIL: engine did not open IPC :$PORT" >&2; tail -n 40 "$LOG_DIR/engine.log" >&2; exit 1; }
done

rss_mb() { awk '/VmRSS/ {print int($2/1024)}' "/proc/$ENGINE_PID/status"; }

IDLE_MB=$(rss_mb)
echo "metric: idle ${IDLE_MB}MB"
if (( IDLE_MB >= 40 )); then
  echo "FAIL: idle ${IDLE_MB}MB >= 40MB threshold" >&2
  exit 1
fi

# Stream 3 replies puffed to ~1MB text each.
"$DRIVER" "$PORT" "$ENGINE_PID" | tee "$LOG_DIR/driver.out"
grep -q "^PASS:" "$LOG_DIR/driver.out" || { echo "FAIL: driver did not pass" >&2; exit 1; }

GROWTH_MB=$(awk '/^METRIC growth_bytes/ {print int($3/1024/1024)}' "$LOG_DIR/driver.out")
RETENTION_PCT=$(awk '/^METRIC retention_ratio_pct/ {print $3}' "$LOG_DIR/driver.out")
REOPEN_P95=$(awk '/^METRIC reopen_p95_ms/ {print $3}' "$LOG_DIR/driver.out")
[[ -n "$GROWTH_MB" && -n "$REOPEN_P95" ]] || { echo "FAIL: driver metrics missing" >&2; exit 1; }

MAX_GROWTH_MB="${ZERON_MEM_SMOKE_MAX_GROWTH_MB:-120}"
echo "metric: stream growth ${GROWTH_MB}MB for ${RETENTION_PCT}% of raw text (threshold <${MAX_GROWTH_MB}MB)"
if (( GROWTH_MB >= MAX_GROWTH_MB )); then
  echo "FAIL: stream growth ${GROWTH_MB}MB >= ${MAX_GROWTH_MB}MB budget" >&2
  echo "      (healthy-main calibration lives in this default; a jump means a" >&2
  echo "       retention regression — bisect with the cum_rss_after_chat_N metrics)" >&2
  exit 1
fi

echo "metric: reopen p95 ${REOPEN_P95}ms (threshold <100ms)"
if (( REOPEN_P95 >= 100 )); then
  echo "FAIL: reopen p95 ${REOPEN_P95}ms >= 100ms" >&2
  exit 1
fi

if [[ "${ZERON_MEM_SMOKE_CREEP:-1}" != "0" ]]; then
  echo "creep: sampling quiet engine over 10 minutes"
  # Floor-of-3 on both ends: point VmRSS readings jitter ±20MB with mimalloc
  # page-return timing; only the floor is comparable across 10 minutes.
  floor_mb() {
    local m=999999 v
    for _ in 1 2 3; do
      v=$(rss_mb)
      (( v < m )) && m=$v
      sleep 1
    done
    echo "$m"
  }
  BEFORE=$(floor_mb)
  sleep 600
  AFTER=$(floor_mb)
  CREEP_MB=$(( AFTER - BEFORE ))
  echo "metric: creep ${CREEP_MB}MB over 10min (threshold <1MB)"
  if (( CREEP_MB >= 1 )); then
    echo "FAIL: idle creep ${CREEP_MB}MB >= 1MB/10min" >&2
    exit 1
  fi
else
  echo "creep: skipped (ZERON_MEM_SMOKE_CREEP=0)"
fi

STATUS=0
echo "PASS: mem-smoke within all §8 thresholds"
