//! P2-D3.1 — postcard façade for `KernelSnapshot`.
//!
//! All callers (D3.5 freeze / D3.7 bench) go through these four
//! functions so the error mapping lives in exactly one place. The
//! wire format is owned by the `KernelSnapshot` derive + the
//! `snapshot::endian` newtypes; this module adds nothing to it.
//!
//! Error precedence for [`read_snapshot_file`]: `MissingPath` (file
//! absent) → `IoError` (other I/O) → `Postcard` (decode failure).

use std::path::Path;

use crate::snapshot::{KernelSnapshot, SnapshotError};

/// Serialize a snapshot to a `Vec<u8>` via `postcard::to_stdvec`.
pub fn encode_snapshot(snap: &KernelSnapshot) -> Result<Vec<u8>, SnapshotError> {
    postcard::to_stdvec(snap).map_err(|e| SnapshotError::Postcard(e.to_string()))
}

/// Deserialize a byte slice produced by [`encode_snapshot`].
pub fn decode_snapshot(bytes: &[u8]) -> Result<KernelSnapshot, SnapshotError> {
    postcard::from_bytes(bytes).map_err(|e| SnapshotError::Postcard(e.to_string()))
}

/// Encode `snap` and write the bytes to `path` (creating or truncating).
pub fn write_snapshot_file(path: &Path, snap: &KernelSnapshot) -> Result<(), SnapshotError> {
    let bytes = encode_snapshot(snap)?;
    std::fs::write(path, &bytes)
        .map_err(|e| SnapshotError::IoError(e, format!("write snapshot to {}", path.display())))
}

/// Read `path` and decode it back into a `KernelSnapshot`.
pub fn read_snapshot_file(path: &Path) -> Result<KernelSnapshot, SnapshotError> {
    if !path.exists() {
        return Err(SnapshotError::MissingPath(path.display().to_string()));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| SnapshotError::IoError(e, format!("read snapshot from {}", path.display())))?;
    decode_snapshot(&bytes)
}

#[cfg(test)]
mod tests {
    use tempfile::{tempdir, NamedTempFile};

    use super::*;
    use crate::snapshot::endian::LeU32;
    use crate::snapshot::{
        ClockStateSnapshot, FdSnapshot, KernelSnapshot, LinearAllocatorSnapshot,
        SignalStateSnapshot, VfsSnapshot, SNAPSHOT_FORMAT_VERSION,
    };

    fn fixture_snapshot() -> KernelSnapshot {
        KernelSnapshot {
            format_version: LeU32(SNAPSHOT_FORMAT_VERSION),
            pages: vec![],
            fds: FdSnapshot::default(),
            mm: LinearAllocatorSnapshot::default(),
            vfs: VfsSnapshot {
                root: "/".into(),
                cwd: "/".into(),
            },
            clock: ClockStateSnapshot::default(),
            brk: LeU32(0),
            args: vec!["a".to_string()],
            env: vec![("K".to_string(), "V".to_string())],
            rng_seed: [7u8; 32],
            signals: SignalStateSnapshot::default(),
            exit_code: None,
            comm: [0u8; 16],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let snap = fixture_snapshot();
        let bytes = encode_snapshot(&snap).expect("encode");
        let back = decode_snapshot(&bytes).expect("decode");
        // `KernelSnapshot` deliberately does not derive `PartialEq` (see
        // `src/snapshot.rs:952`), so compare field-by-field.
        assert_eq!(back.format_version, snap.format_version);
        assert!(back.pages.is_empty());
        assert_eq!(back.args, snap.args);
        assert_eq!(back.env, snap.env);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.brk, snap.brk);
        assert_eq!(back.vfs.root, snap.vfs.root);
        assert_eq!(back.vfs.cwd, snap.vfs.cwd);
        assert_eq!(back.exit_code, snap.exit_code);
        assert_eq!(back.comm, snap.comm);
    }

    #[test]
    fn write_read_file_roundtrip() {
        let snap = fixture_snapshot();
        let tmp = NamedTempFile::new().expect("NamedTempFile::new");
        let path = tmp.path().to_path_buf();
        write_snapshot_file(&path, &snap).expect("write_snapshot_file");
        let back = read_snapshot_file(&path).expect("read_snapshot_file");
        assert_eq!(back.format_version, snap.format_version);
        assert_eq!(back.args, snap.args);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.vfs.root, snap.vfs.root);
        // `tmp` is dropped at end of fn; the OS removes the file.
    }

    #[test]
    fn read_missing_file_returns_missing_path() {
        // `tempdir` + a guaranteed-absent join — unambiguous on every OS.
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("does_not_exist.snap");
        let err = read_snapshot_file(&missing).expect_err("must fail");
        match err {
            SnapshotError::MissingPath(p) => {
                assert!(
                    p.ends_with("does_not_exist.snap"),
                    "path should be preserved in error: {p}"
                );
            }
            other => panic!("expected SnapshotError::MissingPath, got {other:?}"),
        }
    }

    #[test]
    fn decode_truncated_returns_postcard_error() {
        let snap = fixture_snapshot();
        let bytes = encode_snapshot(&snap).expect("encode");
        // Truncate to the first 4 bytes — partial stream for any snapshot.
        let truncated = &bytes[..bytes.len().min(4)];
        let err = decode_snapshot(truncated).expect_err("truncated decode must fail");
        match err {
            SnapshotError::Postcard(msg) => {
                assert!(!msg.is_empty(), "postcard error message must be populated");
            }
            other => panic!("expected SnapshotError::Postcard, got {other:?}"),
        }
    }
}
