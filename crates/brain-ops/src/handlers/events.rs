//! Typed-graph-event emission helper shared across all wire handlers
//! that produce opaque-body events (entity / statement / relation /
//! schema / extractor-admin).
//!
//! Previously this helper lived inside `handlers/entity.rs` and was
//! imported by the other typed-graph handlers — an awkward ownership
//! arrangement that perpetuated the "entity owns the cross-handler
//! event emission" pattern. Promoted to its own module so the
//! per-handler files only contain handler-specific code.
//!
//! ## Why this still exists post-Wave-4
//!
//! Substrate writes get WAL coverage through their *write* records
//! (`wal_map.rs` maps each substrate `Phase` to a typed `WalPayload`).
//! Typed-graph writes (UpsertEntity / UpsertStatement / UpsertRelation /
//! UpsertSchema / MergeEntities / Tombstone(E/S/R) / Supersede(S/R))
//! don't yet have typed `WalPayload` variants — they ship via an
//! opaque `PhaseBody(PhaseBodyRecord)` carrier. Until `wal_map.rs`
//! grows arms for each typed-graph `Phase`, typed-graph events get WAL
//! coverage through this **event-side** path
//! (`OpsContext::publish_graph`).
//!
//! Once `wal_map.rs` is extended, this helper becomes redundant —
//! `submit::publish_events_for` (which bus-publishes substrate events
//! from `phase_to_envelope`) can also publish typed-graph events, and
//! WAL recovery reconstructs them from the write records. That's the
//! future S-11.

use brain_core::MemoryId;
use brain_protocol::envelope::response::EventType;
use brain_protocol::GraphEventPayload;

use crate::context::OpsContext;
use crate::subscribe::EventEnvelope;

/// Emit a opaque-body event onto the EventBus. Substrate fields
/// are zero-filled. Called post-commit by every
/// typed-graph handler that mutates state.
///
/// Routes through [`OpsContext::publish_graph`] so the event is
/// **also** WAL-recorded — letting subscribe-replay (`--start-lsn`)
/// reconstruct typed-graph events the same way it reconstructs
/// substrate events. The WAL append is post-commit (redb is the
/// source of truth for typed-graph state); a crash between commit and
/// WAL append loses the subscribe event for that op, not the
/// typed-graph data.
pub(crate) async fn emit_graph_event(
    ctx: &OpsContext,
    event_type: EventType,
    payload: GraphEventPayload,
    timestamp_unix_nanos: u64,
) {
    // Stamp the writer's bound agent on the envelope so the
    // subscribe `agents` filter routes typed-graph events the same
    // way it routes substrate events. Without this, a
    // schema-on subscriber filtering for "my agent" would silently
    // miss every typed-graph event.
    let agent_id = ctx.executor.writer.agent_id();
    let Some(kind) = wal_kind_for_event(&payload) else {
        // Variants without a WAL record kind: bus-only publish.
        let envelope = EventEnvelope {
            lsn: 0,
            event_type,
            memory_id: MemoryId::NULL,
            context_id: brain_core::ContextId::default(),
            kind: brain_core::MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos,
            text: None,
            graph_payload: Some(payload),
            edge_payload: None,
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
            agent_id,
        };
        let _ = ctx.events.publish(envelope);
        return;
    };
    ctx.publish_graph(kind, payload, agent_id, move |lsn, payload| EventEnvelope {
        lsn,
        event_type,
        memory_id: MemoryId::NULL,
        context_id: brain_core::ContextId::default(),
        kind: brain_core::MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos,
        text: None,
        graph_payload: Some(payload),
        edge_payload: None,
        stage_kind: None,
        stage_outcome: None,
        stage_payload: None,
        agent_id,
    })
    .await;
}

/// Map a [`GraphEventPayload`] variant to its WAL record kind so
/// subscribe-replay can decode the body back into the matching
/// variant. `EntityRenamed` / `EntityUnmerged` collapse onto
/// `EntityUpdate` / `EntityMerge` respectively — recovery
/// distinguishes by rkyv-decoding the body, the kind byte is just
/// the lookup key.
fn wal_kind_for_event(
    payload: &GraphEventPayload,
) -> Option<brain_storage::wal::kinds::WalRecordKind> {
    use brain_storage::wal::kinds::WalRecordKind as K;
    use GraphEventPayload as P;
    Some(match payload {
        P::EntityCreated(_) => K::EntityCreate,
        P::EntityUpdated(_) | P::EntityRenamed(_) => K::EntityUpdate,
        P::EntityMerged(_) | P::EntityUnmerged(_) => K::EntityMerge,
        P::EntityTombstoned(_) => K::EntityTombstone,
        P::StatementCreated(_) => K::StatementCreate,
        P::StatementSuperseded(_) => K::StatementSupersede,
        P::StatementTombstoned(_) => K::StatementTombstone,
        P::RelationCreated(_) => K::RelationCreate,
        P::RelationSuperseded(_) => K::RelationSupersede,
        P::RelationTombstoned(_) => K::RelationTombstone,
        P::SchemaUpdated(_) => K::SchemaUpdate,
    })
}
