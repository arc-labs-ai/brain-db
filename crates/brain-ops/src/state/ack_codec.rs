//! Binary codec for [`crate::write::WriteAck`].
//!
//! The durable idempotency cache stores one row per accepted `Write`;
//! the row's `response_payload` is the encoded `WriteAck`. On a replay
//! the writer decodes the blob and hands the value back without
//! re-executing the write. No external serialization framework is used:
//! `WriteAck` traverses brain-core / brain-storage / brain-protocol
//! types that don't uniformly derive `serde` or `rkyv`, and threading
//! either through every intermediate would balloon the change footprint.
//! A hand-rolled codec keeps the blob format under the control of the
//! one module that needs it.
//!
//! The wire format is little-endian primitives + length-prefixed
//! variable parts. Length prefixes are `u32`. Strings are UTF-8.
//! `pending_stages` is intentionally NOT serialized — on cache hit
//! background stages aren't re-enqueued (a successful original submit
//! already kicked them off), so the replay returns an empty
//! `pending_stages` vector.

use std::convert::TryFrom;

use brain_core::{
    Entity, EntityAttributes, EntityId, EntityTypeId, ExtractorId, MergeId, RelationId, StatementId,
};
use brain_core::MemoryId;
use brain_storage::wal::record::Lsn;

use crate::write::phase::{
    PhaseAck, SupersedeReplacementId, SupersedeTarget, TombstoneMode, TombstoneTarget,
};
use crate::write::{WriteAck, WriteId};

/// Codec format version. Bumped only when the on-disk shape changes in
/// an incompatible way; a mismatched version on decode triggers an
/// `UnsupportedVersion` error and the writer treats the row as a miss
/// (the GC worker will reclaim it).
const FORMAT_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("buffer underrun at offset {0}")]
    Underrun(usize),
    #[error("unsupported format version: got {got}, expected {expected}")]
    UnsupportedVersion { got: u8, expected: u8 },
    #[error("unknown phase ack tag: {0}")]
    UnknownPhaseAckTag(u8),
    #[error("unknown tombstone target tag: {0}")]
    UnknownTombstoneTargetTag(u8),
    #[error("unknown supersede target tag: {0}")]
    UnknownSupersedeTargetTag(u8),
    #[error("unknown supersede replacement tag: {0}")]
    UnknownSupersedeReplacementTag(u8),
    #[error("invalid utf-8 in string field")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("length prefix overflow: {0}")]
    LengthOverflow(u32),
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

#[must_use]
pub fn encode_write_ack(ack: &WriteAck) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&ack.write_id.to_bytes());
    write_u64(&mut out, ack.committed_at_unix_nanos);
    write_u64(&mut out, ack.lsn_first.raw());
    write_u64(&mut out, ack.lsn_last.raw());
    write_len(&mut out, ack.phase_acks.len());
    for pa in &ack.phase_acks {
        write_phase_ack(&mut out, pa);
    }
    out
}

