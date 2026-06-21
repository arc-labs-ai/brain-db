//! Snapshot restore: verify a bundle, then place its files so the next
//! shard spawn recovers to the snapshot LSN.
//!
//! Brain restores by *placing files, then running normal recovery* — it
//! does not hot-swap a live shard's mmap or open redb file. The caller
//! is responsible for stopping the shard (dropping its
//! [`crate::shard::ShardHandle`] and joining the
//! [`crate::shard::ShardJoiner`]) **before** calling
//! [`restore_snapshot`], and for re-spawning it
//! ([`crate::shard::spawn_shard`]) **after**. The existing WAL-recovery
//! path then replays the bundled WAL up to `snapshot_lsn` and rebuilds
//! the HNSW from the restored arena.
//!
//! Restore is same-shard only: the bundle's `shard_uuid` must equal the
//! target shard's UUID (spec §08/06 §7). Every bundle file is BLAKE3-
//! verified against the manifest before anything is overwritten, so a
//! corrupt bundle is rejected before the live data dir is touched.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use brain_storage::ShardPaths;

use crate::shard::snapshot_manifest::{blake3_hex, SnapshotManifest, MANIFEST_FILE};

/// Outcome of a successful restore.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// The LSN the restored data is consistent with. After re-spawn,
    /// recovery replays the bundled WAL up to this LSN.
    pub snapshot_lsn: u64,
    /// The snapshot's checkpoint id (its directory-name id).
    pub checkpoint_id: u64,
    /// Number of WAL segment files placed into the target `wal/` dir.
    pub wal_segments_placed: usize,
}

/// Errors restore can surface. All are pre-swap except [`Self::Place`],
/// which can leave the data dir partially overwritten — the caller must
/// treat a `Place` error as "shard data dir is now inconsistent, do not
/// re-spawn until repaired".
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// The snapshot directory or its manifest is missing / unreadable.
    #[error("manifest unreadable at {path}: {source}")]
    Manifest {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The bundle was taken from a different shard than the target.
    #[error("shard_uuid mismatch: bundle is {bundle}, target is {target}")]
    ShardUuidMismatch { bundle: String, target: String },

    /// A bundle file is missing or its size/BLAKE3 doesn't match the
    /// manifest. The data dir has NOT been modified.
    #[error("integrity check failed for {file}: {detail}")]
    Integrity { file: String, detail: String },

    /// A filesystem error while placing files into the data dir. The
    /// data dir may be partially overwritten.
    #[error("placing {file}: {source}")]
    Place {
        file: String,
        source: std::io::Error,
    },
}

/// Verify a snapshot bundle's integrity without touching the target.
///
/// Reads the manifest, confirms the `shard_uuid` matches, then re-hashes
/// every bundle file and compares size + BLAKE3 against the manifest.
/// Returns the parsed manifest on success.
///
/// # Errors
///
/// [`RestoreError::Manifest`], [`RestoreError::ShardUuidMismatch`], or
/// [`RestoreError::Integrity`].
pub fn verify_snapshot(
    snapshot_dir: &Path,
    target_shard_uuid: [u8; 16],
) -> Result<SnapshotManifest, RestoreError> {
    let manifest_path = snapshot_dir.join(MANIFEST_FILE);
    let manifest =
        SnapshotManifest::read_from(&manifest_path).map_err(|source| RestoreError::Manifest {
            path: manifest_path.clone(),
            source,
        })?;

    let target_hex = hex_lower(&target_shard_uuid);
    if manifest.shard_uuid != target_hex {
        return Err(RestoreError::ShardUuidMismatch {
            bundle: manifest.shard_uuid.clone(),
            target: target_hex,
        });
    }

    for (rel, digest) in &manifest.files {
        let path = snapshot_dir.join(rel);
        let meta = std::fs::metadata(&path).map_err(|e| RestoreError::Integrity {
            file: rel.clone(),
            detail: format!("stat failed: {e}"),
        })?;
        if meta.len() != digest.size {
            return Err(RestoreError::Integrity {
                file: rel.clone(),
                detail: format!("size {} != manifest {}", meta.len(), digest.size),
            });
        }
        let actual = blake3_hex(&path).map_err(|e| RestoreError::Integrity {
            file: rel.clone(),
            detail: format!("hash failed: {e}"),
        })?;
        if actual != digest.blake3 {
            return Err(RestoreError::Integrity {
                file: rel.clone(),
                detail: format!("blake3 {actual} != manifest {}", digest.blake3),
            });
        }
    }

    Ok(manifest)
}

