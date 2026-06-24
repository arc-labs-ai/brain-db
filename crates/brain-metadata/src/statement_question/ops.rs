//! Typed CRUD over the statement question-bridge vector table.
//!
//! Free functions over [`redb::ReadTransaction`] /
//! [`redb::WriteTransaction`], composed inside the caller's own
//! transaction (the embed worker writes question vectors in the same txn
//! that drains the queue). Callers commit themselves. Mirrors
//! [`crate::hype::ops`], keyed by `StatementId` instead of `MemoryId`.

use brain_core::StatementId;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::statement_question::{
    STATEMENT_QUESTION_VECTORS_TABLE, STATEMENT_QUESTION_VECTOR_BYTES,
};

/// Errors from the statement question-bridge CRUD layer.
#[derive(thiserror::Error, Debug)]
pub enum StatementQuestionOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}

/// Little-endian byte image of a question vector.
fn vector_to_bytes(vector: &[f32; 384]) -> [u8; STATEMENT_QUESTION_VECTOR_BYTES] {
    let mut out = [0u8; STATEMENT_QUESTION_VECTOR_BYTES];
    for (i, v) in vector.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// Inverse of [`vector_to_bytes`].
fn bytes_to_vector(bytes: &[u8; STATEMENT_QUESTION_VECTOR_BYTES]) -> [f32; 384] {
    let mut out = [0.0f32; 384];
    for (i, slot) in out.iter_mut().enumerate() {
        let chunk: [u8; 4] = bytes[i * 4..(i + 1) * 4]
            .try_into()
            .expect("invariant: fixed slice");
        *slot = f32::from_le_bytes(chunk);
    }
    out
}

/// Build the 17-byte row key: `statement_id (16) ++ question_index (1)`.
fn row_key(statement_id: StatementId, question_index: u8) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[..16].copy_from_slice(&statement_id.to_bytes());
    key[16] = question_index;
    key
}

/// Persist one question vector for `statement_id` at slot `question_index`.
/// Idempotent on the (statement_id, index) key.
pub fn statement_question_put(
    wtxn: &WriteTransaction,
    statement_id: StatementId,
    question_index: u8,
    vector: &[f32; 384],
) -> Result<(), StatementQuestionOpError> {
    let bytes = vector_to_bytes(vector);
    let mut t = wtxn.open_table(STATEMENT_QUESTION_VECTORS_TABLE)?;
    t.insert(&row_key(statement_id, question_index), &bytes)?;
    Ok(())
}

/// Whether `statement_id` already owns at least one question vector. The
/// embed worker uses this to skip regeneration on re-drain — generation is
/// gated on its OWN vector presence, so a re-queued statement still
/// generates its questions exactly once.
pub fn statement_question_has_vectors(
    rtxn: &ReadTransaction,
    statement_id: StatementId,
) -> Result<bool, StatementQuestionOpError> {
    let prefix = statement_id.to_bytes();
    let lo = row_key(statement_id, 0);
    let hi = row_key(statement_id, u8::MAX);
    let t = rtxn.open_table(STATEMENT_QUESTION_VECTORS_TABLE)?;
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        if k.value()[..16] == prefix {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Delete every question vector owned by `statement_id` (FORGET /
/// supersession / tombstone cascade). Returns the number of rows removed.
pub fn statement_question_delete(
    wtxn: &WriteTransaction,
    statement_id: StatementId,
) -> Result<usize, StatementQuestionOpError> {
    let prefix = statement_id.to_bytes();
    let lo = row_key(statement_id, 0);
    let hi = row_key(statement_id, u8::MAX);
    let mut t = wtxn.open_table(STATEMENT_QUESTION_VECTORS_TABLE)?;
    let mut keys: Vec<[u8; 17]> = Vec::new();
    for entry in t.range(lo..=hi)? {
        let (k, _) = entry?;
        let key = k.value();
        if key[..16] == prefix {
            keys.push(key);
        }
    }
    let removed = keys.len();
    for key in keys {
        t.remove(&key)?;
    }
    Ok(removed)
}

/// One row yielded by [`statement_question_iter_all`]:
/// `(StatementId, vector)`. Several rows may share a `StatementId`.
pub type StatementQuestionRebuildRow = (StatementId, [f32; 384]);

/// Iterate every stored question vector in key order. The boot rebuild
/// feeds these straight into a fresh `StatementQuestionHnswIndex` — no
/// embedder, no templating.
pub fn statement_question_iter_all(
    rtxn: &ReadTransaction,
) -> Result<Vec<StatementQuestionRebuildRow>, StatementQuestionOpError> {
    let t = rtxn.open_table(STATEMENT_QUESTION_VECTORS_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        let key = k.value();
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&key[..16]);
        out.push((
            StatementId::from_bytes(id_bytes),
            bytes_to_vector(&v.value()),
        ));
    }
    Ok(out)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};
    use tempfile::TempDir;

    fn sid(seed: u8) -> StatementId {
        let mut b = [0u8; 16];
        b[0] = seed;
        StatementId::from_bytes(b)
    }

    fn vec_seed(seed: f32) -> [f32; 384] {
        let mut v = [0.0f32; 384];
        v[0] = seed;
        v
    }

    fn open() -> (TempDir, Database) {
        let dir = TempDir::new().unwrap();
        let db = Database::create(dir.path().join("m.redb")).unwrap();
        (dir, db)
    }

    #[test]
    fn put_iter_has_delete_round_trip() {
        let (_d, db) = open();
        let wtxn = db.begin_write().unwrap();
        statement_question_put(&wtxn, sid(1), 0, &vec_seed(0.5)).unwrap();
        statement_question_put(&wtxn, sid(1), 1, &vec_seed(0.7)).unwrap();
        statement_question_put(&wtxn, sid(2), 0, &vec_seed(0.9)).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(statement_question_iter_all(&rtxn).unwrap().len(), 3);
        assert!(statement_question_has_vectors(&rtxn, sid(1)).unwrap());
        assert!(!statement_question_has_vectors(&rtxn, sid(3)).unwrap());
        drop(rtxn);

        let wtxn = db.begin_write().unwrap();
        assert_eq!(statement_question_delete(&wtxn, sid(1)).unwrap(), 2);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert!(!statement_question_has_vectors(&rtxn, sid(1)).unwrap());
        assert!(statement_question_has_vectors(&rtxn, sid(2)).unwrap());
    }
}
