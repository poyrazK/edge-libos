//! `trace-host` — syscall tracer + JSON dumper.
//!
//! Full implementation lands in Step 22. This stub exists so the manifest
//! has its second binary target and `cargo check` resolves.

use anyhow::Result;

fn main() -> Result<()> {
    eprintln!("trace-host: stub driver (not yet implemented)");
    eprintln!("  full tracer lands in Step 22 (syscall JSON dump + --diff mode).");
    std::process::exit(2);
}
