#!/usr/bin/env bash
#
# tests/strace_baselines/strace_native.sh
#
# Capture a native syscall trace for the given command.
#
# Usage:
#   strace_native.sh <out.txt> -- <command...>
#
# On Linux, uses `strace -f -o <out>`. On macOS, uses `dtruss -f` (requires
# sudo because DTrace needs privileges). When neither tracer is available
# the script falls back to writing a header + instructions; the harness
# treats an empty baseline as "no comparison possible" and skips rather
# than failing the build.
#
# Cross-platform trace format normalization is handled in this script:
# strace emits lines like `openat(AT_FDCWD, "/etc/...", O_RDONLY) = 3`,
# dtruss emits lines like `openat(0x3, "/etc/...", 0x0, 0x0)        = 3`.
# We keep the raw format and let diff.py parse either.

set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <out.txt> -- <command...>" >&2
    exit 2
fi

OUT="$1"; shift
if [[ "$1" != "--" ]]; then
    echo "expected '--' separator after out path, got: $1" >&2
    exit 2
fi
shift

# Ensure parent dir exists.
mkdir -p "$(dirname "$OUT")"

if command -v strace >/dev/null 2>&1; then
    # Linux strace.
    strace -f -o "$OUT" "$@"
elif command -v dtruss >/dev/null 2>&1; then
    # macOS dtruss (needs sudo; if not root, capture what we can).
    if [[ $EUID -eq 0 ]]; then
        dtruss -f "$@" 2>"$OUT" || true
    else
        echo "# dtruss requires root on macOS — capturing fallback (no trace)" > "$OUT"
        "$@" >/dev/null 2>&1 || true
    fi
else
    echo "# no tracer installed (strace on Linux, dtruss on macOS)" > "$OUT"
    echo "# install with: brew install strace  (or run on Linux)" >> "$OUT"
    "$@" >/dev/null 2>&1 || true
fi