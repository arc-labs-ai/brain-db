//! `model_fingerprints` table: per-shard registry of seen embedding-model
//! fingerprints.
//!
//! ## Why fingerprint
//!
//! Vectors from different models live in different geometric spaces;
//! comparing across them is noise. The substrate tags every memory
//! with the 16-byte fingerprint of the model that produced its vector
//! (a truncated BLAKE3 over the model's config/tokenizer/weights) and
//! refuses to compare across fingerprints.
//!
//! This table records every fingerprint the shard has ever seen, plus
//! the human-readable model name, when it was first seen, and the
//! memory count for that fingerprint (denormalised; reconciled by the
//! maintenance worker).
//!
//! ## What lives here
//!
//! - [`MODEL_FINGERPRINTS_TABLE`] — `fingerprint_bytes: [u8; 16]` →
//!   [`ModelInfo`].
//! - [`ModelInfo`] — rkyv-derived value carrying `model_name`,
//!   `seen_at_unix_nanos`, `memory_count_at_fingerprint`.
//!
//! ## What does NOT live here
//!
//! - **Auto-registration on first-seen fingerprint**
//!   — wire layer composes ENCODE → insert-if-absent.
//! - **`ADMIN_REGISTER_MODEL` opcode** — wire layer.
//! - **`memory_count_at_fingerprint` maintenance** — the maintenance
//!   worker reconciles by scanning `memories`.

use redb::TableDefinition;

/// The `model_fingerprints` table. Key is the 16-byte fingerprint
/// (`ModelFingerprint = [u8; 16]`, a BLAKE3 truncation
/// over the model's config/tokenizer/weights + substrate-specific
/// fields like `vector_dim` and `normalize`).
pub const MODEL_FINGERPRINTS_TABLE: TableDefinition<'static, [u8; 16], ModelInfo> =
    TableDefinition::new("model_fingerprints");

/// Per-fingerprint metadata row.
///
/// - `model_name` — human-readable label (e.g., `"bge-small-en-v1.5"`).
///   The row is *content-addressed* via the fingerprint; the name is
///   for human convenience (logs, ADMIN_STATS).
/// - `seen_at_unix_nanos` — when the substrate first inserted this row.
///   Time fields are suffixed with `_unix_nanos` by convention.
/// - `memory_count_at_fingerprint` — denormalised. The wire layer
///   may bump it on each ENCODE; the maintenance worker reconciles by
///   scanning `memories`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ModelInfo {
    pub model_name: String,
    pub seen_at_unix_nanos: u64,
    pub memory_count_at_fingerprint: u64,
}

impl ModelInfo {
    #[must_use]
    pub fn new(model_name: String, seen_at_unix_nanos: u64) -> Self {
        Self {
            model_name,
            seen_at_unix_nanos,
            memory_count_at_fingerprint: 0,
        }
    }
}

impl redb::Value for ModelInfo {
    type SelfType<'a> = ModelInfo;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        // rkyv 0.7's validation includes alignment; redb returns bytes
        // at arbitrary alignment, so copy into an AlignedVec first.
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<ModelInfo>(&buf)
            .expect("ModelInfo bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("ModelInfo is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::ModelInfo")
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn fp(byte: u8) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = 0xFE;
        b[15] = byte;
        b
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    #[test]
    fn model_info_with_long_name_round_trips() {
        // Exercises rkyv's variable-length String path (and the
        // AlignedVec copy in from_bytes).
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let key = fp(2);
        let info = ModelInfo::new(
            "really-quite-long-model-name-with-org-prefix/and-version-suffix-v2.7.1-aligned"
                .to_string(),
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MODEL_FINGERPRINTS_TABLE).unwrap();
            t.insert(&key, &info).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MODEL_FINGERPRINTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got.model_name.len(), info.model_name.len());
        assert_eq!(got, info);
    }
}
