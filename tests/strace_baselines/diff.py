#!/usr/bin/env python3
"""
diff.py — compare a host trace-host JSON dump to a native syscall baseline.

Usage:
    diff.py <baseline.txt> <trace_host.json>
        # baseline.txt: one syscall name per line (lines starting with `#`
        # are comments and ignored). May also be a raw strace output file
        # produced by strace_native.sh — the script auto-detects.
        # trace_host.json: one JSON object per line, as emitted by
        # bin/trace-host. Only the `name` field is read.

Exit codes:
    0  baseline is a subset of the host trace (no regressions)
    1  baseline contains a syscall the host did NOT issue (regression)
    2  baseline or trace file missing / malformed
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


# Regex to extract syscall NAME from strace / dtruss output lines like:
#   strace:  openat(AT_FDCWD, "/etc/...", O_RDONLY) = 3
#   dtruss:  openat(0x3, "/etc/...", 0x0, 0x0)        = 3
# We deliberately match the leading identifier before the open paren.
SYSCALL_RE = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(")


def load_baseline(path: Path) -> set[str]:
    """Load the baseline syscall set from either a one-name-per-line file
    (with `#`-prefixed comments) or a raw strace/dtruss file (auto-detected
    by the absence of JSON braces and presence of paren-style entries)."""
    text = path.read_text()
    names: set[str] = set()
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        # One-name-per-line form.
        if "(" not in stripped:
            names.add(stripped)
            continue
        # strace / dtruss raw form.
        m = SYSCALL_RE.match(line)
        if m:
            names.add(m.group(1))
    return names


def load_host_trace(path: Path) -> set[str]:
    """Load the syscall NAMES from a trace-host JSON-lines file."""
    names: set[str] = set()
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                # Not JSON — skip (allows the file to contain human
                # comments in addition to JSON lines).
                continue
            name = entry.get("name")
            if isinstance(name, str) and name:
                names.add(name)
    return names


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: diff.py <baseline.txt> <trace_host.json>", file=sys.stderr)
        return 2
    baseline_path = Path(sys.argv[1])
    trace_path = Path(sys.argv[2])
    if not baseline_path.is_file():
        print(f"missing baseline: {baseline_path}", file=sys.stderr)
        return 2
    if not trace_path.is_file():
        print(f"missing trace: {trace_path}", file=sys.stderr)
        return 2

    baseline = load_baseline(baseline_path)
    host = load_host_trace(trace_path)

    missing = sorted(baseline - host)
    extra = sorted(host - baseline)

    print(f"baseline: {len(baseline)} syscalls")
    print(f"host    : {len(host)} syscalls")
    if extra:
        print(f"info    : {len(extra)} host-only syscalls (not in baseline): {', '.join(extra)}")
    if missing:
        print(f"FAIL    : {len(missing)} baseline syscalls MISSING from host:", file=sys.stderr)
        for m in missing:
            print(f"  - {m}", file=sys.stderr)
        return 1
    print("OK      : baseline ⊆ host")
    return 0


if __name__ == "__main__":
    sys.exit(main())