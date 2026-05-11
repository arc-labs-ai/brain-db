//! `texts` table: per-memory UTF-8 text, stored separately from
//! `memories` so metadata reads don't pay for text bytes.
//!
//! See `spec/07_metadata_graph/07_text_storage.md` (full).
//!
//! ## What lives here
//!
//! - [`TEXTS_TABLE`] — `MemoryId` → raw UTF-8 bytes.
//!
//! That's the whole sub-task. The substrate stores the bytes
//! byte-for-byte (spec §5: "carries UTF-8; the substrate stores it
//! byte-for-byte"); every other concern lives above the storage layer.
//!
//! ## What does NOT live here
//!
//! - **UTF-8 validation** (spec §5) — wire layer (Phase 4).
//! - **`max_text_bytes` size limit** (spec §4, §7) — wire layer.
//! - **Immutability enforcement** (spec §8) — application-level
//!   invariant; ENCODE is the only insert path and writes each
//!   MemoryId once.
//! - **Hard-forget secure-erase** (spec §9) — the zero-then-delete
//!   pattern needs `FALLOC_FL_PUNCH_HOLE` on the redb file to actually
//!   evict pages, which is below redb's API. Phase 8 worker territory.
//! - **Same-transaction coupling with `memories`** (spec §15) —
//!   composed inside `MetadataDb` (sub-task 3.10), which opens both
//!   tables inside one `begin_write()`.
//! - **Compression** — not in the spec.

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
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn mid(byte: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[15] = byte;
        b
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(1);
        let text = b"hello world";

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, text.as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap();
        assert_eq!(got.value(), text.as_ref());
    }

    #[test]
    fn missing_key_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        // Create the table so a read txn can open it.
        let wtxn = db.begin_write().unwrap();
        {
            let _t = wtxn.open_table(TEXTS_TABLE).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        assert!(t.get(&mid(99)).unwrap().is_none());
    }

    #[test]
    fn overwrite_replaces_bytes() {
        // Spec §8 makes text immutable at the application level; the
        // storage layer doesn't enforce that. A second insert at the
        // same key replaces the prior bytes. Documented here so a
        // future change to add storage-level immutability doesn't
        // silently regress.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(2);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, b"first".as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, b"second".as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        assert_eq!(t.get(&key).unwrap().unwrap().value(), b"second");
    }

    #[test]
    fn empty_text_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(3);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, b"".as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap();
        assert_eq!(got.value(), b"");
    }

    #[test]
    fn large_text_round_trips() {
        // 1 MB — the spec's default `max_text_bytes` ceiling (§4, §7).
        // The substrate doesn't enforce the limit; this test pins that
        // reads/writes at that size work in practice.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(4);
        let payload: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024).collect();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, payload.as_slice()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap();
        assert_eq!(got.value().len(), payload.len());
        assert_eq!(got.value(), payload.as_slice());
    }

    #[test]
    fn utf8_bytes_round_trip_byte_for_byte() {
        // Multi-byte UTF-8 sequences must survive unchanged. The
        // substrate doesn't decode or re-encode (spec §5).
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

    #[test]
    fn delete_removes_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = mid(6);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, b"to be removed".as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            assert!(t.remove(&key).unwrap().is_some());
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_none());
    }

    #[test]
    fn iterate_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&mid(10), b"alpha".as_ref()).unwrap();
            t.insert(&mid(20), b"beta".as_ref()).unwrap();
            t.insert(&mid(30), b"gamma".as_ref()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(TEXTS_TABLE).unwrap();
        let mut got: Vec<(u8, Vec<u8>)> = t
            .iter()
            .unwrap()
            .map(|entry| {
                let (k, v) = entry.unwrap();
                (k.value()[15], v.value().to_vec())
            })
            .collect();
        got.sort_by_key(|(k, _)| *k);
        assert_eq!(
            got,
            vec![
                (10, b"alpha".to_vec()),
                (20, b"beta".to_vec()),
                (30, b"gamma".to_vec()),
            ]
        );
    }
}
