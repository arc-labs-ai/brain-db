//! Index rebuild worker (phase 22.6).
//!
//! Recovers the per-shard tantivy indexes from the authoritative
//! redb tables when 22.1's `TantivyShard::open` reports
//! `IndexStatus::NeedsRebuild` (corrupt segments, missing files,
//! schema-version mismatch). Implements the algorithm in
//! `spec/26_knowledge_storage/01_tantivy_layout.md` §5.
//!
//! ## v1 simplifications
//!
//! - **Memory text rebuild emits a valid empty index.** The
//!   substrate's `MEMORIES_TABLE` stores `text_size` but not the
//!   text itself (which only lives on the ENCODE wire path and
//!   the WAL frame); rebuild cannot reconstruct text content
//!   from authoritative storage. v1 produces a fresh empty
//!   index with the correct schema + payload so subsequent
//!   writes work; operators re-ingest existing memories from
//!   their own source-of-truth. Full content-aware rebuild
//!   lands post-v1 (§27/07).
//! - **Statement text rebuild is content-complete** because
//!   `StatementMetadata.object_blob` carries the encoded
//!   `StatementObject`, and `subject_name` / `predicate_name`
//!   are reconstructible via the entity + predicate tables.
//! - **Startup-only.** Rebuild assumes the live writer is not
//!   running (i.e. the shard hasn't reached the
//!   `spawn_*_text_indexer_local` step yet). Hot rebuild is
//!   post-v1.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use brain_core::knowledge::{StatementKind, StatementObject, StatementValue, SubjectRef};
use brain_index::{
    build_analyzer, memory_text_schema, schema_payload_json, statements_schema, LexicalScope,
    BRAIN_TOKENIZER_NAME,
};
use brain_metadata::tables::knowledge::entity::ENTITIES_TABLE;
use brain_metadata::tables::knowledge::predicate::PREDICATES_TABLE;
use brain_metadata::tables::knowledge::statement::{decode_object, STATEMENTS_TABLE};
use brain_metadata::MetadataDb;
use redb::ReadableTable;
use tantivy::{Index, IndexWriter, TantivyDocument, TantivyError};
use thiserror::Error;

const REBUILD_SUFFIX: &str = ".rebuild";
const OLD_SUFFIX: &str = ".old";
const COMMIT_CHUNK: usize = 1024;

#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub scope: LexicalScope,
    pub rows_processed: u64,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum RebuildError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy: {0}")]
    Tantivy(#[from] TantivyError),
    #[error("metadata: {0}")]
    Metadata(String),
}

/// Rebuild `memory_text.tantivy/` under `shard_dir`. v1 produces
/// an empty valid index; the substrate path stays unaffected.
pub fn rebuild_memory_text(
    shard_dir: &Path,
    _metadata: &MetadataDb,
) -> Result<RebuildReport, RebuildError> {
    let live = shard_dir.join("memory_text.tantivy");
    rebuild_with(
        &live,
        memory_text_schema(),
        LexicalScope::MemoryText,
        |_, _| {
            // §27/02 §2 — memory text isn't in redb; rebuild yields
            // an empty but valid index. Documented above.
            Ok(0)
        },
    )
}

/// Rebuild `statements.tantivy/` under `shard_dir`. Iterates
/// `STATEMENTS_TABLE` and joins against `ENTITIES_TABLE` (subject
/// canonical_name) + `PREDICATES_TABLE` (predicate name), then
/// projects the row to the schema defined in §26/01 §2.
pub fn rebuild_statements(
    shard_dir: &Path,
    metadata: &MetadataDb,
) -> Result<RebuildReport, RebuildError> {
    let live = shard_dir.join("statements.tantivy");
    rebuild_with(
        &live,
        statements_schema(),
        LexicalScope::StatementText,
        |writer, rebuild_index| iterate_statements(writer, rebuild_index, metadata),
    )
}

