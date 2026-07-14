//! `edge-cli bench <wasm> --iters <n>` — STUB.
//!
//! Lands in D3.7. For now returns `CliError::Args` so a `--help`-aware
//! caller can distinguish "you used the wrong shape" from "this
//! subcommand doesn't exist yet".

use crate::cli::error::{CliError, CliResult};

pub async fn run_main(_args: &[String]) -> CliResult<i32> {
    Err(CliError::Args(
        "edge-cli bench: not yet implemented (lands in D3.7)".to_string(),
    ))
}
