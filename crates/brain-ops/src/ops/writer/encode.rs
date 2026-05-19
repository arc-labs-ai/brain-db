//! ENCODE handler — splits memory-row write + edge insertion +
//! idempotency stamp into a single redb txn; HNSW insert + change-feed
//! event follow the durability barrier.

use std::sync::atomic::Ordering;

use brain_core::MemoryId;
use brain_metadata::tables::edge::{
    self, derived_by, origin, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE,
};
use brain_metadata::tables::fingerprint::{
    fingerprint_key, FingerprintEntry, FINGERPRINTS_TABLE,
};
use brain_metadata::tables::idempotency::{IdempotencyEntry, IDEMPOTENCY_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_planner::{EdgeOutcome, EncodeAck, EncodeOp, WriterError};
use brain_protocol::response::EventType;
use brain_storage::wal::kinds::WalRecordKind;
use brain_storage::wal::payload::{
    EdgePayload as WalEdgePayload, EncodePayload as WalEncodePayload, WalPayload,
};
use brain_storage::wal::record::{Lsn, WalRecord};
use redb::ReadableTable;

use crate::idempotency::{
    decode_encode_payload, encode_encode_payload, hash_encode_request, RESPONSE_KIND_ENCODE,
};
use crate::subscribe::EventEnvelope;

use super::{hex_short, now_unix_nanos, RealWriterHandle};

pub(super) async fn do_encode(
    writer: &RealWriterHandle,
    op: EncodeOp,
) -> Result<EncodeAck, WriterError> {
    let request_hash = hash_encode_request(&op);
    let request_id_bytes: [u8; 16] = op.request_id.into();

    // ── Idempotency lookup (read txn). ────────────────────────────
    {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("idempotency read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(IDEMPOTENCY_TABLE)
            .map_err(|e| WriterError::Internal(format!("open IDEMPOTENCY_TABLE: {e:?}")))?;
        if let Some(access) = table
            .get(request_id_bytes)
            .map_err(|e| WriterError::Internal(format!("idempotency get: {e:?}")))?
        {
            let prior = access.value();
            if prior.request_hash != request_hash {
                return Err(WriterError::Conflict(format!(
                    "encode request_id={} hash mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            if prior.response_kind != RESPONSE_KIND_ENCODE {
                return Err(WriterError::Conflict(format!(
                    "encode request_id={} kind mismatch",
                    hex_short(&request_id_bytes)
                )));
            }
            let (memory_id, edge_outcomes) = decode_encode_payload(&prior.response_payload)
                .map_err(|e| WriterError::Internal(format!("decode encode payload: {e}")))?;
            let inserted = edge_outcomes
                .iter()
                .filter(|o| matches!(o, EdgeOutcome::Inserted))
                .count() as u32;
            return Ok(EncodeAck {
                memory_id,
                edge_results: edge_outcomes,
                replayed: true,
                // Idempotency replay never sets dedup. Same response
                // shape as the original; if the original was a dedup
                // hit, the cached payload would have recorded that
                // (Phase 8.dedup will roundtrip the dedup state via
                // the cached payload too — for now the simpler
                // contract: dedup signal lives only on the fresh
                // path).
                was_deduplicated: false,
                // Surface the persisted LSN so a client chaining
                // `subscribe --start-lsn=lsn+1` on retry resumes from
                // the same position the original write reached. A
                // zero stored value means the entry predates this
                // field or the original write went through a no-WAL
                // path (tests / dedup hit); surface None there so the
                // client falls back to its tail.
                lsn: if prior.lsn == 0 { None } else { Some(prior.lsn) },
                edges_out_count: inserted,
                created_at_unix_nanos: prior.created_at_unix_nanos,
            });
        }
    }

    // ── Fingerprint dedup lookup (spec §07/07 §6). ────────────────
    //
    // Only consult the `fingerprints` table when the caller asked
    // for dedup AND attached no edges. Edges-on-dedup is an
    // ambiguous combination (apply edges to the existing memory? or
    // skip them?); v1 keeps it simple — if any edges are present we
    // ignore `deduplicate` and take the normal fresh-slot path.
    // Callers wanting both can issue ENCODE + LINK as two ops.
    //
    // Eviction invariant (spec §07/07 §6.3 option b): FORGET /
    // reclamation remove the fingerprint row in the same txn as
    // the tombstone, so any row we read here points at an Active
    // memory by construction. No re-check.
    if op.deduplicate && op.edges.is_empty() {
        let key = fingerprint_key(op.agent_id, op.context_id, &op.content_hash);
        let dedup_hit: Option<MemoryId> = {
            let db = writer.metadata.lock();
            let rtxn = db
                .read_txn()
                .map_err(|e| WriterError::Internal(format!("dedup read_txn: {e:?}")))?;
            // Table may not exist yet on a fresh shard's first dedup
            // request — that's not an error, just a guaranteed miss.
            match rtxn.open_table(FINGERPRINTS_TABLE) {
                Ok(table) => table
                    .get(key)
                    .map_err(|e| WriterError::Internal(format!("dedup get: {e:?}")))?
                    .map(|access| access.value().memory_id()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(WriterError::Internal(format!("dedup open: {e:?}"))),
            }
        };
        if let Some(memory_id) = dedup_hit {
            // Stamp idempotency so a retry of this exact dedup
            // request returns the same response (without re-doing
            // the fingerprint lookup or risking a different MemoryId
            // if the fingerprint table changed between attempts).
            let response_payload = encode_encode_payload(memory_id, &[]);
            let created_at = now_unix_nanos();
            {
                let mut db = writer.metadata.lock();
                let wtxn = db
                    .write_txn()
                    .map_err(|e| WriterError::Internal(format!("dedup idem write_txn: {e:?}")))?;
                {
                    let mut idem_t = wtxn.open_table(IDEMPOTENCY_TABLE).map_err(|e| {
                        WriterError::Internal(format!("dedup open IDEMPOTENCY: {e:?}"))
                    })?;
                    // Dedup hit doesn't append a WAL record — no fresh
                    // memory was created — so the cached LSN is 0
                    // ("unknown"). Retries surface lsn=None and the
                    // client falls back to its tail position.
                    let entry = IdempotencyEntry::new(
                        RESPONSE_KIND_ENCODE,
                        Some(memory_id.to_be_bytes()),
                        response_payload,
                        request_hash,
                        created_at,
                        0,
                    );
                    idem_t.insert(request_id_bytes, entry).map_err(|e| {
                        WriterError::Internal(format!("dedup idem insert: {e:?}"))
                    })?;
                }
                wtxn.commit()
                    .map_err(|e| WriterError::Internal(format!("dedup idem commit: {e:?}")))?;
            }
            return Ok(EncodeAck {
                memory_id,
                edge_results: vec![],
                replayed: false,
                was_deduplicated: true,
                // Dedup hit reuses the original memory; no fresh
                // WAL record on this op.
                lsn: None,
                edges_out_count: 0,
                created_at_unix_nanos: created_at,
            });
        }
    }

    // ── Mint slot + MemoryId. ─────────────────────────────────────
    let slot = writer.next_slot.fetch_add(1, Ordering::Relaxed);
    let memory_id = MemoryId::pack(writer.shard_id, slot, /* version */ 1);
    let created_at = now_unix_nanos();

    // ── Compute edge outcomes against existing memories. ──────────
    // (Read txn before the write txn; minimises lock duration.)
    let edge_outcomes: Vec<EdgeOutcome> = {
        let db = writer.metadata.lock();
        let rtxn = db
            .read_txn()
            .map_err(|e| WriterError::Internal(format!("edges read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WriterError::Internal(format!("edges open_table: {e:?}")))?;
        op.edges
            .iter()
            .map(|edge| {
                let exists = table
                    .get(edge.target.to_be_bytes())
                    .ok()
                    .flatten()
                    .is_some();
                if exists {
                    EdgeOutcome::Inserted
                } else {
                    EdgeOutcome::TargetMissing
                }
            })
            .collect()
    };

    // ── Apply: metadata row + idempotency entry + edges in ONE write txn. ─
    let response_payload = encode_encode_payload(memory_id, &edge_outcomes);

    // ── WAL append (spec §05/07 durability barrier). ─────────────
    // Build the typed payload once we have everything: response
    // bytes, request hash, edge outcomes. Append BEFORE the redb
    // commit so a crash between the two is recoverable by replay.
    // When `wal_sink` is None (test wiring), skip — the legacy path
    // mints the LSN via the EventBus.
    let wal_lsn: Option<Lsn> = if let Some(sink) = &writer.wal_sink {
        let wal_edges: Vec<WalEdgePayload> = op
            .edges
            .iter()
            .zip(edge_outcomes.iter())
            .filter(|(_, o)| matches!(o, EdgeOutcome::Inserted))
            .map(|(e, _)| WalEdgePayload {
                source: memory_id,
                target: e.target,
                kind: e.kind,
                weight: e.weight,
                origin: brain_core::EdgeOrigin::Explicit,
            })
            .collect();
        let payload = WalPayload::Encode(WalEncodePayload {
            memory_id,
            request_id: op.request_id,
            agent_id: op.agent_id,
            context_id: op.context_id,
            kind: op.kind,
            salience_initial: op.salience_initial,
            embedding_model_fp: op.fingerprint,
            text: op.text.clone(),
            vector: op.vector.to_vec(),
            edges: wal_edges,
            request_hash,
            response_payload: response_payload.clone(),
            deduplicate: op.deduplicate,
        });
        let agent_bytes: [u8; 16] = op.agent_id.into();
        let agent_id_lo64 = u64::from_be_bytes(agent_bytes[8..16].try_into().unwrap());
        let record = WalRecord::from_typed(
            Lsn(0),
            /* flags */ 0,
            created_at,
            agent_id_lo64,
            &payload,
        );
        // Sanity: framing assigned the discriminator we expect.
        debug_assert_eq!(record.kind, WalRecordKind::Encode);
        let lsn = sink
            .append(record)
            .await
            .map_err(|e| WriterError::Internal(format!("wal append: {e}")))?;
        Some(lsn)
    } else {
        None
    };

    {
        let mut db = writer.metadata.lock();
        let wtxn = db
            .write_txn()
            .map_err(|e| WriterError::Internal(format!("write_txn: {e:?}")))?;

        // First open all the tables we'll touch.
        let mut new_memory_outgoing: u32 = 0;
        let target_count_bumps: Vec<MemoryId> = op
            .edges
            .iter()
            .zip(edge_outcomes.iter())
            .filter(|(_, o)| matches!(o, EdgeOutcome::Inserted))
            .map(|(e, _)| e.target)
            .collect();

        {
            let mut edges_out_t = wtxn
                .open_table(EDGES_OUT_TABLE)
                .map_err(|e| WriterError::Internal(format!("open EDGES_OUT: {e:?}")))?;
            let mut edges_in_t = wtxn
                .open_table(EDGES_IN_TABLE)
                .map_err(|e| WriterError::Internal(format!("open EDGES_IN: {e:?}")))?;

            // Insert edges whose target exists (Inserted outcomes).
            for (edge, outcome) in op.edges.iter().zip(edge_outcomes.iter()) {
                if !matches!(outcome, EdgeOutcome::Inserted) {
                    continue;
                }
                let data = EdgeData::new(
                    edge.weight,
                    origin::EXPLICIT,
                    derived_by::CLIENT,
                    created_at,
                );
                edge::link(
                    &mut edges_out_t,
                    &mut edges_in_t,
                    memory_id,
                    edge.kind,
                    edge.target,
                    &data,
                )
                .map_err(|e| WriterError::Internal(format!("edge::link: {e:?}")))?;
                new_memory_outgoing += 1;
            }
        }

        // Bump edges_in_count on the targets.
        {
            let mut memories_t = wtxn
                .open_table(MEMORIES_TABLE)
                .map_err(|e| WriterError::Internal(format!("open MEMORIES_TABLE: {e:?}")))?;
            for target_id in &target_count_bumps {
                let key = target_id.to_be_bytes();
                let prior = memories_t
                    .get(key)
                    .map_err(|e| WriterError::Internal(format!("memories get: {e:?}")))?
                    .map(|access| access.value());
                if let Some(mut meta) = prior {
                    meta.edges_in_count = meta.edges_in_count.saturating_add(1);
                    memories_t
                        .insert(key, meta)
                        .map_err(|e| WriterError::Internal(format!("memories update: {e:?}")))?;
                }
            }

            // Insert the new memory row with the right outgoing count.
            let mut meta = MemoryMetadata::new_active(
                memory_id,
                op.agent_id,
                op.context_id,
                slot,
                /* slot_version */ 1,
                op.kind,
                op.fingerprint,
                op.salience_initial,
                /* text_size */ op.text.len() as u32,
                created_at,
            );
            meta.edges_out_count = new_memory_outgoing;
            // Stamp the dedup back-reference on rows whose ENCODE
            // opted in. Forget reads this to evict the matching
            // FINGERPRINTS entry in the same write txn.
            if op.deduplicate {
                meta.content_hash = Some(op.content_hash);
            }
            // Stamp the WAL position so future RECALLs can answer
            // "what LSN was this written at?" without going back to
            // the WAL. `wal_lsn` is `Some` when the shard has a sink
            // wired (production); `None` in tests where the sink
            // mints synthetic LSNs from the event bus — in that case
            // we leave encoded_at_lsn=0 (the default) which the wire
            // surfaces as "unknown."
            if let Some(lsn) = wal_lsn {
                meta.encoded_at_lsn = lsn.raw();
            }
            memories_t
                .insert(memory_id.to_be_bytes(), meta)
                .map_err(|e| WriterError::Internal(format!("memories insert: {e:?}")))?;
        }
        // Couple text to the memory row inside the same write txn:
        // a later RECALL --include-text reads from this table and
        // must see the row atomically with the memory metadata.
        {
            let mut texts_t = wtxn
                .open_table(TEXTS_TABLE)
                .map_err(|e| WriterError::Internal(format!("open TEXTS_TABLE: {e:?}")))?;
            texts_t
                .insert(memory_id.to_be_bytes(), op.text.as_bytes())
                .map_err(|e| WriterError::Internal(format!("texts insert: {e:?}")))?;
        }
        {
            let mut idem_t = wtxn
                .open_table(IDEMPOTENCY_TABLE)
                .map_err(|e| WriterError::Internal(format!("open IDEMPOTENCY_TABLE: {e:?}")))?;
            let entry = IdempotencyEntry::new(
                RESPONSE_KIND_ENCODE,
                Some(memory_id.to_be_bytes()),
                response_payload,
                request_hash,
                created_at,
                wal_lsn.map(|l| l.raw()).unwrap_or(0),
            );
            idem_t
                .insert(request_id_bytes, entry)
                .map_err(|e| WriterError::Internal(format!("idempotency insert: {e:?}")))?;
        }

        // ── Fingerprint dedup index — record this Active memory so
        //    future ENCODE calls with deduplicate=true can hit it.
        //    Spec §07/07 §6. Only inserted when the caller opted in,
        //    so substrate-mode (no dedup) keeps zero overhead.
        if op.deduplicate {
            let key = fingerprint_key(op.agent_id, op.context_id, &op.content_hash);
            let entry = FingerprintEntry::new(memory_id, created_at);
            let mut fp_t = wtxn
                .open_table(FINGERPRINTS_TABLE)
                .map_err(|e| WriterError::Internal(format!("open FINGERPRINTS: {e:?}")))?;
            fp_t.insert(key, entry)
                .map_err(|e| WriterError::Internal(format!("fingerprints insert: {e:?}")))?;
        }

        wtxn.commit()
            .map_err(|e| WriterError::Internal(format!("commit: {e:?}")))?;
    }

    // ── HNSW insert (after the durability barrier). ───────────────
    writer
        .hnsw_writer
        .lock()
        .insert(memory_id, &op.vector)
        .map_err(|e| WriterError::Internal(format!("hnsw insert: {e:?}")))?;

    // ── Change-feed (sub-task 7.10). ─────────────────────────────
    // When the WAL stamped the record, the published event carries
    // the same LSN so subscribe-replay and live tail line up; otherwise
    // the bus's allocator stamps a synthetic LSN (test-only path).
    writer.publish_with_lsn(
        EventEnvelope {
            lsn: 0,
            event_type: EventType::Encoded,
            memory_id,
            context_id: op.context_id,
            kind: op.kind,
            salience: op.salience_initial,
            timestamp_unix_nanos: created_at,
            text: Some(op.text.clone()),
            knowledge_payload: None,
            agent_id: op.agent_id,
        },
        wal_lsn.map(|l| l.raw()),
    );

    let edges_out_count = edge_outcomes
        .iter()
        .filter(|o| matches!(o, EdgeOutcome::Inserted))
        .count() as u32;
    Ok(EncodeAck {
        memory_id,
        edge_results: edge_outcomes,
        replayed: false,
        // Wired by task #52 — placeholder false keeps the build
        // green while the lookup-then-insert path is added.
        was_deduplicated: false,
        lsn: wal_lsn.map(|l| l.raw()),
        edges_out_count,
        created_at_unix_nanos: created_at,
    })
}
