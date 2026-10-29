//! Shared ground for every handler that mutates the typed graph.
//!
//! ENTITY, STATEMENT, RELATION and SCHEMA all have to put an
//! opaque-body event on the bus once their write commits, and they all
//! do it the same way. The helper lived in `entity.rs` and the other
//! three imported it from there, which read as "entity owns typed-graph
//! event emission" — entity was only the first of the four to need it.
//!
//! The module boundary is "how a typed-graph write becomes an event a
//! subscriber can see", not "whichever typed-graph handler was written
//! first".
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
/// Routes through [`OpsContext::publish_notification`] so the event is
/// **also** WAL-recorded — letting subscribe-replay (`--start-lsn`)
/// reconstruct typed-graph events the same way it reconstructs
/// substrate events. The WAL append is post-commit (redb is the
/// source of truth for typed-graph state); a crash between commit and
/// WAL append loses the subscribe event for that op, not the
/// typed-graph data.
pub(super) async fn emit_graph_event(
    ctx: &OpsContext,
    event_type: EventType,
    payload: GraphEventPayload,
    timestamp_unix_nanos: u64,
) {
    // Stamp the writer's bound space on the envelope so the
    // subscribe `spaces` filter routes typed-graph events the same
    // way it routes substrate events. Without this, a
    // schema-on subscriber filtering for "my space" would silently
    // miss every typed-graph event.
    let space_id = ctx.executor.writer.space_id();
    let Some(kind) = wal_kind_for_event(&payload) else {
        // Variants without a WAL record kind: bus-only publish.
        let envelope = EventEnvelope {
            lsn: 0,
            event_type,
            memory_id: MemoryId::NULL,
            session_id: brain_core::SessionId::default(),
            kind: brain_core::MemoryKind::Episodic,
            salience: 0.0,
            timestamp_unix_nanos,
            text: None,
            graph_payload: Some(payload),
            edge_payload: None,
            stage_kind: None,
            stage_outcome: None,
            stage_payload: None,
            space_id,
            vector: None,
        };
        let _ = ctx.events.publish(envelope);
        return;
    };
    ctx.publish_notification(kind, payload, space_id, move |lsn, payload| EventEnvelope {
        lsn,
        event_type,
        memory_id: MemoryId::NULL,
        session_id: brain_core::SessionId::default(),
        kind: brain_core::MemoryKind::Episodic,
        salience: 0.0,
        timestamp_unix_nanos,
        text: None,
        graph_payload: Some(payload),
        edge_payload: None,
        stage_kind: None,
        stage_outcome: None,
        stage_payload: None,
        space_id,
        vector: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::SpaceId;
    use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::MetadataDb;
    use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
    use brain_protocol::{EntityCreatedEvent, EntityUnmergedEvent, SchemaUpdatedEvent};
    use brain_storage::wal::kinds::WalRecordKind as K;
    use std::sync::Arc;
    use GraphEventPayload as P;

    struct MockDispatcher;
    impl Dispatcher for MockDispatcher {
        fn embed(&self, _t: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok([0.0; VECTOR_DIM])
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(texts.iter().map(|_| [0.0; VECTOR_DIM]).collect())
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0xAB; 16]
        }
    }

    /// A context whose writer is bound to `space` — the value every
    /// emitted envelope is expected to carry.
    fn build_ctx(space: SpaceId) -> (tempfile::TempDir, OpsContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let metadata: SharedMetadataDb =
            Arc::new(MetadataDb::open(dir.path().join("meta.redb")).expect("open metadata"));
        let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).expect("hnsw");
        let writer = Arc::new(
            crate::writer::RealWriterHandle::new(metadata.clone(), hnsw_writer)
                .with_space_id(space),
        );
        let executor = ExecutorContext::new(
            Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
            shared,
            metadata,
            writer as Arc<dyn WriterHandle>,
        );
        let ctx = crate::test_support::ops_context_for_tests(executor, dir.path());
        (dir, ctx)
    }

    fn unmerged() -> EntityUnmergedEvent {
        EntityUnmergedEvent {
            restored_entity_id: [8u8; 16],
            from_survivor: [7u8; 16],
            audit_id: [9u8; 16],
        }
    }

    fn entity_created() -> GraphEventPayload {
        P::EntityCreated(EntityCreatedEvent {
            entity_id: [7u8; 16],
            entity_type_id: 1,
            canonical_name: "Priya".into(),
        })
    }

    /// Every payload variant, one of each, so a new variant has to be
    /// added here before the exhaustiveness checks below compile.
    fn one_of_each() -> Vec<GraphEventPayload> {
        use brain_protocol::{
            EntityMergedEvent, EntityRenamedEvent, EntityTombstonedEvent, EntityUpdatedEvent,
            RelationCreatedEvent, RelationSupersededEvent, RelationTombstonedEvent,
            StatementCreatedEvent, StatementSupersededEvent, StatementTombstonedEvent,
        };
        let id = [7u8; 16];
        vec![
            entity_created(),
            P::EntityUpdated(EntityUpdatedEvent {
                entity_id: id,
                entity_type_id: 1,
                canonical_name: "Priya".into(),
                embedding_version_changed: false,
            }),
            P::EntityRenamed(EntityRenamedEvent {
                entity_id: id,
                old_canonical_name: "Priya".into(),
                new_canonical_name: "Priya Patel".into(),
                old_moved_to_alias: true,
            }),
            P::EntityMerged(EntityMergedEvent {
                survivor: id,
                merged: [8u8; 16],
                audit_id: [9u8; 16],
                confidence: 0.9,
                statements_rerouted: 0,
                relations_rerouted: 0,
            }),
            P::EntityUnmerged(unmerged()),
            P::EntityTombstoned(EntityTombstonedEvent {
                entity_id: id,
                reason: "client".into(),
            }),
            P::StatementCreated(StatementCreatedEvent {
                statement_id: id,
                kind: 1,
                subject: [8u8; 16],
                predicate: "manages".into(),
                confidence: 0.9,
            }),
            P::StatementSuperseded(StatementSupersededEvent {
                old_statement_id: id,
                new_statement_id: [8u8; 16],
                chain_root: id,
            }),
            P::StatementTombstoned(StatementTombstonedEvent {
                statement_id: id,
                reason: "client".into(),
            }),
            P::RelationCreated(RelationCreatedEvent {
                relation_id: id,
                relation_type: "mentors".into(),
                from: [8u8; 16],
                to: [9u8; 16],
            }),
            P::RelationSuperseded(RelationSupersededEvent {
                old_relation_id: id,
                new_relation_id: [8u8; 16],
            }),
            P::RelationTombstoned(RelationTombstonedEvent {
                relation_id: id,
                reason: "client".into(),
            }),
            P::SchemaUpdated(SchemaUpdatedEvent {
                namespace: "acme".into(),
                from_version: 1,
                to_version: 2,
                backward_compatible: true,
            }),
        ]
    }

    /// The bus-only branch in `emit_graph_event` is the no-op path for a
    /// payload with no WAL record kind — and today nothing takes it. Every
    /// variant maps to a kind, so every typed-graph event is WAL-recorded
    /// and subscribe-replay can reconstruct all of them. A new variant
    /// added without a mapping arm would silently become bus-only: durable
    /// for the writer, invisible to a replaying subscriber.
    #[test]
    fn every_payload_variant_has_a_wal_kind() {
        for payload in one_of_each() {
            assert!(
                wal_kind_for_event(&payload).is_some(),
                "{payload:?} has no WAL kind and would publish bus-only",
            );
        }
    }

    /// `publish_notification` debug-asserts that the kind it is handed
    /// carries an opaque body — the framing it writes (`space_id || CBOR`)
    /// is only decodable for those kinds. A mapping arm pointing at a
    /// substrate kind would panic every debug build at emit time.
    #[test]
    fn every_mapped_kind_carries_an_opaque_body() {
        for payload in one_of_each() {
            let kind = wal_kind_for_event(&payload).expect("mapped");
            assert!(
                kind.has_opaque_body(),
                "{payload:?} maps to {kind:?}, which publish_notification refuses",
            );
        }
    }

    /// Two pairs share a kind byte on purpose: recovery distinguishes
    /// them by rkyv-decoding the body, so the byte is only a lookup key.
    /// Pinned because it reads like a copy-paste slip.
    #[test]
    fn renamed_and_unmerged_deliberately_share_a_kind() {
        let renamed = one_of_each()
            .into_iter()
            .find(|p| matches!(p, P::EntityRenamed(_)))
            .expect("renamed");
        let unmerged = P::EntityUnmerged(unmerged());
        assert_eq!(wal_kind_for_event(&renamed), Some(K::EntityUpdate));
        assert_eq!(wal_kind_for_event(&unmerged), Some(K::EntityMerge));

        // …and no OTHER pair collides. Distinct kinds for the remaining
        // variants keep replay's lookup unambiguous.
        let kinds: Vec<K> = one_of_each()
            .iter()
            .filter(|p| !matches!(p, P::EntityRenamed(_) | P::EntityUnmerged(_)))
            .map(|p| wal_kind_for_event(p).expect("mapped"))
            .collect();
        let mut sorted: Vec<u8> = kinds.iter().map(|k| *k as u8).collect();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), kinds.len(), "kind collision in {kinds:?}");
    }

    /// The failure path the helper is built to swallow: nobody is
    /// listening. `EventBus::publish` drops the send error, so emitting
    /// into an empty bus completes normally rather than failing the
    /// handler whose write already committed.
    #[tokio::test]
    async fn emitting_with_no_subscriber_is_a_silent_no_op() {
        let (_dir, ctx) = build_ctx(SpaceId(uuid::Uuid::from_u128(1)));
        assert_eq!(ctx.events.subscriber_count(), 0);
        emit_graph_event(&ctx, EventType::EntityCreated, entity_created(), 42).await;
    }

    /// Every envelope carries the writer's bound space, so the subscribe
    /// `spaces` filter routes typed-graph events the same way it routes
    /// substrate ones. Without the stamp the field would be the nil
    /// space and a subscriber filtering for its own space would match
    /// none of its own typed-graph events.
    #[tokio::test]
    async fn emitted_envelope_carries_the_writers_space() {
        let space = SpaceId(uuid::Uuid::from_u128(0xBEEF));
        let (_dir, ctx) = build_ctx(space);
        let mut rx = ctx.events.receiver();

        emit_graph_event(&ctx, EventType::EntityCreated, entity_created(), 42).await;

        let env = rx.try_recv().expect("the event reaches a live subscriber");
        assert_eq!(env.space_id, space);
        assert_ne!(
            env.space_id,
            SpaceId(uuid::Uuid::nil()),
            "a nil space would make the spaces filter drop this event",
        );
        assert_eq!(env.event_type, EventType::EntityCreated);
        assert_eq!(env.timestamp_unix_nanos, 42);
        assert!(matches!(env.graph_payload, Some(P::EntityCreated(_))));
    }

    /// Substrate fields are zero-filled on purpose: a typed-graph event
    /// identifies its subject through `graph_payload`, and a non-null
    /// `memory_id` here would make a memory-id filter match events that
    /// have nothing to do with that memory.
    #[tokio::test]
    async fn substrate_fields_are_zero_filled() {
        let (_dir, ctx) = build_ctx(SpaceId(uuid::Uuid::from_u128(1)));
        let mut rx = ctx.events.receiver();

        emit_graph_event(
            &ctx,
            EventType::SchemaUpdated,
            P::SchemaUpdated(SchemaUpdatedEvent {
                namespace: "acme".into(),
                from_version: 1,
                to_version: 2,
                backward_compatible: true,
            }),
            7,
        )
        .await;

        let env = rx.try_recv().expect("event published");
        assert_eq!(env.memory_id, MemoryId::NULL);
        assert_eq!(env.salience, 0.0);
        assert!(env.text.is_none());
        assert!(env.vector.is_none());
        assert!(env.edge_payload.is_none());
        assert!(env.stage_kind.is_none());
    }

    /// With no WAL sink wired the helper still publishes — the bus mints
    /// the LSN instead. A shard without a sink (every unit-test context,
    /// and any shard before its sink is attached) must not lose events.
    #[tokio::test]
    async fn without_a_wal_sink_the_bus_mints_the_lsn() {
        let (_dir, ctx) = build_ctx(SpaceId(uuid::Uuid::from_u128(1)));
        let mut rx = ctx.events.receiver();

        emit_graph_event(&ctx, EventType::EntityCreated, entity_created(), 1).await;
        emit_graph_event(&ctx, EventType::EntityCreated, entity_created(), 2).await;

        let first = rx.try_recv().expect("first event");
        let second = rx.try_recv().expect("second event");
        assert!(
            second.lsn > first.lsn,
            "bus-minted LSNs must advance: {} then {}",
            first.lsn,
            second.lsn,
        );
    }
}
