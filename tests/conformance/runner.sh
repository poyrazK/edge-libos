#!/usr/bin/env bash
#
# Conformance runner: compile each `tests/conformance/*.c` with `zig cc`,
# drive the resulting .wasm through `edge-cli trace`, and verify the JSON
# trace contains the expected syscall name.
#
# This is the C-side equivalent of `tests/*_conformance.rs`. Each test
# exercises one syscall through the real musl-style C ABI (imported
# via zig cc's LLD backend), so it validates the full wasm32-musl
# calling convention against our single-import dispatch.
#
# Pre-reqs:
#   - zig 0.13+ (tested with 0.16.0)
#   - cargo build --release --bin edge-cli
#
# Usage:
#   bash tests/conformance/runner.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
ZIG=${ZIG:-zig}
CC="$ZIG cc -target wasm32-freestanding -O2"
EDGE_CLI="$ROOT/target/release/edge-cli"

if ! command -v "$ZIG" >/dev/null 2>&1; then
    echo "FAIL: zig not found in PATH"
    exit 1
fi

if [[ ! -x "$EDGE_CLI" ]]; then
    echo "Building edge-cli (release)..."
    (cd "$ROOT" && cargo build --release --bin edge-cli >/dev/null)
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Per-test name → expected syscall name to observe in edge-cli trace JSON.
# Implemented as a function (bash 3.2 on macOS doesn't have associative arrays).
# The default arm fails loudly for unregistered names — it used to silently
# fall back to "exit", which masked forgotten registrations.
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
        getdents64_stream_position) echo "getdents64" ;;
        rt_sigaction)      echo "rt_sigaction" ;;
        rt_sigprocmask)    echo "rt_sigprocmask" ;;
        pipe2)             echo "pipe2" ;;
        pipe)              echo "pipe" ;;
        socket)            echo "socket" ;;
        bind)              echo "bind" ;;
        listen)            echo "listen" ;;
        setsockopt)        echo "setsockopt" ;;
        getsockopt)        echo "getsockopt" ;;
        getsockname)       echo "getsockname" ;;
        shutdown)          echo "shutdown" ;;
        poll)              echo "poll" ;;
        poll_timeout)      echo "poll" ;;
        epoll_create1)     echo "epoll_create1" ;;
        epoll_ctl)         echo "epoll_ctl" ;;
        epoll_wait)        echo "epoll_wait" ;;
        eventfd2)          echo "eventfd2" ;;
        fcntl_nonblock)    echo "pipe2" ;;
        connect)           echo "connect" ;;
        open)              echo "open" ;;
        stat)              echo "stat" ;;
        statx)             echo "statx" ;;
        dup)               echo "dup" ;;
        dup2)              echo "dup2" ;;
        dup3)              echo "dup3" ;;
        f_dupfd_min)       echo "fcntl" ;;
        f_dupfd_min_below_next) echo "fcntl" ;;
        mkdir)             echo "mkdir" ;;
        mkdirat)           echo "mkdirat" ;;
        rmdir)             echo "rmdir" ;;
        unlink)            echo "unlink" ;;
        unlinkat)          echo "unlinkat" ;;
        rename)            echo "rename" ;;
        renameat2_noreplace) echo "renameat2" ;;
        ftruncate)         echo "ftruncate" ;;
        truncate)          echo "truncate" ;;
        readlink)          echo "readlink" ;;
        readlinkat)        echo "readlinkat" ;;
        symlink)           echo "symlink" ;;
        symlinkat)         echo "symlinkat" ;;
        link)              echo "link" ;;
        linkat)            echo "linkat" ;;
        utimensat)         echo "utimensat" ;;
        chmod)             echo "chmod" ;;
        fchmod)            echo "fchmod" ;;
        fchmodat)          echo "fchmodat" ;;
        faccessat)         echo "faccessat" ;;
        faccessat2)        echo "faccessat2" ;;
        chdir)             echo "chdir" ;;
        chroot)            echo "chroot" ;;
        getppid)           echo "getppid" ;;
        uname)             echo "uname" ;;
        prlimit64_self)    echo "prlimit64" ;;
        sched_yield)       echo "sched_yield" ;;
        sched_getaffinity) echo "sched_getaffinity" ;;
        prctl_set_get_name) echo "prctl" ;;
        clock_getres)      echo "clock_getres" ;;
        clock_nanosleep)   echo "clock_nanosleep" ;;
        sigaltstack)       echo "sigaltstack" ;;
        rt_sigreturn)      echo "rt_sigreturn" ;;
        kill_self)         echo "kill" ;;
        tgkill_self)       echo "tgkill" ;;
        mremap_identity)   echo "mremap" ;;
        ioctl_tiocgwinsz)  echo "ioctl" ;;
        ioctl_fionbio)     echo "ioctl" ;;
        getsid)            echo "getsid" ;;
        setsid)            echo "setsid" ;;
        getgroups)         echo "getgroups" ;;
        lstat)             echo "lstat" ;;
        getcwd)            echo "getcwd" ;;
        readv)             echo "readv" ;;
        writev)            echo "writev" ;;
        rseq)              echo "rseq" ;;
        arch_prctl)        echo "arch_prctl" ;;
        nanosleep)         echo "nanosleep" ;;
        exit)              echo "exit" ;;
        sendmsg)           echo "sendmsg" ;;
        recvmsg)           echo "recvmsg" ;;
        ppoll)             echo "ppoll" ;;
        epoll_pwait)       echo "epoll_pwait" ;;
        select)            echo "select" ;;
        eventfd_legacy)    echo "eventfd" ;;
        socketpair)        echo "socketpair" ;;
        af_unix_abstract_returns_eopnotsupp) echo "bind" ;;
        af_unix_bind_connect) echo "connect" ;;
        sysinfo)             echo "sysinfo" ;;
        times)               echo "times" ;;
        clone)               echo "clone" ;;
        fork)                echo "fork" ;;
        wait4)               echo "wait4" ;;
        futex)               echo "futex" ;;
        *) echo "UNREGISTERED: $1" >&2; return 1 ;;
    esac
}