fn rebuild_with<F>(
    live: &Path,
    schema: tantivy::schema::Schema,
    scope: LexicalScope,
    iterate_and_index: F,
) -> Result<RebuildReport, RebuildError>
where
    F: FnOnce(&mut IndexWriter, &Index) -> Result<u64, RebuildError>,
{
    let started = Instant::now();
    let rebuild_dir = path_with_suffix(live, REBUILD_SUFFIX);
    let old_dir = path_with_suffix(live, OLD_SUFFIX);

    // Step 1: truncate stale rebuild dir.
    if rebuild_dir.exists() {
        std::fs::remove_dir_all(&rebuild_dir)?;
    }
    std::fs::create_dir_all(&rebuild_dir)?;

    // Steps 2-3: open + register tokenizer.
    let index = Index::create_in_dir(&rebuild_dir, schema)?;
    index
        .tokenizers()
        .register(BRAIN_TOKENIZER_NAME, build_analyzer());

    // Steps 4-6: iterate, write, final commit + stamp payload.
    let mut writer = index.writer_with_num_threads(1, 50_000_000)?;
    let rows = iterate_and_index(&mut writer, &index)?;
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&schema_payload_json());
    prepared.commit()?;
    drop(writer);
    drop(index);

    // Steps 7-8: atomic swap.
    if live.exists() {
        if old_dir.exists() {
            std::fs::remove_dir_all(&old_dir)?;
        }
        std::fs::rename(live, &old_dir)?;
    }
    std::fs::rename(&rebuild_dir, live)?;
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir)?;
    }

    Ok(RebuildReport {
        scope,
        rows_processed: rows,
        duration: started.elapsed(),
    })
}

fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut buf = p.as_os_str().to_owned();
    buf.push(suffix);
    PathBuf::from(buf)
}

// ---------------------------------------------------------------------------
// Statement iteration with entity + predicate joins.
// ---------------------------------------------------------------------------

fn iterate_statements(
    writer: &mut IndexWriter,
    index: &Index,
    metadata: &MetadataDb,
) -> Result<u64, RebuildError> {
    let schema = index.schema();
    let statement_id_field = schema
        .get_field("statement_id")
        .map_err(|e| RebuildError::Metadata(format!("statement_id: {e}")))?;
    let subject_name_field = schema
        .get_field("subject_name")
        .map_err(|e| RebuildError::Metadata(format!("subject_name: {e}")))?;
    let predicate_name_field = schema
        .get_field("predicate_name")
        .map_err(|e| RebuildError::Metadata(format!("predicate_name: {e}")))?;
    let predicate_id_field = schema
        .get_field("predicate_id")
        .map_err(|e| RebuildError::Metadata(format!("predicate_id: {e}")))?;
    let object_text_field = schema
        .get_field("object_text")
        .map_err(|e| RebuildError::Metadata(format!("object_text: {e}")))?;
    let kind_field = schema
        .get_field("kind")
        .map_err(|e| RebuildError::Metadata(format!("kind: {e}")))?;
    let bucket_field = schema
        .get_field("confidence_bucket")
        .map_err(|e| RebuildError::Metadata(format!("bucket: {e}")))?;
    let extracted_at_field = schema
        .get_field("extracted_at")
        .map_err(|e| RebuildError::Metadata(format!("extracted_at: {e}")))?;

    let rtxn = metadata
        .read_txn()
        .map_err(|e| RebuildError::Metadata(format!("read_txn: {e}")))?;
    // Tables are created lazily on first write. If any of the three
    // joined tables has never been opened (e.g. fresh DB with no
    // statements yet), the rebuild trivially has nothing to do.
    let stmts = match rtxn.open_table(STATEMENTS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => {
            return Err(RebuildError::Metadata(format!(
                "open STATEMENTS_TABLE: {e}"
            )))
        }
    };
    let entities = match rtxn.open_table(ENTITIES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(RebuildError::Metadata(format!("open ENTITIES_TABLE: {e}"))),
    };
    let predicates = match rtxn.open_table(PREDICATES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => {
            return Err(RebuildError::Metadata(format!(
                "open PREDICATES_TABLE: {e}"
            )))
        }
    };

    let mut count: u64 = 0;
    let mut chunk: usize = 0;

    for entry in stmts
        .iter()
        .map_err(|e| RebuildError::Metadata(format!("stmts iter: {e}")))?
    {
        let row = entry.map_err(|e| RebuildError::Metadata(format!("row read: {e}")))?;
        let (_key, value) = row;
        let stmt = value.value();

        // Skip tombstoned statements — lexical layer carries only
        // live rows per §27/02 §3.
        if stmt.tombstoned != 0 {
            continue;
        }

        // Subject canonical_name: only for resolved Entity subjects.
        // SubjectRef::Pending statements are skipped (no canonical
        // name to index against).
        let subject_canonical_name = match decode_subject(&stmt) {
            SubjectRef::Entity(_) => {
                let key = stmt.subject_entity_bytes;
                let ent = entities
                    .get(&key)
                    .map_err(|e| RebuildError::Metadata(format!("entity get: {e}")))?;
                let Some(ent_guard) = ent else {
                    continue;
                };
                ent_guard.value().canonical_name.clone()
            }
            SubjectRef::Pending(_) => continue,
        };

        let predicate_record = predicates
            .get(&stmt.predicate_id)
            .map_err(|e| RebuildError::Metadata(format!("predicate get: {e}")))?;
        let Some(pred_guard) = predicate_record else {
            // Orphan statement — skip rather than fail the rebuild.
            tracing::warn!(
                target: "brain_ops::text_indexer::rebuild",
                predicate_id = stmt.predicate_id,
                "predicate row missing during statement rebuild; skipping",
            );
            continue;
        };
        let predicate = pred_guard.value();
        let predicate_name = predicate.name.clone();

        let object_text = object_text_from_blob(&stmt.object_blob, &entities);

        let kind = match stmt.kind {
            0 => StatementKind::Fact,
            1 => StatementKind::Preference,
            2 => StatementKind::Event,
            other => {
                tracing::warn!(
                    target: "brain_ops::text_indexer::rebuild",
                    kind = other,
                    "unknown statement kind during rebuild; skipping",
                );
                continue;
            }
        };

        let mut doc = TantivyDocument::default();
        doc.add_bytes(statement_id_field, &stmt.statement_id_bytes);
        doc.add_text(subject_name_field, &subject_canonical_name);
        doc.add_text(predicate_name_field, &predicate_name);
        doc.add_u64(predicate_id_field, u64::from(stmt.predicate_id));
        doc.add_text(object_text_field, &object_text);
        doc.add_u64(kind_field, u64::from(kind.as_u8()));
        doc.add_u64(bucket_field, bucket_for_index(stmt.confidence));
        doc.add_u64(extracted_at_field, stmt.extracted_at_unix_nanos / 1_000_000);
        writer.add_document(doc)?;

        count += 1;
        chunk += 1;
        if chunk >= COMMIT_CHUNK {
            writer.commit()?;
            chunk = 0;
        }
    }

    Ok(count)
}

