//! `texts` table: per-memory UTF-8 text, stored separately from
//! `memories` so metadata reads don't pay for text bytes.
//!
//! ## What lives here
//!
//! - [`TEXTS_TABLE`] — `MemoryId` → raw UTF-8 bytes.
//!
//! The substrate stores the bytes byte-for-byte; every other concern
//! lives above the storage layer.
//!
//! ## What does NOT live here
//!
//! - **UTF-8 validation** — wire layer.
//! - **`max_text_bytes` size limit** — wire layer.
//! - **Immutability enforcement** — application-level
//!   invariant; ENCODE is the only insert path and writes each
//!   MemoryId once.
//! - **Hard-forget secure-erase** — the zero-then-delete
//!   pattern needs `FALLOC_FL_PUNCH_HOLE` on the redb file to actually
//!   evict pages, which is below redb's API. Maintenance-worker
//!   territory.
//! - **Same-transaction coupling with `memories`** —
//!   composed inside `MetadataDb`, which opens both tables inside one
//!   `begin_write()`.
//! - **Compression** — out of scope.

use redb::TableDefinition;

/// The `texts` table. Key is the `MemoryId`'s 16-byte raw form (`MemoryId::to_be_bytes()`);
/// value is the memory's UTF-8 bytes, stored as redb's built-in
/// variable-length `&[u8]` type.
///
/// We deliberately use `&[u8]` rather than wrapping a `Vec<u8>` in a
/// rkyv `Value` impl: there's no struct to evolve here, and routing
/// every read through rkyv would add an encode/decode pass and the
/// `AlignedVec` alignment-copy workaround for zero benefit.
pub const TEXTS_TABLE: TableDefinition<'static, [u8; 16], &'static [u8]> =
    TableDefinition::new("texts");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn mid(byte: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[15] = byte;
        b
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn utf8_bytes_round_trip_byte_for_byte() {
        // Multi-byte UTF-8 sequences must survive unchanged. The
        // substrate doesn't decode or re-encode.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(5);
        let text = "héllo 🌍 — multibyte ☃ sequences".as_bytes();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, text).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap();
        assert_eq!(got.value(), text);
        // And the bytes still decode back to the original string.
        assert_eq!(
            std::str::from_utf8(got.value()).unwrap(),
            "héllo 🌍 — multibyte ☃ sequences"
        );
    }
}
