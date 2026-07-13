#!/usr/bin/env bash
#
# scripts/preflight.sh — local mirror of the GitHub Actions CI workflow.
#
# Runs the exact same steps as .github/workflows/ci.yml in the same order,
# with the same exit semantics, so a contributor can run `preflight.sh`
# locally and trust "if this passed, CI will pass."
#
# Keep this file and .github/workflows/ci.yml in sync.
#
# Usage:
#   bash scripts/preflight.sh             # run everything
#   PREFLIGHT_SKIP=cpython bash preflight.sh  # (no-op today; reserved)
#
# Exit codes:
#   0 = all steps green
#   non-zero = the offending step; the failure message names which step failed
#
# This script mirrors CI-1's ubuntu-22.04 environment, but the same
# commands work on macOS or Linux dev hosts. CPython steps auto-skip on
# any host that lacks a guest/cpython submodule.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PASS=0
FAIL=0
SKIPPED=0
FAILED_STEPS=()

# Run a step: print its number + name, run the command, record exit.
# Args: <step-number> <step-name> <command...>
run_step() {
    local num="$1"
    local name="$2"
    shift 2

    printf '\n=== Step %s: %s ===\n' "$num" "$name"

    # We deliberately do NOT `set -e`; we want each step's exit to be
    # captured so we can summarize at the end.
    local rc=0
    "$@" || rc=$?

    if [[ $rc -eq 0 ]]; then
        PASS=$((PASS + 1))
        echo "[step $num: PASS] $name"
    else
        FAIL=$((FAIL + 1))
        FAILED_STEPS+=("Step $num: $name (exit $rc)")
        echo "[step $num: FAIL exit=$rc] $name" >&2
    fi
    return 0
}

# --- 1. Environment checks (cheap; bail-fast on missing tools) ---
if ! command -v cargo >/dev/null 2>&1; then
    echo "FAIL: cargo not found in PATH" >&2
    echo "  → Run: bash scripts/dev_setup.sh" >&2
    exit 1
fi
if ! command -v zig >/dev/null 2>&1; then
    echo "FAIL: zig not found in PATH" >&2
    echo "  → Run: bash scripts/dev_setup.sh (installs zig 0.16)" >&2
    exit 1
fi
if ! command -v wat2wasm >/dev/null 2>&1; then
    echo "WARN: wat2wasm (wabt) not found; some C conformance tests may fail to build"
fi

# Detect whether the repo is on the rust-toolchain version.
EXPECTED_TOOLCHAIN="$(grep -oE '"[0-9]+\.[0-9]+\.[0-9]+"' rust-toolchain.toml | head -1 | tr -d '"')"
ACTUAL_TOOLCHAIN="$(rustc --version | awk '{print $2}')"
if [[ -n "$EXPECTED_TOOLCHAIN" && "$EXPECTED_TOOLCHAIN" != "$ACTUAL_TOOLCHAIN" ]]; then
    echo "WARN: rust $ACTUAL_TOOLCHAIN active, but rust-toolchain.toml pins $EXPECTED_TOOLCHAIN" >&2
    echo "  → rustup will pick the right toolchain when you 'cargo build'" >&2
fi

# --- 2. Run the same steps as the CI workflow ---

# --- 0. Static-analysis gates (mirror .github/workflows/ci.yml) ---
# Steps 0a-0f are cargo-side; they share step 2's warm target/.
# Steps 0g-0j are non-cargo; tools-side linters. Each non-cargo step
# silently SKIPs when its binary isn't installed locally so a contributor
# without all tools can still get a partial green signal.

run_step 0a "NR-consistency check (Rust ↔ C ↔ dispatch)" \
    bash scripts/check_nr_consistency.sh

run_step 0b "cargo fmt --check" \
    bash -c 'cargo fmt --all -- --check'

run_step 0c "cargo clippy --all-targets (-D warnings)" \
    bash -c 'cargo clippy --profile ci --all-targets -- -D warnings'

run_step 0d "cargo doc --no-deps (broken-docs gate)" \
    bash -c 'RUSTDOCFLAGS="-D warnings" cargo doc --profile ci --no-deps --document-private-items >/dev/null'

run_step 0e "cargo-deny (advisories, licenses, sources, bans)" \
    bash -c 'command -v cargo-deny >/dev/null 2>&1 || { echo "SKIP: cargo-deny not installed (cargo install --locked cargo-deny)"; exit 0; }; cargo deny check'

run_step 0f "cargo-machete (unused dependency detection)" \
    bash -c 'command -v cargo-machete >/dev/null 2>&1 || { echo "SKIP: cargo-machete not installed (cargo install --locked cargo-machete)"; exit 0; }; cargo machete'

run_step 0g "actionlint (GitHub Actions syntax check)" \
    bash -c 'command -v actionlint >/dev/null 2>&1 || { echo "SKIP: actionlint not installed (brew install actionlint or grab GitHub release tarball)"; exit 0; }; actionlint'

run_step 0h "shellcheck (shell-script lint)" \
    bash -c 'command -v shellcheck >/dev/null 2>&1 || { echo "SKIP: shellcheck not installed (apt install shellcheck / brew install shellcheck)"; exit 0; }; shellcheck scripts/*.sh tests/*.sh tests/conformance/runner.sh tests/strace_baselines/*.sh guest/build.sh'

run_step 0i "gitleaks (secret-leak scan)" \
    bash -c 'command -v gitleaks >/dev/null 2>&1 || { echo "SKIP: gitleaks not installed (brew install gitleaks or grab GitHub release tarball)"; exit 0; }; gitleaks detect --source . --config .gitleaks.toml --no-banner --redact'

run_step 0j "wat2wasm validation (WAT syntax check)" \
    bash -c 'command -v wat2wasm >/dev/null 2>&1 || { echo "SKIP: wabt not installed (apt install wabt)"; exit 0; }; for wat in $(find tests guests -name "*.wat" -not -path "*/target/*" 2>/dev/null); do echo "==> $wat"; wat2wasm "$wat" -o /tmp/check.wasm || { echo "FAIL: $wat"; exit 1; }; done'

run_step 1 "cargo build --profile ci (trace-host + edge-python)" \
    bash -c 'cargo build --profile ci --bin trace-host --bin edge-python'

run_step 2 "cargo test --profile ci (full Rust suite)" \
    cargo test --profile ci

run_step 3 "C conformance (marker-enforced)" \
    bash tests/conformance/runner.sh

run_step 4 "strace baseline diff" \
    bash -c 'cargo test --profile ci --test strace_baseline_diff'

run_step 5 "scripts/reproduce_dod.sh (8 steps)" \
    bash scripts/reproduce_dod.sh

# --- 3. Final summary (mirrors the CI workflow's job summary) ---
echo
echo "=== Preflight summary ==="
echo "Pass: $PASS  Fail: $FAIL"
if [[ $FAIL -gt 0 ]]; then
    echo "Failed steps:" >&2
    for s in "${FAILED_STEPS[@]}"; do
        echo "  - $s" >&2
    done
fi
echo
echo "=== Canonical test totals (from tests/count_tests.sh) ==="
bash tests/count_tests.sh

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
