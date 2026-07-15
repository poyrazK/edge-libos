//! Per-request CPU metering — `fuel` as the budget primitive.
//!
//! ADR 0003 §3: Wasmtime 45.0.3 ships two metering primitives
//! (`consume_fuel` and `epoch_interruption`); the ADR picks fuel
//! because the wasmtime docs explicitly recommend it for
//! "deterministic interruption of a fixed, finite interval"
//! (`wasmtime-45.0.3/src/runtime/store.rs:1158`). This module is
//! the single source of truth for fuel constants and conversions.
//!
//! Every subcommand (`run`, `serve`, `bench`) reaches the fuel
//! knob through `ms_to_fuel()` rather than open-coding the
//! conversion, so a future retune of `FUEL_PER_MS` ripples through
//! automatically.

/// Provisional fuel-per-millisecond constant. **MUST be overwritten
/// by the M6 calibration commit** — see ADR 0003 §3.
///
/// 10⁶ fuel/ms ≈ 1 µs/instruction on x86_64 for a typical mix of
/// arithmetic + control-flow instructions. Empirically tuned by a
/// WAT fixture that busy-loops N iterations and reads the consumed
/// fuel; the calibration writes the real number here.
pub const FUEL_PER_MS: u64 = 1_000_000;

/// How often (in fuel units) a long-running wasm call yields back
/// to the host tokio runtime. `Store::fuel_async_yield_interval`
/// requires `consume_fuel(true)` to be effective. `u64::MAX`
/// disables yielding — the runtime never cooperatively yields in
/// the middle of a wasm call. This is the v1 default; a follow-on
/// commit may lower the constant once the host-side fiber re-entry
/// safety contract (the write handler in `src/sys/file.rs` is the
/// load-bearing case) is audited and host handlers are made
/// yield-safe. See `tests/snapshot_roundtrip.rs` for the regression
/// test that this constant's calibration must not break.
pub const YIELD_INTERVAL_FUEL: u64 = u64::MAX;

/// Convert a wall-clock-millisecond budget to fuel units. Saturates
/// at `u64::MAX` rather than overflowing on absurd inputs; the
/// caller (the CLI argv parser) is expected to have rejected
/// zero/nonsense values before this is reached, but the saturation
/// guard prevents a panic on a pathologically large budget.
///
/// `u64::MAX` is also the "unbounded" sentinel: `run` and `bench`
/// default to it when `--cpu-budget-ms` is omitted.
pub fn ms_to_fuel(ms: u64) -> u64 {
    if ms == u64::MAX {
        return u64::MAX;
    }
    ms.saturating_mul(FUEL_PER_MS)
}

/// Convert consumed fuel back to a wall-clock-millisecond estimate.
/// Inverse of [`ms_to_fuel`], rounded down. Saturates at `u64::MAX`
/// (caller is expected to format via `Display`, which handles it).
pub fn fuel_to_ms(fuel: u64) -> u64 {
    if fuel == u64::MAX {
        return u64::MAX;
    }
    fuel / FUEL_PER_MS
}

/// True if the given wasmtime error is the `OutOfFuel` trap.
///
/// Wasmtime 45.0.3 does not expose the `Trap` enum through
/// `wasmtime::Error`'s public API; the only reliable classifier is
/// the `Display` string. The constant on the right hand side is the
/// exact phrasing emitted by `wasmtime-environ-26.0.1::trap_encoding`
/// (`OutOfFuel => "all fuel consumed by WebAssembly"`). Pinned here
/// so a future wasmtime rename turns into a single test failure.
pub fn is_out_of_fuel(err: &wasmtime::Error) -> bool {
    err.to_string().contains("all fuel consumed by WebAssembly")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_to_fuel_round_trip() {
        assert_eq!(fuel_to_ms(ms_to_fuel(50)), 50);
        assert_eq!(fuel_to_ms(ms_to_fuel(1)), 1);
        assert_eq!(fuel_to_ms(ms_to_fuel(0)), 0);
    }

    #[test]
    fn ms_to_fuel_saturates_on_max() {
        assert_eq!(ms_to_fuel(u64::MAX), u64::MAX);
    }

    #[test]
    fn fuel_to_ms_max_input_returns_max() {
        assert_eq!(fuel_to_ms(u64::MAX), u64::MAX);
    }

    #[test]
    fn provisional_constant_is_one_million() {
        // Sentinel: the M6 calibration commit must delete or replace
        // this test (it is intentionally a no-op after calibration).
        assert_eq!(FUEL_PER_MS, 1_000_000);
    }
}