fn decode_subject(
    row: &brain_metadata::tables::knowledge::statement::StatementMetadata,
) -> SubjectRef {
    if row.subject_is_pending == 0 {
        SubjectRef::Entity(brain_core::EntityId::from(row.subject_entity_bytes))
    } else {
        SubjectRef::Pending(brain_core::AuditId::from(row.subject_entity_bytes))
    }
}

fn object_text_from_blob(
    blob: &[u8],
    entities: &redb::ReadOnlyTable<
        [u8; 16],
        brain_metadata::tables::knowledge::entity::EntityMetadata,
    >,
) -> String {
    let Some(object) = decode_object(blob) else {
        return String::new();
    };
    match object {
        StatementObject::Value(StatementValue::Text(s)) => s,
        StatementObject::Value(StatementValue::Integer(n)) => n.to_string(),
        StatementObject::Value(StatementValue::Float(f)) => f.to_string(),
        StatementObject::Value(StatementValue::Bool(b)) => b.to_string(),
        StatementObject::Value(StatementValue::UnixNanos(n)) => n.to_string(),
        StatementObject::Value(StatementValue::Blob(_)) => String::new(),
        StatementObject::Entity(id) => entities
            .get(&id.to_bytes())
            .ok()
            .flatten()
            .map(|g| g.value().canonical_name.clone())
            .unwrap_or_default(),
        StatementObject::Memory(_) | StatementObject::Statement(_) => String::new(),
    }
}

fn bucket_for_index(confidence: f32) -> u64 {
    let clamped = confidence.clamp(0.0, 1.0);
    let bucket = (clamped * 10.0).floor() as u64;
    bucket.min(9)
}

#[cfg(test)]
mod tests;