pub fn decode_write_ack(buf: &[u8]) -> Result<WriteAck, CodecError> {
    let mut c = Cursor::new(buf);
    let version = c.u8()?;
    if version != FORMAT_VERSION {
        return Err(CodecError::UnsupportedVersion {
            got: version,
            expected: FORMAT_VERSION,
        });
    }
    let write_id = WriteId::from_bytes(c.bytes16()?);
    let committed_at_unix_nanos = c.u64()?;
    let lsn_first = Lsn(c.u64()?);
    let lsn_last = Lsn(c.u64()?);
    let n = c.len()?;
    let mut phase_acks = Vec::with_capacity(n);
    for _ in 0..n {
        phase_acks.push(read_phase_ack(&mut c)?);
    }
    Ok(WriteAck {
        write_id,
        committed_at_unix_nanos,
        lsn_first,
        lsn_last,
        phase_acks,
        // Background stages are not part of the durable ack — they're
        // best-effort enqueues whose `StageCompleted` events fire only
        // once. On replay we don't re-enqueue, so an empty list is the
        // correct surface.
        pending_stages: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// PhaseAck encode.
// ---------------------------------------------------------------------------

// Tag bytes — wire-stable once shipped. Append-only.
mod tag {
    pub const UPSERTED_MEMORY: u8 = 1;
    pub const UPSERTED_ENTITY: u8 = 2;
    pub const UPSERTED_STATEMENT: u8 = 3;
    pub const UPSERTED_RELATION: u8 = 4;
    pub const UPSERTED_SCHEMA: u8 = 5;
    pub const LINKED: u8 = 6;
    pub const UNLINKED: u8 = 7;
    pub const TOMBSTONED: u8 = 8;
    pub const SUPERSEDED: u8 = 9;
    pub const SALIENCE_UPDATED: u8 = 10;
    pub const KIND_UPDATED: u8 = 11;
    pub const CONTEXT_UPDATED: u8 = 12;
    pub const EMBEDDING_UPDATED: u8 = 13;
    pub const ENTITY_UPDATED: u8 = 14;
    pub const ENTITY_RENAMED: u8 = 15;
    pub const ENTITIES_UNMERGED: u8 = 16;
    pub const ENTITY_MERGED: u8 = 17;
    pub const EXTRACTOR_ENABLED_SET: u8 = 18;
    pub const SLOTS_RECLAIMED: u8 = 19;
    pub const MERGE_PROPOSAL_APPROVED: u8 = 20;
    pub const MERGE_PROPOSAL_REJECTED: u8 = 21;
}

mod tomb_tag {
    pub const MEMORY: u8 = 1;
    pub const ENTITY: u8 = 2;
    pub const STATEMENT: u8 = 3;
    pub const RELATION: u8 = 4;
}

mod sup_target_tag {
    pub const STATEMENT: u8 = 1;
    pub const RELATION: u8 = 2;
}

mod sup_repl_tag {
    pub const STATEMENT: u8 = 1;
    pub const RELATION: u8 = 2;
}

fn write_phase_ack(out: &mut Vec<u8>, pa: &PhaseAck) {
    match pa {
        PhaseAck::UpsertedMemory(id) => {
            out.push(tag::UPSERTED_MEMORY);
            write_memory_id(out, *id);
        }
        PhaseAck::UpsertedEntity(id) => {
            out.push(tag::UPSERTED_ENTITY);
            out.extend_from_slice(&id.to_bytes());
        }
        PhaseAck::UpsertedStatement(id, ver) => {
            out.push(tag::UPSERTED_STATEMENT);
            out.extend_from_slice(&id.to_bytes());
            write_u32(out, *ver);
        }
        PhaseAck::UpsertedRelation(id, ver) => {
            out.push(tag::UPSERTED_RELATION);
            out.extend_from_slice(&id.to_bytes());
            write_u32(out, *ver);
        }
        PhaseAck::UpsertedSchema { namespace, version } => {
            out.push(tag::UPSERTED_SCHEMA);
            write_string(out, namespace);
            write_u32(out, *version);
        }
        PhaseAck::Linked => out.push(tag::LINKED),
        PhaseAck::Unlinked => out.push(tag::UNLINKED),
        PhaseAck::Tombstoned {
            target,
            tombstoned_at_unix_nanos,
        } => {
            out.push(tag::TOMBSTONED);
            write_tombstone_target(out, target);
            write_u64(out, *tombstoned_at_unix_nanos);
        }
        PhaseAck::Superseded(target, replacement) => {
            out.push(tag::SUPERSEDED);
            write_supersede_target(out, target);
            write_supersede_replacement_id(out, replacement);
        }
        PhaseAck::SalienceUpdated => out.push(tag::SALIENCE_UPDATED),
        PhaseAck::KindUpdated => out.push(tag::KIND_UPDATED),
        PhaseAck::ContextUpdated => out.push(tag::CONTEXT_UPDATED),
        PhaseAck::EmbeddingUpdated => out.push(tag::EMBEDDING_UPDATED),
        PhaseAck::EntityUpdated { id, snapshot } => {
            out.push(tag::ENTITY_UPDATED);
            out.extend_from_slice(&id.to_bytes());
            write_entity(out, snapshot);
        }
        PhaseAck::EntityRenamed {
            id,
            old_canonical_name,
            snapshot,
        } => {
            out.push(tag::ENTITY_RENAMED);
            out.extend_from_slice(&id.to_bytes());
            write_string(out, old_canonical_name);
            write_entity(out, snapshot);
        }
        PhaseAck::EntitiesUnmerged { restored, survivor } => {
            out.push(tag::ENTITIES_UNMERGED);
            out.extend_from_slice(&restored.to_bytes());
            out.extend_from_slice(&survivor.to_bytes());
        }
        PhaseAck::EntityMerged {
            source,
            target,
            audit_id,
        } => {
            out.push(tag::ENTITY_MERGED);
            out.extend_from_slice(&source.to_bytes());
            out.extend_from_slice(&target.to_bytes());
            out.extend_from_slice(&audit_id.to_bytes());
        }
        PhaseAck::ExtractorEnabledSet { id, enabled } => {
            out.push(tag::EXTRACTOR_ENABLED_SET);
            write_u32(out, id.raw());
            out.push(u8::from(*enabled));
        }
        PhaseAck::SlotsReclaimed { count } => {
            out.push(tag::SLOTS_RECLAIMED);
            write_u64(out, *count as u64);
        }
        PhaseAck::MergeProposalApproved {
            proposal_id,
            audit_id,
        } => {
            out.push(tag::MERGE_PROPOSAL_APPROVED);
            out.extend_from_slice(&proposal_id.to_bytes());
            out.extend_from_slice(&audit_id.to_bytes());
        }
        PhaseAck::MergeProposalRejected { proposal_id } => {
            out.push(tag::MERGE_PROPOSAL_REJECTED);
            out.extend_from_slice(&proposal_id.to_bytes());
        }
    }
}

fn read_phase_ack(c: &mut Cursor<'_>) -> Result<PhaseAck, CodecError> {
    let tag = c.u8()?;
    Ok(match tag {
        tag::UPSERTED_MEMORY => PhaseAck::UpsertedMemory(read_memory_id(c)?),
        tag::UPSERTED_ENTITY => PhaseAck::UpsertedEntity(EntityId::from_bytes(c.bytes16()?)),
        tag::UPSERTED_STATEMENT => {
            let id = StatementId::from_bytes(c.bytes16()?);
            let ver = c.u32()?;
            PhaseAck::UpsertedStatement(id, ver)
        }
        tag::UPSERTED_RELATION => {
            let id = RelationId::from_bytes(c.bytes16()?);
            let ver = c.u32()?;
            PhaseAck::UpsertedRelation(id, ver)
        }
        tag::UPSERTED_SCHEMA => {
            let namespace = c.string()?;
            let version = c.u32()?;
            PhaseAck::UpsertedSchema { namespace, version }
        }
        tag::LINKED => PhaseAck::Linked,
        tag::UNLINKED => PhaseAck::Unlinked,
        tag::TOMBSTONED => {
            let target = read_tombstone_target(c)?;
            let tombstoned_at_unix_nanos = c.u64()?;
            PhaseAck::Tombstoned {
                target,
                tombstoned_at_unix_nanos,
            }
        }
        tag::SUPERSEDED => {
            let target = read_supersede_target(c)?;
            let repl = read_supersede_replacement_id(c)?;
            PhaseAck::Superseded(target, repl)
        }
        tag::SALIENCE_UPDATED => PhaseAck::SalienceUpdated,
        tag::KIND_UPDATED => PhaseAck::KindUpdated,
        tag::CONTEXT_UPDATED => PhaseAck::ContextUpdated,
        tag::EMBEDDING_UPDATED => PhaseAck::EmbeddingUpdated,
        tag::ENTITY_UPDATED => {
            let id = EntityId::from_bytes(c.bytes16()?);
            let snapshot = read_entity(c)?;
            PhaseAck::EntityUpdated {
                id,
                snapshot: Box::new(snapshot),
            }
        }
        tag::ENTITY_RENAMED => {
            let id = EntityId::from_bytes(c.bytes16()?);
            let old_canonical_name = c.string()?;
            let snapshot = read_entity(c)?;
            PhaseAck::EntityRenamed {
                id,
                old_canonical_name,
                snapshot: Box::new(snapshot),
            }
        }
        tag::ENTITIES_UNMERGED => {
            let restored = EntityId::from_bytes(c.bytes16()?);
            let survivor = EntityId::from_bytes(c.bytes16()?);
            PhaseAck::EntitiesUnmerged { restored, survivor }
        }
        tag::ENTITY_MERGED => {
            let source = EntityId::from_bytes(c.bytes16()?);
            let target = EntityId::from_bytes(c.bytes16()?);
            let audit_id = MergeId::from_bytes(c.bytes16()?);
            PhaseAck::EntityMerged {
                source,
                target,
                audit_id,
            }
        }
        tag::EXTRACTOR_ENABLED_SET => {
            let id = ExtractorId::from(c.u32()?);
            let enabled = c.u8()? != 0;
            PhaseAck::ExtractorEnabledSet { id, enabled }
        }
        tag::SLOTS_RECLAIMED => {
            let count = c.u64()? as usize;
            PhaseAck::SlotsReclaimed { count }
        }
        tag::MERGE_PROPOSAL_APPROVED => {
            let proposal_id = MergeId::from_bytes(c.bytes16()?);
            let audit_id = MergeId::from_bytes(c.bytes16()?);
            PhaseAck::MergeProposalApproved {
                proposal_id,
                audit_id,
            }
        }
        tag::MERGE_PROPOSAL_REJECTED => {
            let proposal_id = MergeId::from_bytes(c.bytes16()?);
            PhaseAck::MergeProposalRejected { proposal_id }
        }
        other => return Err(CodecError::UnknownPhaseAckTag(other)),
    })
}

// ---------------------------------------------------------------------------
// Nested types.
// ---------------------------------------------------------------------------

fn write_tombstone_target(out: &mut Vec<u8>, t: &TombstoneTarget) {
    match t {
        TombstoneTarget::Memory { id, mode } => {
            out.push(tomb_tag::MEMORY);
            write_memory_id(out, *id);
            out.push(match mode {
                TombstoneMode::Soft => 0,
                TombstoneMode::Hard => 1,
            });
        }
        TombstoneTarget::Entity(id) => {
            out.push(tomb_tag::ENTITY);
            out.extend_from_slice(&id.to_bytes());
        }
        TombstoneTarget::Statement(id) => {
            out.push(tomb_tag::STATEMENT);
            out.extend_from_slice(&id.to_bytes());
        }
        TombstoneTarget::Relation(id) => {
            out.push(tomb_tag::RELATION);
            out.extend_from_slice(&id.to_bytes());
        }
    }
}

fn read_tombstone_target(c: &mut Cursor<'_>) -> Result<TombstoneTarget, CodecError> {
    let t = c.u8()?;
    Ok(match t {
        tomb_tag::MEMORY => {
            let id = read_memory_id(c)?;
            let mode = match c.u8()? {
                0 => TombstoneMode::Soft,
                _ => TombstoneMode::Hard,
            };
            TombstoneTarget::Memory { id, mode }
        }
        tomb_tag::ENTITY => TombstoneTarget::Entity(EntityId::from_bytes(c.bytes16()?)),
        tomb_tag::STATEMENT => TombstoneTarget::Statement(StatementId::from_bytes(c.bytes16()?)),
        tomb_tag::RELATION => TombstoneTarget::Relation(RelationId::from_bytes(c.bytes16()?)),
        other => return Err(CodecError::UnknownTombstoneTargetTag(other)),
    })
}

fn write_supersede_target(out: &mut Vec<u8>, t: &SupersedeTarget) {
    match t {
        SupersedeTarget::Statement(id) => {
            out.push(sup_target_tag::STATEMENT);
            out.extend_from_slice(&id.to_bytes());
        }
        SupersedeTarget::Relation(id) => {
            out.push(sup_target_tag::RELATION);
            out.extend_from_slice(&id.to_bytes());
        }
    }
}

fn read_supersede_target(c: &mut Cursor<'_>) -> Result<SupersedeTarget, CodecError> {
    let t = c.u8()?;
    Ok(match t {
        sup_target_tag::STATEMENT => {
            SupersedeTarget::Statement(StatementId::from_bytes(c.bytes16()?))
        }
        sup_target_tag::RELATION => SupersedeTarget::Relation(RelationId::from_bytes(c.bytes16()?)),
        other => return Err(CodecError::UnknownSupersedeTargetTag(other)),
    })
}

fn write_supersede_replacement_id(out: &mut Vec<u8>, r: &SupersedeReplacementId) {
    match r {
        SupersedeReplacementId::Statement(id) => {
            out.push(sup_repl_tag::STATEMENT);
            out.extend_from_slice(&id.to_bytes());
        }
        SupersedeReplacementId::Relation(id) => {
            out.push(sup_repl_tag::RELATION);
            out.extend_from_slice(&id.to_bytes());
        }
    }
}

fn read_supersede_replacement_id(c: &mut Cursor<'_>) -> Result<SupersedeReplacementId, CodecError> {
    let t = c.u8()?;
    Ok(match t {
        sup_repl_tag::STATEMENT => {
            SupersedeReplacementId::Statement(StatementId::from_bytes(c.bytes16()?))
        }
        sup_repl_tag::RELATION => {
            SupersedeReplacementId::Relation(RelationId::from_bytes(c.bytes16()?))
        }
        other => return Err(CodecError::UnknownSupersedeReplacementTag(other)),
    })
}

fn write_entity(out: &mut Vec<u8>, e: &Entity) {
    out.extend_from_slice(&e.id.to_bytes());
    write_u32(out, e.entity_type.raw());
    write_string(out, &e.canonical_name);
    write_string(out, &e.normalized_name);
    write_len(out, e.aliases.len());
    for a in &e.aliases {
        write_string(out, a);
    }
    let attr_bytes = e.attributes.as_bytes();
    write_len(out, attr_bytes.len());
    out.extend_from_slice(attr_bytes);
    write_u32(out, e.mention_count);
    write_u64(out, e.created_at_unix_nanos);
    write_u64(out, e.updated_at_unix_nanos);
    match &e.merged_into {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_bytes());
        }
        None => out.push(0),
    }
    write_u32(out, e.embedding_version);
    write_u32(out, e.flags);
}

fn read_entity(c: &mut Cursor<'_>) -> Result<Entity, CodecError> {
    let id = EntityId::from_bytes(c.bytes16()?);
    let entity_type = EntityTypeId::from(c.u32()?);
    let canonical_name = c.string()?;
    let normalized_name = c.string()?;
    let alias_count = c.len()?;
    let mut aliases = Vec::with_capacity(alias_count);
    for _ in 0..alias_count {
        aliases.push(c.string()?);
    }
    let attr_len = c.len()?;
    let attr_bytes = c.take(attr_len)?.to_vec();
    let attributes = EntityAttributes::from(attr_bytes);
    let mention_count = c.u32()?;
    let created_at_unix_nanos = c.u64()?;
    let updated_at_unix_nanos = c.u64()?;
    let merged_into = match c.u8()? {
        0 => None,
        _ => Some(EntityId::from_bytes(c.bytes16()?)),
    };
    let embedding_version = c.u32()?;
    let flags = c.u32()?;
    Ok(Entity {
        id,
        entity_type,
        canonical_name,
        normalized_name,
        aliases,
        attributes,
        mention_count,
        created_at_unix_nanos,
        updated_at_unix_nanos,
        merged_into,
        embedding_version,
        flags,
    })
}

// ---------------------------------------------------------------------------
// Primitive writers + cursor reader.
// ---------------------------------------------------------------------------

fn write_memory_id(out: &mut Vec<u8>, id: MemoryId) {
    out.extend_from_slice(&id.to_be_bytes());
}

fn read_memory_id(c: &mut Cursor<'_>) -> Result<MemoryId, CodecError> {
    Ok(MemoryId::from_be_bytes(c.bytes16()?))
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_len(out: &mut Vec<u8>, n: usize) {
    let n_u32 = u32::try_from(n).expect("invariant: length fits in u32");
    write_u32(out, n_u32);
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(CodecError::Underrun(self.pos))?;
        if end > self.buf.len() {
            return Err(CodecError::Underrun(self.pos));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    fn bytes16(&mut self) -> Result<[u8; 16], CodecError> {
        let s = self.take(16)?;
        let mut b = [0u8; 16];
        b.copy_from_slice(s);
        Ok(b)
    }

    fn len(&mut self) -> Result<usize, CodecError> {
        let raw = self.u32()?;
        usize::try_from(raw).map_err(|_| CodecError::LengthOverflow(raw))
    }

    fn string(&mut self) -> Result<String, CodecError> {
        let n = self.len()?;
        let s = self.take(n)?.to_vec();
        Ok(String::from_utf8(s)?)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ack_with(phase_acks: Vec<PhaseAck>) -> WriteAck {
        WriteAck {
            write_id: WriteId::new(),
            committed_at_unix_nanos: 1_700_000_000_000_000_000,
            lsn_first: Lsn(42),
            lsn_last: Lsn(50),
            phase_acks,
            pending_stages: Vec::new(),
        }
    }

    fn round_trip(ack: WriteAck) -> WriteAck {
        let bytes = encode_write_ack(&ack);
        decode_write_ack(&bytes).expect("decode")
    }

    #[test]
    fn round_trip_empty_phase_acks() {
        let original = ack_with(Vec::new());
        let decoded = round_trip(original.clone());
        assert_eq!(decoded.write_id, original.write_id);
        assert_eq!(
            decoded.committed_at_unix_nanos,
            original.committed_at_unix_nanos
        );
        assert_eq!(decoded.lsn_first, original.lsn_first);
        assert_eq!(decoded.lsn_last, original.lsn_last);
        assert!(decoded.phase_acks.is_empty());
        assert!(decoded.pending_stages.is_empty());
    }

    #[test]
    fn round_trip_linked_unlinked() {
        let decoded = round_trip(ack_with(vec![PhaseAck::Linked, PhaseAck::Unlinked]));
        assert_eq!(
            decoded.phase_acks,
            vec![PhaseAck::Linked, PhaseAck::Unlinked]
        );
    }

    #[test]
    fn round_trip_upserted_memory() {
        let id = MemoryId::pack(7, 42, 3);
        let decoded = round_trip(ack_with(vec![PhaseAck::UpsertedMemory(id)]));
        assert_eq!(decoded.phase_acks, vec![PhaseAck::UpsertedMemory(id)]);
    }

    #[test]
    fn round_trip_upserted_schema() {
        let pa = PhaseAck::UpsertedSchema {
            namespace: "acme".into(),
            version: 7,
        };
        let decoded = round_trip(ack_with(vec![pa.clone()]));
        assert_eq!(decoded.phase_acks, vec![pa]);
    }

    #[test]
    fn round_trip_tombstoned_memory_soft() {
        let pa = PhaseAck::Tombstoned {
            target: TombstoneTarget::Memory {
                id: MemoryId::pack(0, 1, 0),
                mode: TombstoneMode::Soft,
            },
            tombstoned_at_unix_nanos: 1_700_000_001_000,
        };
        let decoded = round_trip(ack_with(vec![pa.clone()]));
        assert_eq!(decoded.phase_acks, vec![pa]);
    }

    #[test]
    fn round_trip_entity_renamed() {
        let id = EntityId::new();
        let snapshot = Box::new(Entity::new_active(
            id,
            EntityTypeId::from(1),
            "New Name".into(),
            "new name".into(),
            1_700_000_000_000,
        ));
        let pa = PhaseAck::EntityRenamed {
            id,
            old_canonical_name: "Old Name".into(),
            snapshot,
        };
        let decoded = round_trip(ack_with(vec![pa.clone()]));
        assert_eq!(decoded.phase_acks, vec![pa]);
    }

    #[test]
    fn unsupported_version_errors() {
        let mut bytes = encode_write_ack(&ack_with(vec![PhaseAck::Linked]));
        bytes[0] = 99;
        let err = decode_write_ack(&bytes).unwrap_err();
        assert!(matches!(err, CodecError::UnsupportedVersion { .. }));
    }

    #[test]
    fn truncated_buffer_errors() {
        let bytes = encode_write_ack(&ack_with(vec![PhaseAck::Linked]));
        let err = decode_write_ack(&bytes[..bytes.len() - 1]).unwrap_err();
        assert!(matches!(err, CodecError::Underrun(_)));
    }

    #[test]
    fn pending_stages_drops_on_decode() {
        // pending_stages is post-commit fan-out; the cache hit must
        // surface an empty list so subscribers don't expect duplicate
        // StageCompleted events for an already-completed write.
        let mut ack = ack_with(vec![PhaseAck::Linked]);
        ack.pending_stages.push(crate::write::PendingStage {
            memory_id: MemoryId::pack(0, 1, 0),
            stage_kind: brain_protocol::StageKind::AutoEdge,
        });
        let decoded = round_trip(ack);
        assert!(decoded.pending_stages.is_empty());
    }
}
