#!/usr/bin/env bash
#
# scripts/dev_setup.sh — install P0 toolchain dependencies.
#
# Idempotent: skips tools already present at compatible versions.
#
# Toolchain:
#   - zig 0.16.0 (for `zig cc` cross-compile of CPython)
#   - wabt (for `wat2wasm`, used by the conformance suite)
#   - strace (Linux only, for `tests/strace_baselines/strace_native.sh`)
#
# macOS detection: prefers `brew`; falls back to instructions.
# Linux detection: prefers `apt`; falls back to instructions.

set -euo pipefail

say() { echo "==> $*"; }
warn() { echo "WARN: $*" >&2; }
fail() { echo "FAIL: $*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

zig_version_required="0.16"
zig_installed=""
if have zig; then
    zig_installed=$(zig version 2>/dev/null || true)
    say "zig already installed: ${zig_installed:-unknown}"
fi

if [[ -z "$zig_installed" || "$zig_installed" != ${zig_version_required}* ]]; then
    if [[ "$(uname -s)" == "Darwin" ]] && have brew; then
        say "installing zig via brew"
        brew install zig
    elif [[ "$(uname -s)" == "Linux" ]] && [[ "$(uname -m)" == "x86_64" ]]; then
        # Linux x86_64: apt's zig is 0.10; download the release tarball
        # directly. This is the same install path CI uses — see
        # .github/workflows/ci.yml step 5. Tarball name is
        # `zig-x86_64-linux-<ver>.tar.xz`.
        say "installing zig ${zig_version_required} via release tarball"
        ZIG_VER="0.16.0"
        ZIG_URL="https://ziglang.org/download/${ZIG_VER}/zig-x86_64-linux-${ZIG_VER}.tar.xz"
        TMPZIG=$(mktemp -d)
        trap 'rm -rf "$TMPZIG"' EXIT
        if have curl; then
            curl -fSL -o "$TMPZIG/zig.tar.xz" "$ZIG_URL" || warn "zig download failed: $ZIG_URL"
        fi
        if [[ -f "$TMPZIG/zig.tar.xz" ]]; then
            sudo mkdir -p /opt/zig
            sudo tar -xJf "$TMPZIG/zig.tar.xz" -C /opt/zig --strip-components=1
            echo "/opt/zig" | sudo tee /etc/ld.so.conf.d/zig.conf >/dev/null
            sudo ldconfig
            warn "zig ${ZIG_VER} installed at /opt/zig — add /opt/zig to PATH for this shell"
        else
            warn "apt's zig is often 0.10; for 0.16 retry or download manually: $ZIG_URL"
            sudo apt-get install -y zig || warn "apt install zig failed"
        fi
    elif have apt-get; then
        # Non-x86_64 Linux (aarch64 etc.) — fall back to apt with a warning.
        say "installing zig via apt (may need manual download for 0.16 on non-x86_64)"
        warn "apt's zig is often 0.10; for 0.16 try: https://ziglang.org/download/"
        sudo apt-get install -y zig || warn "apt install zig failed"
    else
        warn "install zig 0.16 manually: https://ziglang.org/download/"
    fi
fi

if ! have wat2wasm; then
    if [[ "$(uname -s)" == "Darwin" ]] && have brew; then
        say "installing wabt via brew"
        brew install wabt
    elif have apt-get; then
        sudo apt-get install -y wabt || warn "apt install wabt failed"
    else
        warn "install wabt manually: https://github.com/WebAssembly/wabt"
    fi
else
    say "wat2wasm already installed"
fi

if [[ "$(uname -s)" == "Linux" ]] && ! have strace; then
    if have apt-get; then
        say "installing strace via apt"
        sudo apt-get install -y strace
    else
        warn "install strace manually for native syscall baselines"
    fi
elif have strace; then
    say "strace already installed: $(strace -V 2>&1 | head -1)"
fi

# Rust toolchain — pinned via rust-toolchain.toml so `cargo` from rustup
# will pick the right toolchain automatically.
if ! have cargo; then
    fail "cargo not found. Install rustup: https://rustup.rs/"
else
    say "cargo: $(cargo --version)"
fi

say "dev setup complete."
echo
echo "Next:"
echo "  cargo build --release"
echo "  cargo test --release"