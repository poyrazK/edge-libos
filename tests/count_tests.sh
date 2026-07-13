#!/usr/bin/env bash
#
# tests/count_tests.sh — single source of truth for the "tests pass" total.
#
# Counts:
#   1. Rust unit tests     (those whose path contains `::`, i.e. inside #[cfg(test)] mods)
#   2. Rust integration tests (top-level integration binary names)
#   3. C conformance tests (one .c file per test in tests/conformance/)
#
# The C conformance runner prints the same total at the end of its run, so
# `tests/count_tests.sh` and `tests/conformance/runner.sh` always agree.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# 1 + 2. Rust tests from `cargo test -- --list`.
list_output=$(cargo test --release -- --list 2>&1 || true)

# Unit tests have at least one `::` in their path; integration tests are
# top-level test function names only.
unit=$(printf '%s\n' "$list_output" \
    | grep ': test$' \
    | sed 's/^[[:space:]]*//' \
    | grep -c '::' || true)
integ=$(printf '%s\n' "$list_output" \
    | grep ': test$' \
    | sed 's/^[[:space:]]*//' \
    | grep -cv '::' || true)

# 3. C conformance tests: one .c file per test (excluding any non-test files).
c_count=$(ls "$ROOT"/tests/conformance/*.c 2>/dev/null | wc -l | tr -d ' ')

rust_total=$((unit + integ))
grand=$((rust_total + c_count))

cat <<EOF
Rust tests : ${rust_total}  (unit: ${unit}, integration: ${integ})
C tests    : ${c_count}
Total      : ${grand}
EOF