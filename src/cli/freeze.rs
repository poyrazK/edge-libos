//! `edge-cli freeze <wasm> --out <path>` — STUB.
//!
//! Lands in D3.5. For now returns `CliError::Args` so a `--help`-aware
//! caller can distinguish "you used the wrong shape" from "this
//! subcommand doesn't exist yet".

use crate::cli::error::{CliError, CliResult};

pub async fn run_main(_args: &[String]) -> CliResult<i32> {
    Err(CliError::Args(
        "edge-cli freeze: not yet implemented (lands in D3.5)".to_string(),
    ))
}