PASS=0
FAIL=0
FAIL_NAMES=()
SOFT_PASS_NAMES=()

# Default to soft mode: tests without a mark_pass/mark_fail marker fall back
# to the syscall-name grep. STRICT=1 makes missing markers a hard fail; flip
# the default once all C tests have been migrated (post-B1).
STRICT="${STRICT:-0}"

# Pre-create the directory used by getdents64_stream_position.c.
# P2-B2: this test asserts the kernel tracks dir-stream position across
# multiple getdents64 calls. We create the dir under $ROOT (the edge-cli
# process's cwd, which becomes the kernel's cwd) and clean it up at exit.
GD_DIR="$ROOT/getdents64_dir"
mkdir -p "$GD_DIR"
echo "foo" > "$GD_DIR/foo"
echo "bar" > "$GD_DIR/bar"
echo "baz" > "$GD_DIR/baz"
# Schedule cleanup even if we exit non-zero. We deliberately overwrite
# any prior EXIT trap — runner.sh is sourced/called from CI jobs that
# set their own cleanup, but those only run `bash tests/conformance/runner.sh`
# and never observe intermediate state, so chaining isn't needed.
trap 'rm -rf "$GD_DIR"' EXIT

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

    # Note: removed `|| true` — a guest trap is a real failure now.
    trace_json=$("$EDGE_CLI" trace "$wasm" 2>/dev/null)
    trace_rc=$?

    if [[ $trace_rc -ne 0 ]]; then
        FAIL=$((FAIL + 1))
        FAIL_NAMES+=("$name")
        echo "FAIL  $name (edge-cli trace exited $trace_rc; guest likely trapped)"
        echo "$trace_json" | tail -3 | sed 's/^/    /'
        continue
    fi

    # Extract marker from the trailing JSON line emitted by edge-cli trace.
    marker=$(echo "$trace_json" | grep -oE '\{"marker":"[^"]*"' | head -1 | sed 's/^{"marker":"//; s/"$//')
    expected=$(expected_syscall "$name") || {
        FAIL=$((FAIL + 1))
        FAIL_NAMES+=("$name (unregistered)")
        echo "FAIL  $name (no expected_syscall mapping)"
        continue
    }

    if [[ -n "$marker" ]]; then
        # Test wrote a marker — trust it.
        case "$marker" in
            PASS)
                PASS=$((PASS + 1))
                echo "PASS  $name"
                ;;
            FAIL:*)
                FAIL=$((FAIL + 1))
                FAIL_NAMES+=("$name")
                reason="${marker#FAIL:}"
                echo "FAIL  $name ($reason)"
                ;;
            *)
                # Unrecognized marker prefix. Treat as fail.
                FAIL=$((FAIL + 1))
                FAIL_NAMES+=("$name")
                echo "FAIL  $name (unrecognized marker: $marker)"
                ;;
        esac
    else
        # No marker — old-style test. Fall back to syscall-name grep in
        # soft mode; fail outright in strict mode.
        if [[ "$STRICT" == "1" ]]; then
            FAIL=$((FAIL + 1))
            FAIL_NAMES+=("$name")
            echo "FAIL  $name (no marker; STRICT=1 requires mark_pass/mark_fail)"
        else
            if echo "$trace_json" | grep -q "\"name\":\"$expected\""; then
                PASS=$((PASS + 1))
                SOFT_PASS_NAMES+=("$name")
                echo "PASS* $name  (no marker; syscall '$expected' found)"
            else
                FAIL=$((FAIL + 1))
                FAIL_NAMES+=("$name")
                echo "FAIL  $name (no marker, expected syscall '$expected' not in trace)"
            fi
        fi
    fi
