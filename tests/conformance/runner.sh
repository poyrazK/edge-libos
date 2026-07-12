#!/usr/bin/env bash
#
# Conformance runner: compile each `tests/conformance/*.c` with `zig cc`,
# drive the resulting .wasm through `trace-host`, and verify the JSON
# trace contains the expected syscall name.
#
# This is the C-side equivalent of `tests/*_conformance.rs`. Each test
# exercises one syscall through the real musl-style C ABI (imported
# via zig cc's LLD backend), so it validates the full wasm32-musl
# calling convention against our single-import dispatch.
#
# Pre-reqs:
#   - zig 0.13+ (tested with 0.16.0)
#   - cargo build --release --bin trace-host
#
# Usage:
#   bash tests/conformance/runner.sh

set -o pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ZIG=${ZIG:-zig}
CC="$ZIG cc -target wasm32-freestanding -O2"
TRACE_HOST="$ROOT/target/release/trace-host"

if ! command -v "$ZIG" >/dev/null 2>&1; then
    echo "FAIL: zig not found in PATH"
    exit 1
fi

if [[ ! -x "$TRACE_HOST" ]]; then
    echo "Building trace-host (release)..."
    (cd "$ROOT" && cargo build --release --bin trace-host >/dev/null)
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Per-test name → expected syscall name to observe in trace-host JSON.
# Implemented as a function (bash 3.2 on macOS doesn't have associative arrays).
expected_syscall() {
    case "$1" in
        getpid)            echo "getpid" ;;
        getuid)            echo "getuid" ;;
        set_tid_address)   echo "set_tid_address" ;;
        clock_gettime)     echo "clock_gettime" ;;
        getrandom)         echo "getrandom" ;;
        brk)               echo "brk" ;;
        mmap_anon)         echo "mmap" ;;
        munmap)            echo "munmap" ;;
        mprotect)          echo "mprotect" ;;
        read_write_stdio)  echo "write" ;;
        openat_close)      echo "openat" ;;
        lseek)             echo "lseek" ;;
        fstat)             echo "fstat" ;;
        getdents64)        echo "getdents64" ;;
        rt_sigaction)      echo "rt_sigaction" ;;
        rt_sigprocmask)    echo "rt_sigprocmask" ;;
        pipe2)             echo "pipe2" ;;
        pipe)              echo "pipe" ;;
        socket)            echo "socket" ;;
        bind)              echo "bind" ;;
        listen)            echo "listen" ;;
        open)              echo "open" ;;
        stat)              echo "stat" ;;
        lstat)             echo "lstat" ;;
        getcwd)            echo "getcwd" ;;
        readv)             echo "readv" ;;
        writev)            echo "writev" ;;
        rseq)              echo "rseq" ;;
        arch_prctl)        echo "arch_prctl" ;;
        nanosleep)         echo "nanosleep" ;;
        exit)              echo "exit" ;;
        *)                 echo "exit" ;;
    esac
}

PASS=0
FAIL=0
FAIL_NAMES=()

for c in "$ROOT"/tests/conformance/*.c; do
    name=$(basename "$c" .c)
    wasm="$TMP/$name.wasm"
    if ! $CC -I"$ROOT/tests/conformance" "$c" -o "$wasm" 2>"$TMP/build.err"; then
        echo "BUILD FAIL: $name"
        cat "$TMP/build.err"
        FAIL=$((FAIL + 1))
        FAIL_NAMES+=("$name (build)")
        continue
    fi

    trace_json=$("$TRACE_HOST" "$wasm" 2>/dev/null || true)

    expected=$(expected_syscall "$name")
    if echo "$trace_json" | grep -q "\"name\":\"$expected\""; then
        PASS=$((PASS + 1))
        echo "PASS  $name"
    else
        FAIL=$((FAIL + 1))
        FAIL_NAMES+=("$name")
        echo "FAIL  $name (expected syscall '$expected' in trace)"
        echo "$trace_json" | head -3 | sed 's/^/    /'
    fi
done

echo
echo "Pass: $PASS  Fail: $FAIL"
if [[ $FAIL -gt 0 ]]; then
    echo "Failed: ${FAIL_NAMES[*]}"
    exit 1
fi
exit 0