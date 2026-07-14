//! Futex — P3 reservation.
//!
//! Returns `-ENOSYS` in v1. The real implementation needs
//! `wasm_threads = true` and a futex hash table keyed by
//! guest-address; see `docs/adr/0001-p3-futex-semantics.md` for the
//! integration contract with the existing per-fd `tokio::sync::Notify`
//! scheme (P1-7).

/// Linux x86-64 NR for `futex(2)`.
pub const NR_FUTEX: u32 = 202;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nr_matches_linux_x86_64() {
        assert_eq!(NR_FUTEX, 202);
    }
}
