//! `edge-cli` — the single CLI entry point.
//!
//! P2-D3.3: this module is the top-level dispatcher. It maps
//! `std::env::args()` to one of the subcommand modules under
//! `src/cli/{run,freeze,serve,bench,trace}.rs`, builds a current-thread
//! tokio runtime for the duration of the call, and translates the
//! per-subcommand `CliResult<i32>` into a process exit code.
//!
//! ## Exit code semantics
//!
//! - `0` — success (subcommand returned `Ok(0)`, OR `--help` /
//!   `--version` was requested).
//! - `1` — runtime error (`CliError::Snapshot`, `CliError::Wasmtime`,
//!   `CliError::Io`, OR a non-zero exit code bubbled up from the
//!   subcommand itself).
//! - `2` — usage error (`CliError::Args`, `CliError::Unknown`,
//!   `CliError::MissingSubcommand`).
//!
//! ## Testability
//!
//! `run_main` reads from `std::env::args()`. Tests that need a fixed
//! argv path call `run_main_from(iter)` directly. (`src/bin/edge_cli.rs`
//! only invokes the no-arg form.)
//!
//! ## `--diff` semantics
//!
//! The `trace` subcommand is the only one with a non-trivial exit code
//! path: `--diff <baseline>` returns `Ok(1)` (mapped to host exit 1) when
//! baseline syscalls are missing from the trace. This is intentional —
//! `tests/strace_baseline_diff.rs` parses the exit code directly.

pub mod bench;
pub mod error;
pub mod freeze;
pub mod migrate;
pub mod run;
pub mod serve;
pub mod subcommand;
pub mod trace;
pub mod usage;
pub mod util;

use crate::cli::error::{CliError, CliResult};
use crate::cli::subcommand::Subcommand;

/// Process entry. Reads argv from the environment and delegates.
pub fn run_main() -> i32 {
    run_main_from(std::env::args().skip(1))
}

/// Test-friendly dispatcher. Takes any iterator of `String` so callers
/// can pin argv without touching `std::env`.
///
/// Exit-code mapping lives at the bottom of this function; subcommands
/// return their own exit code via `Ok(i32)` and the dispatcher honors it.
pub fn run_main_from<I: IntoIterator<Item = String>>(args: I) -> i32 {
    let mut it = args.into_iter();
    let first = match it.next() {
        None => return usage::print_help_and_return(2),
        Some(a) => a,
    };
    if first == "--help" || first == "-h" {
        return usage::print_help_and_return(0);
    }
    if first == "--version" || first == "-V" {
        return usage::print_version_and_return();
    }

    let sub = match first.parse::<Subcommand>() {
        Ok(s) => s,
        Err(_) => return usage::print_unknown_and_return(&first, 2),
    };
    let rest: Vec<String> = it.collect();

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("edge-cli: tokio runtime build failed: {e}");
            return 1;
        }
    };

    let res: CliResult<i32> = match sub {
        Subcommand::Run => rt.block_on(run::run_main(&rest)),
        Subcommand::Freeze => rt.block_on(freeze::run_main(&rest)),
        Subcommand::Serve => rt.block_on(serve::run_main(&rest)),
        Subcommand::Bench => rt.block_on(bench::run_main(&rest)),
        Subcommand::Trace => rt.block_on(trace::run_main(&rest)),
        Subcommand::Migrate => rt.block_on(migrate::run_main(&rest)),
    };

    match res {
        Ok(code) => code,
        Err(CliError::Args(msg)) => {
            eprintln!("edge-cli: {msg}");
            2
        }
        Err(CliError::Snapshot(e)) => {
            eprintln!("edge-cli: snapshot error: {e}");
            1
        }
        Err(CliError::Wasmtime(e)) => {
            eprintln!("edge-cli: wasmtime error: {e}");
            1
        }
        Err(CliError::Io(e)) => {
            eprintln!("edge-cli: io error: {e}");
            1
        }
        Err(CliError::Bench(msg)) => {
            eprintln!("edge-cli: bench: {msg}");
            1
        }
        Err(CliError::MissingSubcommand) | Err(CliError::Unknown(_)) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> i32 {
        run_main_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_returns_2_and_prints_help() {
        assert_eq!(run(&[]), 2);
    }

    #[test]
    fn help_long_returns_0() {
        assert_eq!(run(&["--help"]), 0);
    }

    #[test]
    fn help_short_returns_0() {
        assert_eq!(run(&["-h"]), 0);
    }

    #[test]
    fn version_long_returns_0() {
        assert_eq!(run(&["--version"]), 0);
    }

    #[test]
    fn version_short_returns_0() {
        assert_eq!(run(&["-V"]), 0);
    }

    #[test]
    fn unknown_subcommand_returns_2() {
        assert_eq!(run(&["frobnicate"]), 2);
    }

    #[test]
    fn freeze_stub_returns_args_error_which_maps_to_2() {
        // Freeze is wired as a stub in D3.3. The subcommand returns
        // CliError::Args("not yet implemented"), which the dispatcher
        // maps to exit 2.
        assert_eq!(run(&["freeze"]), 2);
    }

    #[test]
    fn serve_stub_returns_2() {
        assert_eq!(run(&["serve"]), 2);
    }

    #[test]
    fn bench_stub_returns_2() {
        assert_eq!(run(&["bench"]), 2);
    }

    #[test]
    fn trace_with_no_args_returns_2() {
        // The trace subcommand itself rejects empty positional args with
        // CliError::Args("usage: ..."), which maps to exit 2.
        assert_eq!(run(&["trace"]), 2);
    }
}
