#!/usr/bin/env bash
#
# guest/build.sh — cross-compile CPython to wasm32-musl + edge-libos ABI.
#
# Pipeline:
#   1. Verify zig cc + wabt + CPython submodule are present.
#   2. Stage musl_syscall.c into guest/cpython/Modules/ (CPython's
#      Modules/Setup.local pulls it into the link).
#   3. ./configure with our cross-compile flags.
#   4. make (compile libpython + frozen importlib).
#   5. Compile guest/syscall_shim/main.c against libpython + musl shim.
#   6. Link with --import-memory --import-table.
#   7. Verify the output imports `kernel.syscall`.
#
# Output:
#   target/wasm32-unknown-linux-musl/release/python.wasm
#
# This is the highest-risk step in P0. Two things tend to go wrong:
#   (a) zig 0.16.0 dropped `--sysroot=<zigstd>`; we use the modern path
#       `$ZIG/lib/zig/libc/wasi/libc-top-half/headers` for includes.
#   (b) CPython's configure has a long tail of optional features that
#       need to be disabled. The `--disable-*` list is in step 3.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
GUEST="$ROOT/guest"
CPYTHON="$GUEST/cpython"
OUT="${EDGE_LIBOS_WASM_OUT:-$ROOT/target/wasm32-unknown-linux-musl/release/python.wasm}"

ZIG=${ZIG:-zig}
WAT2WASM=${WAT2WASM:-$(command -v wat2wasm || echo "")}

if ! command -v "$ZIG" >/dev/null 2>&1; then
    echo "FAIL: zig not found in PATH (set ZIG=<path>)"
    exit 1
fi

if [[ ! -d "$CPYTHON" ]]; then
    echo "FAIL: $CPYTHON does not exist. Run:"
    echo "    git submodule add https://github.com/python/cpython.git guest/cpython"
    echo "    (cd guest/cpython && git checkout v3.13.7 && git submodule update --init --recursive)"
    exit 1
fi

# shellcheck disable=SC2034  # consumed by generated Makefile recipe (line 63)
CC="$ZIG cc -target wasm32-freestanding -O2"
# zig 0.16.0 musl sysroot (headers only — we provide our own shim, no libc).
MUSL_HEADERS="$("$ZIG" env 2>/dev/null | grep -oE 'lib_dir=.*' | head -1 | cut -d= -f2-)"
if [[ -z "$MUSL_HEADERS" ]]; then
    # Fallback for older zig without `env` JSON.
    MUSL_HEADERS="/opt/homebrew/Cellar/zig/0.16.0_1/lib/zig/libc/wasi/libc-top-half/headers"
fi

# 1. Stage our syscall shim into Modules/. CPython's Setup.local picks it up.
SHIM_SRC="$GUEST/syscall_shim/musl_syscall.c"
SETUP_LOCAL="$CPYTHON/Modules/Setup.local"
mkdir -p "$CPYTHON/Modules"
{
    echo "# edge-libos syscall shim"
    echo "*syscall-shim*"
    echo "musl_syscall.o: $SHIM_SRC"
    echo "    \$(CC) \$(PY_CORE_CFLAGS) -c $SHIM_SRC -o musl_syscall.o"
} >> "$SETUP_LOCAL"

# 2. Configure. Disable everything we don't need for P0; threads + ssl + ipc
#    would require posix_spawn/pthread/mutex machinery we don't have yet.
cd "$CPYTHON"
./configure \
    --host=wasm32-unknown-linux-musl \
    --disable-shared \
    --disable-threads \
    --without-threads \
    --disable-pyc \
    --disable-test-modules \
    --disable-ipv6 \
    --disable-crypt \
    --disable-dlopen \
    --disable-ssl \
    --disable-static-libpython \
    CC="$ZIG cc" \
    AR="$ZIG ar" \
    RANLIB="$ZIG ranlib" \
    CFLAGS="-target wasm32-freestanding -O2 -Dwasm32 -D__wasm__ -DWASM=1" \
    LIBFFI_LIBS="" \
    2>&1 | tee "$ROOT/target/build-configure.log"

# 3. Build.
make regen-importlib
make -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)" 2>&1 | tee "$ROOT/target/build-make.log"

# 4. Compile our entry point against libpython + the shim.
ENTRY_OBJS=()
for src in musl_syscall.c main.c; do
    obj="$ROOT/target/build-${src%.c}.o"
    "$ZIG" cc -target wasm32-freestanding -O2 -c \
        -I"$CPYTHON/Include" -I"$CPYTHON" \
        "$GUEST/syscall_shim/$src" -o "$obj"
    ENTRY_OBJS+=("$obj")
done

# 4b. P2-DNS: compile the getaddrinfo/freeaddrinfo musl overrides
# (ADR 0007). These objects appear in ENTRY_OBJS so they precede
# libpython + musl on the link line — wasm-ld resolves duplicate
# symbols by first-definition-wins, so our getaddrinfo shadows the
# libc one for any caller that pulls it in via CPython's socket
# module or httpx's resolver path.
GUEST_RESOLVER="$GUEST/resolver"
for src in getaddrinfo.c freeaddrinfo.c; do
    obj="$ROOT/target/build-resolver-${src%.c}.o"
    "$ZIG" cc -target wasm32-freestanding -O2 -c \
        -I"$GUEST_RESOLVER" \
        "$GUEST_RESOLVER/$src" -o "$obj"
    ENTRY_OBJS+=("$obj")
done

# 5. Link.
mkdir -p "$(dirname "$OUT")"
"$ZIG" cc -target wasm32-freestanding -O2 \
    -Wl,--import-memory \
    -Wl,--import-table \
    -Wl,--max-memory=2147483648 \
    -Wl,--export=__heap_base \
    -Wl,--export=__data_end \
    -Wl,--export=malloc \
    -Wl,--export=free \
    -Wl,--allow-undefined \
    "${ENTRY_OBJS[@]}" \
    "$CPYTHON/libpython3.13.a" \
    -o "$OUT"

# 6. Verify the output imports kernel.syscall.
echo
echo "WASM -> $OUT"
echo "Verifying kernel.syscall import..."
if grep -q "kernel.syscall" "$OUT" 2>/dev/null; then
    echo "  ✓ kernel.syscall found in $OUT"
else
    echo "  ✗ kernel.syscall NOT found in $OUT — link failed?"
    exit 1
fi
echo "Done."