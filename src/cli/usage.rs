//! `edge-cli` usage / help text.
//!
//! P2-D3.3: the dispatcher calls into here for `--help`, `--version`,
//! missing-subcommand, and unknown-subcommand paths. All four print to
//! stderr (matching the convention the deleted `edge-python` binary
//! followed) and return a process exit code; the dispatcher uses
//! `return code` from `print_*_and_return` directly.

/// Top-of-usage line. Single source of truth — printed by both `--help`
/// and the "no subcommand" path.
pub const USAGE_TOP: &str = "edge-cli <SUBCOMMAND> [args...]";

/// Subcommands listed under `--help`. Status legend:
///
/// - `(wired)` — fully functional, see subcommand module for arg shape.
/// - `(pending D3.4+)` — stub; returns "not yet implemented".
pub const HELP_BODY: &str = "\
SUBCOMMANDS:
  run <wasm> [--] [args...]              (wired)  Execute a wasm guest one-shot.
  freeze <wasm> --out <path>             (pending D3.5)  Snapshot a live kernel.
  serve <snap> [--port <p>]              (pending D3.5)  Restore + serve from a snapshot.
  bench <wasm> --iters <n>               (pending D3.7)  Measure cold-start latency.
  trace <wasm> [--diff <baseline>] [--no-marker]  (wired)  JSON-line syscall tracer.

FLAGS:
  --help, -h                             Print this help to stderr.
  --version, -V                          Print the version to stderr.

EXIT CODES:
  0    success
  1    runtime error (snapshot, wasmtime, io)
  2    usage error (unknown subcommand, bad flags)";

/// Print help text to stderr and return the exit code the dispatcher
/// should propagate. `--help` exits 0; the "missing subcommand" path
/// (called by the dispatcher when no positional args were given) exits 2.
pub fn print_help_and_return(code: i32) -> i32 {
    eprintln!("{USAGE_TOP}");
    eprintln!();
    eprintln!("{HELP_BODY}");
    code
}

/// Print a "unknown subcommand" line to stderr and return 2.
pub fn print_unknown_and_return(name: &str, code: i32) -> i32 {
    eprintln!("edge-cli: unknown subcommand `{name}`");
    eprintln!("{USAGE_TOP}");
    eprintln!("(run `edge-cli --help` for the full subcommand list)");
    code
}

/// Print the version to stderr and return 0.
///
/// The version string is the crate's `CARGO_PKG_VERSION` (read at build
/// time via `env!`). Bumping `Cargo.toml` is the only way to bump it.
pub fn print_version_and_return() -> i32 {
    eprintln!("edge-cli {}", env!("CARGO_PKG_VERSION"));
    0
}
