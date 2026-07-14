//! P2-D2 / ADR 0002 §2 — explicit little-endian wrappers for every
//! multi-byte integer in the snapshot wire format.
//!
//! `postcard`'s default `Serializer::serialize_u32` writes a **varint**
//! (not fixed-width LE). For snapshot stability across versions and
//! across host endianness, the newtypes in this module opt the inner
//! integer field into fixed-width little-endian encoding via
//! [`postcard::fixint::le`].
//!
//! ## Wire format
//!
//! `LeU32(0x12345678)` encodes as the four bytes `[0x78, 0x56, 0x34, 0x12]`.
//! `LeI32(-1)` encodes as `[0xFF, 0xFF, 0xFF, 0xFF]`. No varint length
//! prefix. Verified by `le_newtype_is_transparent_in_struct` test.
//!
//! ## Why newtypes instead of bare field-level adapters
//!
//! A bare `#[serde(with = "postcard::fixint::le")] x: u32` works on one
//! field but does not propagate cleanly into `Option<LeU32>`,
//! `Vec<LeU32>`, `BTreeMap<LeU32, V>`, etc. The newtype form keeps the
//! ADR-compliant encoding visible in the type system — every reviewer
//! sees `LeU32` in the field declaration and knows it is fixed-width LE,
//! without having to chase the `with = ...` adapter on each call site.

use serde::{Deserialize, Serialize};

/// Fixed-width little-endian `u32`. Wire form: 4 bytes, host-independent.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LeU32(#[serde(with = "postcard::fixint::le")] pub u32);

/// Fixed-width little-endian `u64`. Wire form: 8 bytes, host-independent.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LeU64(#[serde(with = "postcard::fixint::le")] pub u64);

/// Fixed-width little-endian `i32` (two's-complement). Wire form: 4 bytes.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LeI32(#[serde(with = "postcard::fixint::le")] pub i32);

/// Fixed-width little-endian `i64` (two's-complement). Wire form: 8 bytes.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LeI64(#[serde(with = "postcard::fixint::le")] pub i64);

impl From<u32> for LeU32 {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<LeU32> for u32 {
    fn from(v: LeU32) -> Self {
        v.0
    }
}

impl From<u64> for LeU64 {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<LeU64> for u64 {
    fn from(v: LeU64) -> Self {
        v.0
    }
}

impl From<i32> for LeI32 {
    fn from(v: i32) -> Self {
        Self(v)
    }
}

impl From<LeI32> for i32 {
    fn from(v: LeI32) -> Self {
        v.0
    }
}

impl From<i64> for LeI64 {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

impl From<LeI64> for i64 {
    fn from(v: LeI64) -> Self {
        v.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes `LeU32(0x12345678)` and asserts the wire form is the four
    /// little-endian bytes with NO varint length prefix. If this ever
    /// regresses to a varint path, the byte length jumps and the
    /// assertion fails.
    #[test]
    fn le_u32_roundtrip_via_postcard() {
        let original = LeU32(0x1234_5678);
        let bytes = postcard::to_stdvec(&original).expect("encode LeU32");
        assert_eq!(
            bytes,
            vec![0x78, 0x56, 0x34, 0x12],
            "LeU32 must encode as 4 fixed-width LE bytes, no length prefix"
        );
        assert_eq!(bytes.len(), 4, "exactly 4 bytes on the wire");
        let back: LeU32 = postcard::from_bytes(&bytes).expect("decode LeU32");
        assert_eq!(back, LeU32(0x1234_5678));
    }

    #[test]
    fn le_u64_roundtrip_via_postcard() {
        let original = LeU64(0x0123_4567_89AB_CDEF);
        let bytes = postcard::to_stdvec(&original).expect("encode LeU64");
        assert_eq!(
            bytes,
            vec![0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01],
            "LeU64 must encode as 8 fixed-width LE bytes"
        );
        assert_eq!(bytes.len(), 8);
        let back: LeU64 = postcard::from_bytes(&bytes).expect("decode LeU64");
        assert_eq!(back, LeU64(0x0123_4567_89AB_CDEF));
    }

    /// `LeI32(-1)` is `0xFFFFFFFF` in two's complement. Wire form: four
    /// `0xFF` bytes, no length prefix.
    #[test]
    fn le_i32_roundtrip_negative() {
        let original = LeI32(-1);
        let bytes = postcard::to_stdvec(&original).expect("encode LeI32(-1)");
        assert_eq!(bytes, vec![0xFF, 0xFF, 0xFF, 0xFF]);
        let back: LeI32 = postcard::from_bytes(&bytes).expect("decode LeI32");
        assert_eq!(back, LeI32(-1));
    }

    /// When `LeU32` appears as a struct field, the wire form is still
    /// exactly 4 bytes — proving `#[serde(transparent)]` does not add
    /// any framing.
    #[test]
    fn le_newtype_is_transparent_in_struct() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            x: LeU32,
            y: LeI64,
        }
        let w = W {
            x: LeU32(0xDEAD_BEEF),
            y: LeI64(-2),
        };
        let bytes = postcard::to_stdvec(&w).expect("encode struct");
        // 4 bytes for x + 8 bytes for y = 12 bytes total.
        assert_eq!(
            bytes.len(),
            12,
            "struct with LeU32 + LeI64 must encode as 12 fixed-width bytes, got {} bytes: {bytes:?}",
            bytes.len()
        );
        // x = 0xDEADBEEF, LE: [0xEF, 0xBE, 0xAD, 0xDE]
        assert_eq!(&bytes[0..4], &[0xEF, 0xBE, 0xAD, 0xDE]);
        // y = -2 = 0xFFFF_FFFF_FFFF_FFFE, LE: [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        assert_eq!(
            &bytes[4..12],
            &[0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        let back: W = postcard::from_bytes(&bytes).expect("decode struct");
        assert_eq!(back, w);
    }
}