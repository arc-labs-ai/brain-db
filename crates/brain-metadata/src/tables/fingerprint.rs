//! `fingerprints` table: per-shard opt-in content-dedup index.
//!
//! Spec §07/07 §6 — opt-in fingerprint deduplication. When an
//! `EncodeRequest` arrives with `deduplicate = true`, the substrate
//! consults this table; on a hit, the existing `MemoryId` is
//! returned without allocating a new slot.
//!
//! ## Key
//!
//! `[u8; 56]` packed big-endian:
//!
//! ```text
//!   0..16   agent_id          (UUID bytes)
//!  16..24   context_id        (u64 BE)
//!  24..56   content_hash      (BLAKE3(canonical_utf8(text))[..32])
//! ```
//!
//! Partitioning by `agent_id` is privacy + ownership: one agent's
//! encoded text never matches against another's index. Partitioning
//! by `context_id` matches spec §07/07 §6.1 — the same utterance in
//! different episodic contexts is a different memory.
//!
//! ## Value
//!
//! [`FingerprintEntry`] — the `MemoryId` of the Active memory and
//! the `inserted_at_unix_nanos` for diagnostics. Only Active
//! memories are reachable here; FORGET / reclamation evict the row
//! in the same write transaction as the tombstone (spec §07/07
//! §6.3 + §07/07 §6.5).
//!
//! ## What does NOT live here
//!
//! - A refcount. v1 deliberately does not refcount — a dedup hit
//!   returns the **same** `MemoryId`, not a new one backed by
//!   shared storage (spec §07/07 §6.6).
//! - Cross-shard entries. The fingerprint table is per-shard;
//!   routing already hashes the agent to one shard.
//! - Tombstone state. The eviction discipline keeps this table
//!   Active-only by construction.

use brain_core::{AgentId, ContextId, MemoryId};
use redb::TableDefinition;

/// The `fingerprints` table. See module docs for key layout.
pub const FINGERPRINTS_TABLE: TableDefinition<'static, [u8; 56], FingerprintEntry> =
    TableDefinition::new("fingerprints");

/// Value row in `FINGERPRINTS_TABLE`. Compact: just the MemoryId of
/// the Active memory and a timestamp for diagnostics.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[archive(check_bytes)]
pub struct FingerprintEntry {
    /// `MemoryId` of the Active memory whose text hashes to this
    /// row's content_hash. Stored as the 16-byte big-endian wire
    /// form (matches `MEMORIES_TABLE`'s key).
    pub memory_id_bytes: [u8; 16],
    /// Unix-nanos timestamp this row was inserted. Diagnostic only;
    /// the dedup hit path doesn't read it.
    pub inserted_at_unix_nanos: u64,
}

impl FingerprintEntry {
    #[must_use]
    pub fn new(memory_id: MemoryId, inserted_at_unix_nanos: u64) -> Self {
        Self {
            memory_id_bytes: memory_id.to_be_bytes(),
            inserted_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn memory_id(&self) -> MemoryId {
        MemoryId::from_be_bytes(self.memory_id_bytes)
    }
}

impl redb::Value for FingerprintEntry {
    type SelfType<'a> = FingerprintEntry;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let archived =
            rkyv::check_archived_root::<FingerprintEntry>(data).expect("invariant: valid rkyv");
        rkyv::Deserialize::deserialize(archived, &mut rkyv::Infallible)
            .expect("invariant: deserialize")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 64>(value)
            .expect("invariant: serialize")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::FingerprintEntry")
    }
}

/// Pack the `(agent_id, context_id, content_hash)` triple into the
/// 56-byte key used by [`FINGERPRINTS_TABLE`].
#[must_use]
pub fn fingerprint_key(
    agent_id: AgentId,
    context_id: ContextId,
    content_hash: &[u8; 32],
) -> [u8; 56] {
    let mut key = [0u8; 56];
    let agent_bytes: [u8; 16] = agent_id.into();
    key[0..16].copy_from_slice(&agent_bytes);
    key[16..24].copy_from_slice(&context_id.0.to_be_bytes());
    key[24..56].copy_from_slice(content_hash);
    key
}

/// Compute the canonical content hash for the given text. Currently
/// a BLAKE3 over the raw UTF-8 bytes; future revisions may add NFC
/// normalisation (spec §07/07 §6.2).
#[must_use]
pub fn content_hash(text: &str) -> [u8; 32] {
    *blake3::hash(text.as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::ShardId;
    use redb::ReadableDatabase;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn fresh_db() -> (TempDir, redb::Database) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("fp.redb");
        let db = redb::Database::create(&path).expect("open");
        (dir, db)
    }

    fn agent(seed: u8) -> AgentId {
        AgentId(Uuid::from_bytes([seed; 16]))
    }

    fn memory(shard: ShardId, slot: u64) -> MemoryId {
        MemoryId::pack(shard, slot, 1)
    }

    #[test]
    fn key_packing_is_deterministic_and_disjoint() {
        let a = fingerprint_key(agent(1), ContextId(7), &content_hash("hello"));
        let b = fingerprint_key(agent(1), ContextId(7), &content_hash("hello"));
        assert_eq!(a, b);

        let c = fingerprint_key(agent(2), ContextId(7), &content_hash("hello"));
        assert_ne!(a, c, "different agent → different key");
        let d = fingerprint_key(agent(1), ContextId(8), &content_hash("hello"));
        assert_ne!(a, d, "different context → different key");
        let e = fingerprint_key(agent(1), ContextId(7), &content_hash("world"));
        assert_ne!(a, e, "different content → different key");
    }

    #[test]
    fn content_hash_stable_across_calls() {
        assert_eq!(content_hash("hello world"), content_hash("hello world"));
        assert_ne!(content_hash("hello world"), content_hash("hello  world"));
    }

    #[test]
    fn round_trip_insert_get_remove() {
        let (_dir, db) = fresh_db();
        let key = fingerprint_key(agent(1), ContextId(7), &content_hash("hello"));
        let mid = memory(0, 42);
        let entry = FingerprintEntry::new(mid, 1_700_000_000_000_000_000);

        // Insert.
        {
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(FINGERPRINTS_TABLE).unwrap();
                t.insert(key, entry).unwrap();
            }
            wtxn.commit().unwrap();
        }

        // Get.
        {
            let rtxn = db.begin_read().unwrap();
            let t = rtxn.open_table(FINGERPRINTS_TABLE).unwrap();
            let got = t.get(key).unwrap().expect("present").value();
            assert_eq!(got.memory_id(), mid);
            assert_eq!(got.inserted_at_unix_nanos, entry.inserted_at_unix_nanos);
        }

        // Remove.
        {
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(FINGERPRINTS_TABLE).unwrap();
                let removed = t.remove(key).unwrap();
                assert!(removed.is_some());
            }
            wtxn.commit().unwrap();
        }

        // Gone.
        {
            let rtxn = db.begin_read().unwrap();
            let t = rtxn.open_table(FINGERPRINTS_TABLE).unwrap();
            assert!(t.get(key).unwrap().is_none());
        }
    }
}
