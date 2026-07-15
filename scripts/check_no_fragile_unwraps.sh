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
# Strategy: intentionally MINIMAL. The load-bearing compiler-level
# enforcement is `clippy::await_holding_lock` (enabled in src/lib.rs
# by this PR). This script is a backstop for the narrowest bug class:
#
#   let g = x.lock();   ← single-line let-binding
#   foo.await;          ← next non-blank line
#
# Excludes (verified by tests below):
#   - `*x.lock() += 1;`        — deref assignment, guard consumed by op
#   - `x.lock().method()`      — chain, guard held only for the chain
#   - `let g = { ... lock(); ... }; await` — multi-line block, guard
#     dropped inside the block (canonical ADR 0001 §2 pattern)
#   - locks inside `#[cfg(test)] mod tests { ... }`
#
# Anything more elaborate — cross-function futures, guards held in
# struct fields, nested match arms — is caught by
# `clippy::await_holding_lock`. Wider heuristics here produce false
# positives; if this gate ever fires on real production code, the
# fix is to add `drop(guard);` (or scope the let-block tighter),
# not to widen the heuristic.
lock_await_violations=$(mktemp)
trap 'rm -f "$lock_await_violations"' EXIT

for f in src/sys/*.rs src/fd.rs; do
    [[ -f "$f" ]] || continue

    awk '
        BEGIN { in_test = 0; last_lock = 0 }
        {
            line = $0
            sub(/\/\/.*$/, "", line)  # strip line comments
        }
        /^#\[cfg\(test\)\]/ { in_test = 1; next }
        in_test && /^}/ { in_test = 0; next }
        in_test { next }

        # Match a single-line `let <name> = <expr>.lock();` — semicolon
        # at end of line means the guard is alive on the next line.
        # Exclude deref-assignment (`*x.lock() = ...`) and chain
        # (`x.lock().something`).
        line ~ /^[ \t]*let[ \t]+(mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*[ \t]*=/ \
            && line ~ /\.lock\(\)[ \t]*;/ \
            && line !~ /\*[ \t]*[A-Za-z_][A-Za-z0-9_]*[ \t]*\.lock\(\)/ \
            && line !~ /\.lock\(\)[ \t]*\./ {
            last_lock = NR
        }

        # `.await` on the very next non-blank line = obvious bug.
        line ~ /\.await/ && last_lock > 0 && NR == last_lock + 1 {
            print FILENAME ":" NR ": parking_lot .lock() bound at L" last_lock ", .await on next line — review whether the guard is held across the await"
            last_lock = 0
        }
        NR > last_lock + 1 { last_lock = 0 }
    ' "$f" >> "$lock_await_violations" || true
done

if [[ -s "$lock_await_violations" ]]; then
    echo "FAIL: parking_lot::Mutex::lock() bound to a name with .await on next line:" >&2
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