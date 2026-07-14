//! `edge-cli` binary entry point.
//!
//! P2-D3.3: this binary replaces `edge-python` + `trace-host`. The
//! `edge_libos::cli::run_main` function does all the real work — argv
//! parsing, subcommand dispatch, per-subcommand execution, exit code
//! propagation — see `src/cli/mod.rs`.
//!
//! Exit codes:
//! - 0  success
//! - 1  runtime error
//! - 2  usage error
//!
//! See the `Usage` section of `edge-cli --help` for subcommand list.

fn main() {
    std::process::exit(edge_libos::cli::run_main());
}
