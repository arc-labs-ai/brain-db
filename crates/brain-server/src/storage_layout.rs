//! The storage layout record — what a data directory was created with.
//!
//! `shard_count` is not a tuning knob. A memory id carries the shard that owns
//! it and each shard's data lives under `<data_dir>/<shard_id>/`, so starting
//! the server with a different count routes every id at a shard that does not
//! hold it: the data is intact and unreachable, with nothing to say so.
//!
//! That is load-bearing because the count is DERIVED from the machine for any
//! deployment that does not pin it. Without a record, moving a data directory
//! to a host with a different core count would quietly change the routing. The
//! record makes the first boot's decision durable and gives every later boot
//! something to check itself against.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the record, directly under the data directory.
pub const LAYOUT_FILE: &str = "layout.json";

/// Bumped only when the record's own meaning changes, never for the values.
const LAYOUT_VERSION: u32 = 1;

/// What the data directory was initialised with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageLayout {
    pub version: u32,
    pub shard_count: usize,
}

/// Why a data directory could not be opened with the requested layout.
#[derive(Debug)]
pub enum LayoutError {
    /// The configured count disagrees with what the directory holds. Carries
    /// both so the operator is told what to set rather than just "mismatch".
    ShardCountMismatch {
        configured: usize,
        recorded: usize,
    },
    /// The record exists but was written by a newer server.
    UnsupportedVersion {
        found: u32,
    },
    Io(std::io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShardCountMismatch {
                configured,
                recorded,
            } => write!(
                f,
                "this data directory was created with storage.shard_count = {recorded}, but the \
                 server was started with {configured}. Memory ids route by shard, so serving it \
                 with a different count would make existing memories unreachable. Set \
                 storage.shard_count = {recorded}, or point at an empty data directory."
            ),
            Self::UnsupportedVersion { found } => write!(
                f,
                "storage layout version {found} was written by a newer brain-server than this one"
            ),
            Self::Io(e) => write!(f, "storage layout: {e}"),
            Self::Parse(e) => write!(f, "storage layout is not readable: {e}"),
        }
    }
}

impl std::error::Error for LayoutError {}

fn layout_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LAYOUT_FILE)
}

/// Shard directories already present, which is what a pre-record data
/// directory can still be read from.
///
/// A shard directory is named by its index, so the highest index + 1 is the
/// count the directory was written with. Counting entries instead would be
/// wrong for a directory whose shards have not all been written to yet.
fn shard_dirs_present(data_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str()?.parse::<usize>().ok())
        .map(|idx| idx + 1)
        .max()
        .unwrap_or(0)
}

/// Decide the shard count for this boot, and record it the first time.
///
/// `configured` is `Some` only when the operator pinned it; `None` means they
/// left it to the server, in which case an existing record simply wins — a
/// machine with more cores must not silently re-shard someone's data.
///
/// # Errors
/// [`LayoutError::ShardCountMismatch`] when a pinned count disagrees with what
/// the directory holds, which is refused rather than served wrongly.
pub fn resolve(
    data_dir: &Path,
    configured: Option<usize>,
    derive: impl FnOnce() -> usize,
) -> Result<usize, LayoutError> {
    fs::create_dir_all(data_dir).map_err(LayoutError::Io)?;
    let path = layout_path(data_dir);

    if path.exists() {
        let raw = fs::read_to_string(&path).map_err(LayoutError::Io)?;
        let recorded: StorageLayout = serde_json::from_str(&raw).map_err(LayoutError::Parse)?;
        if recorded.version > LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedVersion {
                found: recorded.version,
            });
        }
        return match configured {
            Some(c) if c != recorded.shard_count => Err(LayoutError::ShardCountMismatch {
                configured: c,
                recorded: recorded.shard_count,
            }),
            _ => Ok(recorded.shard_count),
        };
    }

    // No record. A directory that already holds shards predates it, so adopt
    // what it was written with rather than imposing a fresh decision on data
    // that is already laid out.
    let existing = shard_dirs_present(data_dir);
    let count = if existing > 0 {
        if let Some(c) = configured {
            if c != existing {
                return Err(LayoutError::ShardCountMismatch {
                    configured: c,
                    recorded: existing,
                });
            }
        }
        existing
    } else {
        configured.unwrap_or_else(derive).max(1)
    };

    write_record(&path, count)?;
    Ok(count)
}

