//! Request-hash helpers for the unified write path's idempotency
//! cache.
//!
//! a duplicate `request_id` with a different
//! `request_hash` returns `Conflict`. The hash covers every
//! canonical field of the request **except** the `request_id` itself
//! (that's the table key). Fields are joined with NUL separators to
//! prevent canonicalisation ambiguity. Wire handlers compute the hash
//! and stamp it on the `Write` they submit; the writer's
//! `WriteIdempotencyCache` compares hashes on lookup.

use brain_planner::{EncodeOp, ForgetOp, LinkOp, UnlinkOp};
use brain_protocol::envelope::request::ForgetMode;

/// BLAKE3 over the canonical encode-request fields. Excludes
/// `request_id` (the table key) —.
#[must_use]
pub fn hash_encode_request(op: &EncodeOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"encode:");
    h.update(op.text.as_bytes());
    h.update(b"\0");
    h.update(&op.context_id.raw().to_le_bytes());
    h.update(b"\0");
    h.update(&[memory_kind_byte(op.kind)]);
    h.update(b"\0");
    h.update(&op.salience_initial.to_le_bytes());
    h.update(b"\0");
    h.update(&op.fingerprint);
    h.update(b"\0");
    h.update(&(op.edges.len() as u32).to_le_bytes());
    for edge in &op.edges {
        h.update(&edge.target.to_be_bytes());
        h.update(&[edge_kind_byte(edge.kind)]);
        h.update(&edge.weight.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_forget_request(op: &ForgetOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"forget:");
    h.update(&op.memory_id.to_be_bytes());
    h.update(b"\0");
    h.update(&[forget_mode_byte(op.mode)]);
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_link_request(op: &LinkOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"link:");
    h.update(&op.source.to_be_bytes());
    h.update(b"\0");
    h.update(&op.target.to_be_bytes());
    h.update(b"\0");
    h.update(&[edge_kind_byte(op.kind)]);
    h.update(b"\0");
    h.update(&op.weight.to_le_bytes());
    *h.finalize().as_bytes()
}

#[must_use]
pub fn hash_unlink_request(op: &UnlinkOp) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"unlink:");
    h.update(&op.source.to_be_bytes());
    h.update(b"\0");
    h.update(&op.target.to_be_bytes());
    h.update(b"\0");
    h.update(&[edge_kind_byte(op.kind)]);
    *h.finalize().as_bytes()
}

fn memory_kind_byte(k: brain_core::MemoryKind) -> u8 {
    match k {
        brain_core::MemoryKind::Episodic => 0,
        brain_core::MemoryKind::Semantic => 1,
        brain_core::MemoryKind::Consolidated => 2,
    }
}

fn edge_kind_byte(k: brain_core::EdgeKind) -> u8 {
    match k {
        brain_core::EdgeKind::Caused => 0,
        brain_core::EdgeKind::FollowedBy => 1,
        brain_core::EdgeKind::DerivedFrom => 2,
        brain_core::EdgeKind::SimilarTo => 3,
        brain_core::EdgeKind::Contradicts => 4,
        brain_core::EdgeKind::Supports => 5,
        brain_core::EdgeKind::References => 6,
        brain_core::EdgeKind::PartOf => 7,
    }
}

fn forget_mode_byte(m: ForgetMode) -> u8 {
    match m {
        ForgetMode::Soft => 0,
        ForgetMode::Hard => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_planner::EncodeOpEdge;

    fn encode_op() -> EncodeOp {
        EncodeOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            context_id: brain_core::ContextId(42),
            kind: brain_core::MemoryKind::Episodic,
            text: "hello".into(),
            vector: [0.0; brain_embed::VECTOR_DIM],
            salience_initial: 0.5,
            fingerprint: [0x11; 16],
            edges: vec![EncodeOpEdge {
                target: brain_core::MemoryId::from(7u128),
                kind: brain_core::EdgeKind::Caused,
                weight: 0.5,
            }],
            deduplicate: false,
            content_hash: [0u8; 32],
            agent_id: brain_core::AgentId::default(),
        }
    }

    #[test]
    fn encode_hash_is_deterministic() {
        let a = hash_encode_request(&encode_op());
        let b = hash_encode_request(&encode_op());
        assert_eq!(a, b);
    }

    #[test]
    fn encode_hash_changes_on_text_change() {
        let a = hash_encode_request(&encode_op());
        let mut op = encode_op();
        op.text = "HELLO".into();
        let b = hash_encode_request(&op);
        assert_ne!(a, b);
    }

    #[test]
    fn encode_hash_excludes_request_id() {
        // Different request_ids must hash to the same value.
        let mut op_a = encode_op();
        let mut op_b = encode_op();
        op_a.request_id = brain_core::RequestId::from([1u8; 16]);
        op_b.request_id = brain_core::RequestId::from([99u8; 16]);
        assert_eq!(hash_encode_request(&op_a), hash_encode_request(&op_b));
    }

    #[test]
    fn forget_hash_is_deterministic() {
        let op = ForgetOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            memory_id: brain_core::MemoryId::from(7u128),
            mode: ForgetMode::Soft,
            agent_id: brain_core::AgentId::default(),
        };
        let a = hash_forget_request(&op);
        let b = hash_forget_request(&op);
        assert_eq!(a, b);
    }

    #[test]
    fn forget_hash_changes_on_mode_change() {
        let mut op = ForgetOp {
            request_id: brain_core::RequestId::from([1u8; 16]),
            memory_id: brain_core::MemoryId::from(7u128),
            mode: ForgetMode::Soft,
            agent_id: brain_core::AgentId::default(),
        };
        let soft = hash_forget_request(&op);
        op.mode = ForgetMode::Hard;
        let hard = hash_forget_request(&op);
        assert_ne!(soft, hard);
    }
}
