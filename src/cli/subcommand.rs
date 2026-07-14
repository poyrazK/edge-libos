//! `edge-cli` subcommand enum.
//!
//! P2-D3.3: every top-level invocation is one of these five subcommands.
//! The mapping from a positional argv string → `Subcommand` lives in
//! `FromStr for Subcommand`; everything else (parsing per-subcommand
//! flags, the tokio runtime, the actual work) is in `src/cli/mod.rs`
//! and the per-subcommand modules.
//!
//! Per-subcommand status:
//!
//! - `Run`    — wired (migrated from `src/bin/edge_python.rs`, D0-era).
//! - `Trace`  — wired (migrated from `src/bin/trace_host.rs`, P2-A2).
//!   Preserves JSON-line protocol + `--diff` / `--no-marker`
//!   semantics so `tests/conformance/runner.sh` keeps working.
//! - `Freeze` — stub, lands in D3.5.
//! - `Serve`  — stub, lands in D3.5.
//! - `Bench`  — stub, lands in D3.7.

use std::str::FromStr;

use crate::cli::error::{CliError, CliResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    /// `edge-cli run <wasm> [--] [args...]` — one-shot guest execution.
    Run,
    /// `edge-cli freeze <wasm> --out <path>` — snapshot a live kernel.
    Freeze,
    /// `edge-cli serve <snap> [--port <p>]` — restore a snapshot + serve.
    Serve,
    /// `edge-cli bench <wasm> --iters <n>` — measure cold-start latency.
    Bench,
    /// `edge-cli trace <wasm> [--diff <baseline>] [--no-marker]`
    /// — JSON-line syscall tracer. Used by `tests/conformance/runner.sh`.
    Trace,
}

impl FromStr for Subcommand {
    type Err = CliError;

    fn from_str(s: &str) -> CliResult<Self> {
        Ok(match s {
            "run" => Subcommand::Run,
            "freeze" => Subcommand::Freeze,
            "serve" => Subcommand::Serve,
            "bench" => Subcommand::Bench,
            "trace" => Subcommand::Trace,
            other => return Err(CliError::Unknown(other.to_string())),
        })
    }
}
