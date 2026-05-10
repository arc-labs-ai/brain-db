//! Integration tests for `brain-core`'s public API.
//!
//! As foundational types stabilise, these tests will grow. For now: a smoke
//! test that the public surface is reachable and behaves sanely.

use brain_core::{
    AgentId, ContextId, Edge, EdgeKind, EdgeOrigin, Error, Memory, MemoryId, MemoryKind, RequestId,
    Salience, TxnId,
};

#[test]
fn public_types_construct() {
    let _ = AgentId::new();
    let _ = ContextId::DEFAULT;
    let _ = ContextId(42);
    let _ = RequestId::new();
    let _ = TxnId::new();
    let _ = Salience::new(0.7);
    let _ = MemoryKind::Episodic;
    let _ = EdgeKind::Caused;
    let _ = EdgeOrigin::Explicit;
}

#[test]
fn memory_id_is_routable_after_packing() {
    let id = MemoryId::pack(3, 1234, 7);
    assert_eq!(id.shard(), 3);
    assert_eq!(id.slot(), 1234);
    assert_eq!(id.version(), 7);
}

#[test]
fn memory_can_be_constructed() {
    let m = Memory {
        id: MemoryId::pack(0, 1, 0),
        agent: AgentId::new(),
        context: ContextId(0),
        kind: MemoryKind::Episodic,
        salience: Salience::default(),
        text: Some("hello".into()),
        created_at_unix_ms: 0,
        last_accessed_at_unix_ms: 0,
    };
    assert_eq!(m.salience.raw(), 0.5);
    assert_eq!(m.text.as_deref(), Some("hello"));
}

#[test]
fn edges_carry_kind_and_endpoints() {
    let e = Edge {
        source: MemoryId::pack(0, 1, 0),
        target: MemoryId::pack(0, 2, 0),
        kind: EdgeKind::FollowedBy,
        weight: 1.0,
        origin: EdgeOrigin::Explicit,
        created_at_unix_nanos: 0,
    };
    assert_eq!(e.kind, EdgeKind::FollowedBy);
    assert_ne!(e.source, e.target);
}

#[test]
fn errors_render_as_strings() {
    let e: Error = Error::NotFound;
    assert_eq!(e.to_string(), "not found");
}
