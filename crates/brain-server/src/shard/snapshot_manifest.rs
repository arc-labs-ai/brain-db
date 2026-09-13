//! The `manifest.json` written into every snapshot bundle.
//!
//! The manifest is the snapshot's index of record: it names every
//! bundle file, carries a BLAKE3 digest of each so restore can verify
//! integrity before swapping files in, and records the `snapshot_lsn`
//! (the durable LSN the bundle is consistent with) plus the
//! `shard_uuid` so restore can refuse a cross-shard mismatch.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Filename of the manifest inside a snapshot directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// One bundle file's size and BLAKE3 digest. The digest is the
/// lowercase hex of the 32-byte BLAKE3 hash so the manifest is
/// human-readable and stable across serde versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDigest {
    pub size: u64,
    pub blake3: String,
}

/// The full snapshot manifest.
///
/// `files` keys are paths *relative to the snapshot directory* — e.g.
/// `arena.bin`, `metadata.redb`, `wal/0000000003.wal`. Restore walks
/// this map, re-hashes each file, and rejects the bundle on any
/// mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// The durable LSN this snapshot is consistent with. Recovery
    /// replays the bundled WAL up to (and including) this LSN.
    pub snapshot_lsn: u64,
    /// The checkpoint id that produced this snapshot. Doubles as the
    /// snapshot's directory-name id.
    pub checkpoint_id: u64,
    /// Owning shard's UUID, lowercase hex. Restore requires this to
    /// match the target shard — a snapshot restores only onto the same
    /// shard it was taken from.
    pub shard_uuid: String,
    /// Wall-clock instant the snapshot was taken, in unix nanoseconds.
    pub taken_at_unix_nanos: u64,
    /// Per-bundle-file size + BLAKE3 digest, keyed by path relative to
    /// the snapshot directory.
    pub files: BTreeMap<String, FileDigest>,
}

impl SnapshotManifest {
    /// Serialize to pretty JSON and write to `path`.
    ///
    /// # Errors
    ///
    /// Propagates the filesystem write error, or a serialization error
    /// surfaced as `io::ErrorKind::InvalidData`.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Read and parse a `manifest.json`.
    ///
    /// # Errors
    ///
    /// Propagates the filesystem read error, or a parse error surfaced
    /// as `io::ErrorKind::InvalidData`.
    pub fn read_from(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Stream BLAKE3 over `path` and return the lowercase-hex digest.
///
/// # Errors
///
/// Propagates the underlying read error.
pub fn blake3_hex(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_via_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE);
        let mut files = BTreeMap::new();
        files.insert(
            "arena.bin".to_string(),
            FileDigest {
                size: 12_800,
                blake3: "ab".repeat(32),
            },
        );
        files.insert(
            "wal/0000000000.wal".to_string(),
            FileDigest {
                size: 4096,
                blake3: "cd".repeat(32),
            },
        );
        let m = SnapshotManifest {
            snapshot_lsn: 42,
            checkpoint_id: 7,
            shard_uuid: "ff".repeat(16),
            taken_at_unix_nanos: 1_700_000_000_000_000_000,
            files,
        };

        m.write_to(&path).expect("write_to");
        let back = SnapshotManifest::read_from(&path).expect("read_from");
        assert_eq!(m, back);
    }

    #[test]
    fn blake3_hex_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data.bin");
        std::fs::write(&f, b"abc").unwrap();
        // BLAKE3("abc") — official test vector.
        let expected = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
        assert_eq!(blake3_hex(&f).unwrap(), expected);
    }
}
