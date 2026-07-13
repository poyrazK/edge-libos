#!/usr/bin/env bash
#
# scripts/check_nr_consistency.sh — verify the NR table is mirrored across
# three sources of truth:
#
#   A. src/sys/*.rs           — pub const NR_FOO: u32 = N;
#   B. src/dispatch.rs        — sys::<group>::NR_* references in match arms
#   C. tests/conformance/syscall.h — #define NR_FOO N
#
# Catches:
#   - Syscalls declared in Rust but not wired into dispatch::dispatch.
#   - Dispatched NRs without a C #define (guest side).
#   - Numeric drift (Rust pub const = 42, C #define = 41) — silent ABI
#     break, would surface as a guest trap on the wrong syscall.
#
# Exit codes:
#   0 = clean (all three mirrors consistent)
#   1 = drift detected (prints full report to stderr)
#
# Run locally: bash scripts/check_nr_consistency.sh
# Wired into CI as a step inside the `build` job in
# .github/workflows/ci.yml.

set -euo pipefail

# Force byte-wise sort so comm(1) compares the same string order across
# the three sources. Without this, the host's UTF-8 locale can sort
# e.g. `NR_EXIT=60` AFTER `NR_EXIT_GROUP=231`, producing phantom diffs.
export LC_ALL=C

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# ── A. Extract NR_ constants from Rust source files. ─────────────────────
#   pub const NR_FOO: u32 = 42;
# Parse each source twice — once for "name=value" (drift check), once
# for "name" only (set-membership). Sorting on the name-only form
# keeps the byte order consistent across all three sources (otherwise
# `NR_ACCEPT=43` sorts AFTER `NR_ACCEPT4=288` because '=' (0x3D) > '4').
RUST_DEFS=$(grep -h "^pub const NR_" src/sys/*.rs \
    | sed -nE 's/^pub const (NR_[A-Z0-9_]+): u32 = ([0-9]+);.*/\1=\2/p' \
    | sort -u)
RUST_NAMES=$(grep -h "^pub const NR_" src/sys/*.rs \
    | sed -nE 's/^pub const (NR_[A-Z0-9_]+): u32 = ([0-9]+);.*/\1/p' \
    | sort -u)

# ── B. Extract NR_ references inside dispatch::dispatch + syscall_name. ──
DISPATCH_NAMES=$(grep -hE "sys::[a-z]+::NR_[A-Z0-9_]+" src/dispatch.rs \
    | grep -oE "NR_[A-Z0-9_]+" \
    | sort -u)

# ── C. Extract NR_ #define values from the C header. ─────────────────────
#   #define NR_FOO 42
C_DEFS=$(grep -hE "^#define NR_[A-Z0-9_]+\s+[0-9]+" \
        tests/conformance/syscall.h \
    | sed -nE 's/^#define (NR_[A-Z0-9_]+)[ \t]+([0-9]+).*/\1=\2/p' \
    | sort -u)
C_NAMES=$(grep -hE "^#define NR_[A-Z0-9_]+\s+[0-9]+" \
        tests/conformance/syscall.h \
    | sed -nE 's/^#define (NR_[A-Z0-9_]+)[ \t]+([0-9]+).*/\1/p' \
    | sort -u)

# ── Audit passes. ────────────────────────────────────────────────────────
fail=0

# 1. Rust const defined but NOT in dispatch::dispatch match arms.
unused_in_dispatch=$(comm -23 <(printf '%s\n' "$RUST_NAMES") \
                                  <(printf '%s\n' "$DISPATCH_NAMES"))
if [[ -n "$unused_in_dispatch" ]]; then
    echo "FAIL: NR_* defined in src/sys but missing from src/dispatch.rs:" >&2
    echo "$unused_in_dispatch" | sed 's/^/  /' >&2
    fail=1
fi

# 2. dispatch::dispatch references an NR_ not defined in src/sys.
# (should be impossible by construction — `pub` constants exported and used.)
phantom_in_dispatch=$(comm -13 <(printf '%s\n' "$RUST_NAMES") \
                                  <(printf '%s\n' "$DISPATCH_NAMES"))
if [[ -n "$phantom_in_dispatch" ]]; then
    echo "FAIL: src/dispatch.rs references NR_* not defined in src/sys:" >&2
    echo "$phantom_in_dispatch" | sed 's/^/  /' >&2
    fail=1
fi

# 3. C #define NR_ exists in dispatch but not in syscall.h.
c_missing=$(comm -23 <(printf '%s\n' "$DISPATCH_NAMES") \
                       <(printf '%s\n' "$C_NAMES"))
if [[ -n "$c_missing" ]]; then
    echo "FAIL: dispatch::dispatch has NR_* without a C #define in syscall.h:" >&2
    echo "$c_missing" | sed 's/^/  /' >&2
    echo "  → Either add a C conformance test (preferred) or add the" >&2
    echo "    #define NR_* line to tests/conformance/syscall.h so the" >&2
    echo "    guest-side compiler can refer to it." >&2
    fail=1
fi

# 4. Numeric drift: same name, different number between Rust and C.
#    This is the SILENT killer — guest would call syscall 42 thinking
#    it's NR_OPEN, host would dispatch as NR_STAT. We diff by name.
while IFS='=' read -r name num; do
    [[ -z "$name" ]] && continue
    c_num=$(printf '%s\n' "$C_DEFS" | awk -F= -v n="$name" '$1==n{print $2}')
    if [[ -n "$c_num" && "$c_num" != "$num" ]]; then
        echo "FAIL: NR_* numeric drift — $name: Rust=$num, C=$c_num" >&2
        fail=1
    fi
done < <(printf '%s\n' "$RUST_DEFS")

if [[ "$fail" -ne 0 ]]; then
    echo >&2
    echo "NR-consistency check FAILED. See messages above." >&2
    exit 1
fi

echo "NR-consistency OK: $(printf '%s\n' "$RUST_NAMES" | wc -l | tr -d ' ') NRs, three-way mirror clean."