/// Restore `snapshot_dir` into the shard data directory rooted at
/// `target_root`.
///
/// **The shard must be stopped before this is called and re-spawned
/// after** — see the module docs. The function:
///
/// 1. Verifies the bundle ([`verify_snapshot`]) — UUID match + BLAKE3 of
///    every file. Nothing is overwritten if verification fails.
/// 2. Removes the target's existing `wal/` segments so a stale tail
///    can't shadow the restored one, then re-creates an empty `wal/`.
/// 3. Reflink-copies `arena.bin`, `metadata.redb`, and each bundled WAL
///    segment into their `ShardPaths` locations.
///
/// HNSW is intentionally not placed — it's rebuilt on the next spawn
/// from the restored arena.
///
/// # Errors
///
/// Any [`RestoreError`]. A [`RestoreError::Place`] means the data dir
/// may be partially written; do not re-spawn until repaired.
pub fn restore_snapshot(
    snapshot_dir: &Path,
    target_root: &Path,
    target_shard_uuid: [u8; 16],
) -> Result<RestoreReport, RestoreError> {
    let manifest = verify_snapshot(snapshot_dir, target_shard_uuid)?;

    let paths = ShardPaths::at(target_root);
    let target_wal_dir = paths.wal_dir();

    // Clear the existing WAL tail. The bundled WAL is authoritative for
    // the snapshot LSN; leaving stale segments would let recovery replay
    // post-snapshot records (or fail the contiguity check on mixed
    // segment_seq ranges).
    if target_wal_dir.exists() {
        for entry in std::fs::read_dir(&target_wal_dir).map_err(|source| RestoreError::Place {
            file: "wal/".to_string(),
            source,
        })? {
            let entry = entry.map_err(|source| RestoreError::Place {
                file: "wal/".to_string(),
                source,
            })?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wal") {
                std::fs::remove_file(&p).map_err(|source| RestoreError::Place {
                    file: format!("rm {}", p.display()),
                    source,
                })?;
            }
        }
    }
    std::fs::create_dir_all(&target_wal_dir).map_err(|source| RestoreError::Place {
        file: "wal/".to_string(),
        source,
    })?;

    let mut wal_segments_placed = 0usize;
    for rel in manifest.files.keys() {
        let src = snapshot_dir.join(rel);
        let dst = if rel == "arena.bin" {
            paths.arena()
        } else if rel == "metadata.redb" {
            paths.metadata_db()
        } else if let Some(seg) = rel.strip_prefix("wal/") {
            wal_segments_placed += 1;
            target_wal_dir.join(seg)
        } else {
            // Unknown bundle entry — skip rather than place it somewhere
            // arbitrary. Manifests only carry the three known kinds.
            continue;
        };
        brain_storage::reflink_or_copy(&src, &dst).map_err(|source| RestoreError::Place {
            file: rel.clone(),
            source,
        })?;
    }

    // Place shard.uuid if the bundle carries it. Restore is same-UUID
    // only (verified above), so the target's existing shard.uuid already
    // matches; this is belt-and-suspenders for bundles copied to a
    // freshly-laid-out data dir.
    let bundle_uuid = snapshot_dir.join(brain_storage::layout::SHARD_UUID_FILE);
    if bundle_uuid.exists() {
        brain_storage::reflink_or_copy(&bundle_uuid, &paths.shard_uuid()).map_err(|source| {
            RestoreError::Place {
                file: brain_storage::layout::SHARD_UUID_FILE.to_string(),
                source,
            }
        })?;
    }

    Ok(RestoreReport {
        snapshot_lsn: manifest.snapshot_lsn,
        checkpoint_id: manifest.checkpoint_id,
        wal_segments_placed,
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::shard::snapshot_manifest::FileDigest;

    fn write_bundle(dir: &Path, uuid: [u8; 16]) -> SnapshotManifest {
        std::fs::create_dir_all(dir.join("wal")).unwrap();
        std::fs::write(dir.join("arena.bin"), b"arena-bytes").unwrap();
        std::fs::write(dir.join("metadata.redb"), b"redb-bytes").unwrap();
        std::fs::write(dir.join("wal/0000000000.wal"), b"wal-seg-0").unwrap();

        let mut files = BTreeMap::new();
        for rel in ["arena.bin", "metadata.redb", "wal/0000000000.wal"] {
            let p = dir.join(rel);
            files.insert(
                rel.to_string(),
                FileDigest {
                    size: std::fs::metadata(&p).unwrap().len(),
                    blake3: blake3_hex(&p).unwrap(),
                },
            );
        }
        let m = SnapshotManifest {
            snapshot_lsn: 123,
            checkpoint_id: 4,
            shard_uuid: hex_lower(&uuid),
            taken_at_unix_nanos: 99,
            files,
        };
        m.write_to(&dir.join(MANIFEST_FILE)).unwrap();
        m
    }

    #[test]
    fn verify_accepts_clean_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        write_bundle(&snap, uuid);
        verify_snapshot(&snap, uuid).expect("clean bundle verifies");
    }

    #[test]
    fn verify_rejects_uuid_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        write_bundle(&snap, uuid);
        let other: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        let err = verify_snapshot(&snap, other).unwrap_err();
        assert!(matches!(err, RestoreError::ShardUuidMismatch { .. }));
    }

    #[test]
    fn verify_rejects_corrupt_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        write_bundle(&snap, uuid);
        // Flip one byte of arena.bin without touching the manifest.
        let arena = snap.join("arena.bin");
        let mut bytes = std::fs::read(&arena).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&arena, bytes).unwrap();

        let err = verify_snapshot(&snap, uuid).unwrap_err();
        match err {
            RestoreError::Integrity { file, .. } => assert_eq!(file, "arena.bin"),
            other => panic!("expected Integrity, got {other:?}"),
        }
    }

    #[test]
    fn restore_places_files_into_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        let target = tmp.path().join("shard-0");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        write_bundle(&snap, uuid);
        brain_storage::ensure_dirs(&target).unwrap();
        // Pre-existing stale WAL segment that must be cleared.
        std::fs::write(target.join("wal/0000000009.wal"), b"stale").unwrap();

        let report = restore_snapshot(&snap, &target, uuid).expect("restore");
        assert_eq!(report.snapshot_lsn, 123);
        assert_eq!(report.checkpoint_id, 4);
        assert_eq!(report.wal_segments_placed, 1);

        let paths = ShardPaths::at(&target);
        assert_eq!(std::fs::read(paths.arena()).unwrap(), b"arena-bytes");
        assert_eq!(std::fs::read(paths.metadata_db()).unwrap(), b"redb-bytes");
        assert_eq!(
            std::fs::read(target.join("wal/0000000000.wal")).unwrap(),
            b"wal-seg-0"
        );
        assert!(
            !target.join("wal/0000000009.wal").exists(),
            "stale WAL segment must be removed"
        );
    }

    #[test]
    fn restore_refuses_corrupt_bundle_without_touching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        let target = tmp.path().join("shard-0");
        let uuid: [u8; 16] = *uuid::Uuid::now_v7().as_bytes();
        write_bundle(&snap, uuid);
        brain_storage::ensure_dirs(&target).unwrap();
        std::fs::write(ShardPaths::at(&target).arena(), b"original-arena").unwrap();

        // Corrupt the bundle.
        std::fs::write(snap.join("metadata.redb"), b"tampered").unwrap();

        let err = restore_snapshot(&snap, &target, uuid).unwrap_err();
        assert!(matches!(err, RestoreError::Integrity { .. }));
        // Target arena untouched — verification ran before any placement.
        assert_eq!(
            std::fs::read(ShardPaths::at(&target).arena()).unwrap(),
            b"original-arena"
        );
    }
}