fn write_record(path: &Path, shard_count: usize) -> Result<(), LayoutError> {
    let body = serde_json::to_string_pretty(&StorageLayout {
        version: LAYOUT_VERSION,
        shard_count,
    })
    .map_err(LayoutError::Parse)?;
    fs::write(path, body).map_err(LayoutError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dir() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    fn record_of(d: &Path) -> StorageLayout {
        serde_json::from_str(&fs::read_to_string(layout_path(d)).expect("read")).expect("parse")
    }

    #[test]
    fn a_fresh_directory_without_a_pinned_count_is_sized_to_the_machine() {
        let d = dir();
        let n = resolve(d.path(), None, || 12).expect("resolve");
        assert_eq!(n, 12);
        assert_eq!(record_of(d.path()).shard_count, 12, "and it is recorded");
    }

    #[test]
    fn a_pinned_count_wins_on_a_fresh_directory() {
        let d = dir();
        let n = resolve(d.path(), Some(4), || 12).expect("resolve");
        assert_eq!(n, 4);
        assert_eq!(record_of(d.path()).shard_count, 4);
    }

    #[test]
    fn the_record_beats_the_machine_on_a_later_boot() {
        // The reason the record exists: the same data directory must serve
        // identically on a host with a different core count.
        let d = dir();
        resolve(d.path(), None, || 12).expect("first boot");
        let n = resolve(d.path(), None, || 64).expect("second boot, bigger machine");
        assert_eq!(n, 12, "a bigger machine must not re-shard existing data");
    }

    #[test]
    fn a_pinned_count_that_disagrees_with_the_record_is_refused() {
        // Serving it would route every id at a shard that does not hold it.
        let d = dir();
        resolve(d.path(), Some(4), || 1).expect("first boot");
        let err = resolve(d.path(), Some(8), || 1).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains('4'), "names what the directory holds: {msg}");
        assert!(msg.contains('8'), "names what was asked for: {msg}");
        assert!(msg.contains("unreachable"), "says what is at stake: {msg}");
    }

    #[test]
    fn a_matching_pinned_count_is_accepted() {
        let d = dir();
        resolve(d.path(), Some(4), || 1).expect("first boot");
        assert_eq!(resolve(d.path(), Some(4), || 1).expect("second boot"), 4);
    }

    #[test]
    fn a_directory_written_before_the_record_keeps_its_own_layout() {
        // Upgrade path: existing deployments have shard dirs and no record.
        // The highest index + 1 is what they were written with.
        let d = dir();
        for i in 0..3 {
            fs::create_dir_all(d.path().join(i.to_string())).expect("shard dir");
        }
        let n = resolve(d.path(), None, || 32).expect("resolve");
        assert_eq!(n, 3, "adopted from the directory, not from the machine");
        assert_eq!(
            record_of(d.path()).shard_count,
            3,
            "and recorded from now on"
        );
    }

    #[test]
    fn a_sparse_pre_record_directory_counts_by_highest_index() {
        // Only shard 2 has been written to; the layout is still 3 wide.
        let d = dir();
        fs::create_dir_all(d.path().join("2")).expect("shard dir");
        assert_eq!(resolve(d.path(), None, || 99).expect("resolve"), 3);
    }

    #[test]
    fn a_pre_record_directory_refuses_a_disagreeing_pinned_count() {
        let d = dir();
        for i in 0..2 {
            fs::create_dir_all(d.path().join(i.to_string())).expect("shard dir");
        }
        assert!(resolve(d.path(), Some(8), || 1).is_err());
    }

    #[test]
    fn non_shard_directories_are_not_mistaken_for_shards() {
        // `snapshots/` and friends live beside the shards.
        let d = dir();
        fs::create_dir_all(d.path().join("snapshots")).expect("dir");
        let n = resolve(d.path(), None, || 7).expect("resolve");
        assert_eq!(n, 7, "a non-numeric directory is not a shard");
    }

    #[test]
    fn a_record_from_a_newer_server_is_refused() {
        let d = dir();
        fs::create_dir_all(d.path()).expect("dir");
        fs::write(
            layout_path(d.path()),
            serde_json::to_string(&StorageLayout {
                version: LAYOUT_VERSION + 1,
                shard_count: 4,
            })
            .expect("encode"),
        )
        .expect("write");
        assert!(matches!(
            resolve(d.path(), None, || 1),
            Err(LayoutError::UnsupportedVersion { .. })
        ));
    }
}
