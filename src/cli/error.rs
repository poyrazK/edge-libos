//! `edge-cli` error type.
//!
//! P2-D3.3: every subcommand module under `src/cli/{run,freeze,serve,bench,trace}.rs`
//! returns `CliResult<T>`. The top-level dispatcher in `src/cli/mod.rs` maps
//! each variant to a host exit code:
//!
//! - `Args` / `MissingSubcommand` / `Unknown` → exit code `2` (usage error)
//! - `Snapshot` / `Wasmtime` / `Io`           → exit code `1` (runtime error)
//!
//! This mirrors the convention the deleted `edge-python` binary used
//! (exit 2 on usage, exit 1 on runtime). Subcommands are responsible for
//! surfacing their own richer diagnostics via the `eprintln!` channel
//! when needed.
//!
//! ## Why hand-written (no `thiserror`)
//!
//! P2-D3.3 deliberately avoids adding a direct `thiserror` dep — it's
//! only available transitively in the lockfile. The enum is small
//! enough that hand-writing `Display` + `Error` impls is ~30 LOC and
//! keeps `Cargo.toml` flat. If `cli::error` grows past ~6 variants or
//! the display strings become parametric, switching to `thiserror`
//! becomes worth a direct dep.

use std::fmt;

use crate::snapshot::SnapshotError;

/// The error every `edge-cli` subcommand can return.
///
/// Wraps the common error categories a subcommand is likely to surface
/// (bad CLI args, snapshot decoding errors, wasmtime instantiation
/// errors, host I/O). `From<...>` impls make `?` ergonomic inside
/// subcommand bodies.
#[derive(Debug)]
pub enum CliError {
    /// Usage / argument parse error. Surfaces via `eprintln!` and exits 2.
    Args(String),

    /// Internal: the top-level dispatcher found zero positional args.
    /// The dispatcher handles this case directly (prints help) so this
    /// variant is mostly defensive.
    MissingSubcommand,

    /// Internal: `Subcommand::from_str` failed. The dispatcher turns
    /// this into "unknown subcommand" + exit 2.
    Unknown(String),

    /// Snapshot decode / IO failure (e.g. corrupt postcard header,
    /// missing version byte).
    Snapshot(SnapshotError),

    /// Wasmtime instantiation / engine failure.
    Wasmtime(wasmtime::Error),

    /// Host std I/O failure.
    Io(std::io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Args(msg) => write!(f, "cli: {msg}"),
            CliError::MissingSubcommand => {
                write!(f, "cli: no subcommand given; try `edge-cli --help`")
            }
            CliError::Unknown(s) => {
                write!(f, "cli: unknown subcommand `{s}`; try `edge-cli --help`")
            }
            CliError::Snapshot(e) => write!(f, "snapshot: {e}"),
            CliError::Wasmtime(e) => write!(f, "wasmtime: {e}"),
            CliError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::Args(_) | CliError::MissingSubcommand | CliError::Unknown(_) => None,
            CliError::Snapshot(e) => Some(e),
            // `wasmtime::Error` does not impl `std::error::Error` in
            // wasmtime 45.0.3 — chain it through `Display` instead.
            CliError::Wasmtime(_) => None,
            CliError::Io(e) => Some(e),
        }
    }
}

impl From<SnapshotError> for CliError {
    fn from(e: SnapshotError) -> Self {
        CliError::Snapshot(e)
    }
}

impl From<wasmtime::Error> for CliError {
    fn from(e: wasmtime::Error) -> Self {
        CliError::Wasmtime(e)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Io(e)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        // Best-effort downcast: if the anyhow error wraps one of our
        // mapped types, route it through that variant so the dispatcher's
        // per-variant exit code mapping still works. Otherwise fall back
        // to a generic `Args` (which the dispatcher turns into exit 2);
        // subcommand authors should prefer the explicit `CliError::*`
        // variants for predictable exit codes.
        if let Some(s) = e.downcast_ref::<SnapshotError>() {
            // SAFETY-ish: we only read the inner error type's Display impl.
            return CliError::Args(format!("anyhow: snapshot: {s}"));
        }
        CliError::Args(format!("anyhow: {e}"))
    }
}

/// Convenience: every subcommand returns `Result<T, CliError>`.
pub type CliResult<T> = std::result::Result<T, CliError>;