done

echo
echo "Pass: $PASS  Fail: $FAIL  Soft: ${#SOFT_PASS_NAMES[@]}"
if [[ ${#SOFT_PASS_NAMES[@]} -gt 0 ]]; then
    echo "Soft passes (no marker; migrate to mark_pass): ${SOFT_PASS_NAMES[*]}"
fi

# Total test count (kept in sync with tests/count_tests.sh). Printed on
# every run, success or failure, so the operator sees the same number the
# HANDOFF and README claim.
#
# `cargo test --list` compiles the ENTIRE dependency graph just to enumerate
# tests. In CI this runner is invoked from the `c-conformance` job, which has
# no cargo cache — so this cosmetic count would trigger a full cold release
# build of wasmtime. Set SKIP_TEST_COUNT=1 there to skip it; the C count still
# prints. `tests/count_tests.sh` remains the source of truth off the hot path.
if [[ "${SKIP_TEST_COUNT:-0}" == "1" ]]; then
    c_total=$(ls "$ROOT"/tests/conformance/*.c 2>/dev/null | wc -l | tr -d ' ')
    echo "Test totals: C=$c_total (Rust count skipped: SKIP_TEST_COUNT=1)"
else
    list_output=$(cd "$ROOT" && cargo test --release -- --list 2>&1 || true)
    rust_unit=$(printf '%s\n' "$list_output" | grep ': test$' | sed 's/^[[:space:]]*//' | grep -c '::' || true)
    rust_integ=$(printf '%s\n' "$list_output" | grep ': test$' | sed 's/^[[:space:]]*//' | grep -cv '::' || true)
    rust_total=$((rust_unit + rust_integ))
    c_total=$(ls "$ROOT"/tests/conformance/*.c 2>/dev/null | wc -l | tr -d ' ')
    grand=$((rust_total + c_total))
    echo "Test totals: Rust=$rust_total (unit=$rust_unit, integ=$rust_integ), C=$c_total, Grand=$grand"
fi

if [[ $FAIL -gt 0 ]]; then
    echo "Failed: ${FAIL_NAMES[*]}"
    exit 1
fi
exit 0