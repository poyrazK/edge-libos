//! `edge-python` — P0 DoD driver.
//!
//! Loads a CPython wasm32-musl guest and runs a script. Full implementation
//! lands in Step 19+ of the build order. This stub exists so `cargo check`
//! has a binary target to compile.

use anyhow::Result;

fn main() -> Result<()> {
    let wasm_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: edge-python <python.wasm> [script args...]"))?;
    eprintln!("edge-python: stub driver (not yet implemented)");
    eprintln!("  would load: {wasm_path}");
    eprintln!(
        "  this binary is a placeholder for the P0 build order; the real driver"
    );
    eprintln!("  is wired in once the guest build + bin entry land (Step 19+).");
    std::process::exit(2);
}
