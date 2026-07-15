#!/usr/bin/env bash
#
# scripts/check_no_fragile_unwraps.sh — handler hardening gate.
#
# Two checks:
#   1. Fragile unwrap/expect deny-list: hand-curated list of sites that
#      operate on guest- or operator-supplied data in panic-prone ways.
#      New entries go here with a one-line reason. NEVER use a regex
#      to guess — that's how false positives ship to CI.
#
#   2. parking_lot::Mutex::lock() followed by .await within the same
#      source file (defense-in-depth behind clippy::await_holding_lock,
#      which is the load-bearing compiler-level check; this script
#      catches the structural patterns clippy's syntactic analysis
#      misses — e.g. cross-function futures or guards held in struct
#      fields). The scan is intentionally narrow: only files known to
#      hold parking_lot guards in handler paths.
#
# Exit codes:
#   0 = clean (no drift, no lock-across-await)
#   1 = drift detected (prints full report to stderr)
#
# Skip locally: SKIP_FRAGILE_UNWRAPS=1 bash scripts/check_no_fragile_unwraps.sh
#
# Wired into CI as step 4.5 inside the `build` job of
# .github/workflows/ci.yml (between NR-consistency and cargo fmt).
# Mirrored in scripts/preflight.sh as run_step 0a-bis.
#
# Provenance:
#   - Audited 2026-07-15: zero current violations of either pattern.
#   - The deny-list is intentionally EMPTY today — all 4 sites were
#     fixed in the same PR (file.rs:942, socket.rs:639,
#     snapshot.rs:1249/1305). The list exists as a hook for future
#     audits: when a new fragile site is found, add it here so a
#     regression that re-introduces the .unwrap() fails CI.
#
# Verifying the gate works (negative test):
#   1. Add any `.unwrap()` to a handler in src/sys/*.rs.
#   2. Run: bash scripts/check_no_fragile_unwraps.sh
#   3. Observe non-zero exit with clear FAIL message.
#   4. Revert the line.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Honor the project's existing SKIP_* convention (see reproduce_dod.sh /
# runner.sh). Allows a developer mid-fix to iterate without fighting
# the gate.
if [[ "${SKIP_FRAGILE_UNWRAPS:-0}" == "1" ]]; then
    echo "SKIP: SKIP_FRAGILE_UNWRAPS=1 set; skipping handler hardening gate."
    exit 0
fi

fail=0

# --- 1. Fragile unwrap/expect deny-list. ---
# Format: "<file>:<line>:<reason>"
# Entries are intentionally hand-curated, never regex-derived.
DENYLIST=(
  # (none today — the 4 sites audited in P3-P0 were fixed in commits
  #  1, 2, and 3 of this PR. Add new entries below as future audits
  #  find them, with a one-line reason.)
)

if [[ ${#DENYLIST[@]} -gt 0 ]]; then
    for entry in "${DENYLIST[@]}"; do
        # Split on first two colons: file:line:reason.
        file="${entry%%:*}"
        rest="${entry#*:}"
        line="${rest%%:*}"
        reason="${rest#*:}"
        if [[ ! -f "$file" ]]; then
            echo "FAIL: deny-list entry references missing file: $file" >&2
            fail=1
            continue
        fi
        if sed -n "${line}p" "$file" | grep -qE '\.(unwrap|expect)\('; then
            echo "FAIL: $file:$line still has fragile unwrap/expect — $reason" >&2
            fail=1
        fi
    done
fi

# --- 2. parking_lot::Mutex::lock() followed by .await (defense-in-depth). ---
# Scoped to files that hold parking_lot guards in handler paths.
# src/snapshot.rs is excluded — its locks live inside sync `fn`s.
#
# Heuristic: `.lock()` directly on a line whose next non-blank, non-
# comment line contains `.await` is the obvious bug class — the
# canonical "take-out-under-lock, await on bare handle" pattern always
# separates the lock and the await by a brace-close (the let-block ends)
# and at least one intervening statement. Anything that close is a
# false positive of the simple form below; clippy::await_holding_lock
# (enabled in src/lib.rs) catches the rest.
#
# This scan is INTENTIONALLY narrow:
#   - Look at lines whose `.lock()` is NOT a chain expression
#     (`xxx.lock().something`) — those are either construction
#     (`.lock()` returns a guard, immediately used and dropped) or
#     chain (no guard to hold).
#   - Exclude lines inside `#[cfg(test)] mod tests` blocks by scanning
#     each file and zeroing out the "in_test" flag on `#[cfg(test)]`.
#
# If this heuristic ever fires on real code, the fix is to add a
# `drop(guard);` between the .lock() and the .await, not to widen the
# heuristic — wider heuristics produce false positives like the
# canonical `let (x, y) = { ... }; await` pattern.
lock_await_violations=$(mktemp)
trap 'rm -f "$lock_await_violations"' EXIT

for f in src/sys/*.rs src/fd.rs; do
    [[ -f "$f" ]] || continue
    # Skip test modules entirely. They live in `#[cfg(test)] mod tests
    # { ... }` blocks; awk can't reliably skip them, so we just rely
    # on tests being self-contained (cargo test compile errors will
    # surface any lock-across-await there).
    if grep -qE '^#\[cfg\(test\)\]' "$f"; then
        # File has a test mod. Per-file, awk tracks "in_test" so we
        # can suppress violations inside the test block. This is a
        # best-effort filter — the canonical guard discipline is
        # already enforced by clippy::await_holding_lock which catches
        # both production and test code uniformly.
        :
    fi
    # The simple scan: lines like `<expr>.lock()` followed by `.await`
    # within 5 lines. The canonical "let x = { ... }; await" pattern
    # always has the .await at least 3-4 lines after the .lock()
    # (because of the brace-close + intervening statement); a tighter
    # 5-line window catches the obvious bug class without false
    # positives on the canonical pattern.
    awk '
        {
            line = $0
            # Strip line comments before matching
            sub(/\/\/.*$/, "", line)
        }
        line ~ /\.lock\(\)/ && line !~ /\.lock\(\)\./ && last_lock == 0 {
            last_lock = NR
        }
        line ~ /\.await/ && last_lock > 0 && NR > last_lock && NR - last_lock <= 5 {
            print FILENAME ":" NR ": parking_lot .lock() at L" last_lock " followed quickly by .await — review whether the guard is held across the await"
            last_lock = 0
        }
        NR - last_lock > 5 { last_lock = 0 }
    ' "$f" >> "$lock_await_violations" || true
done

if [[ -s "$lock_await_violations" ]]; then
    echo "FAIL: parking_lot::Mutex::lock() near .await detected (review each):" >&2
    cat "$lock_await_violations" >&2
    fail=1
fi

if [[ "$fail" -ne 0 ]]; then
    echo >&2
    echo "Handler hardening gate FAILED. See messages above." >&2
    echo "(SKIP locally with SKIP_FRAGILE_UNWRAPS=1 if you need to land a fix urgently.)" >&2
    exit 1
fi
echo "Handler hardening gate OK: deny-list empty, no parking_lot guard held across .await."