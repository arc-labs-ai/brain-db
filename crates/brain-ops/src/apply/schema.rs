//! Apply schema-shaped phases: `UpsertSchema` — the single phase behind
//! every schema mutation (UPLOAD / REPLACE / DROP).
//!
//! It reconstructs the schema document from `Phase::UpsertSchema.blob`
//! (DSL source for UPLOAD / REPLACE, re-parsed here; serde_json for a
//! targeted DROP, whose narrowed form has no DSL source), applies the
//! destructive delta first (REPLACE's `replace_all` drops all declared
//! vocabulary; DROP's `drops` removes specific targets), then delegates to
//! [`brain_metadata::schema::store::schema_upload`] which atomically writes
//! the schema-version row, updates the active-version pointer, fans out
//! predicate/relation-type/entity-type/extractor interns, and re-flags
//! pre-existing statements outside the new vocabulary — all inside the same
//! wtxn. Recovery runs the identical sequence, so replay converges on the
//! same state the live write produced.

use brain_metadata::extractor::ops::extractor_drop_declared;
use brain_metadata::relation::types::{relation_type_drop_one, relation_type_drop_schema_declared};
use brain_metadata::schema::predicate::{predicate_drop_one, predicate_drop_schema_declared};
use brain_metadata::schema::store::{declared_context_wtxn, schema_upload_with_mode, SourceMode};
use brain_metadata::system_schema::SYSTEM_SCHEMA_NAMESPACE;
use brain_metadata::DeclScope;
use brain_protocol::schema::{parse_schema, validate_with};
use brain_protocol::schema_drop_target;
use redb::WriteTransaction;

use super::ApplyError;
use crate::write::{Phase, PhaseAck, Write};

/// Apply [`Phase::UpsertSchema`].
///
/// `Phase.blob` carries the raw DSL source text as UTF-8 bytes. The
/// handler already parsed + validated before calling submit; we
/// re-parse + re-validate as a safety check (deterministic + cheap;
/// a divergence here indicates a build-time bug in the protocol
/// crate). The actual persistence + fan-out lives in
/// `brain_metadata::schema::store::schema_upload` so the write path
/// shares one canonical implementation with the system-schema
/// bootstrap.
pub fn apply_upsert_schema(
    wtxn: &WriteTransaction,
    phase: &Phase,
    _write: &Write,
) -> Result<PhaseAck, ApplyError> {
    let Phase::UpsertSchema {
        space,
        blob,
        created_at_unix_nanos,
        replace_all,
        drops,
        ..
    } = phase
    else {
        return Err(ApplyError::PhaseMisShape("expected UpsertSchema"));
    };

    // Blob format is chosen by the op: UPLOAD / REPLACE carry the DSL source
    // text (re-parsed here, so replay stays authoritative against the running
    // binary's parser); targeted DROP carries the already-narrowed schema as
    // serde_json (its narrowed form has no DSL source to re-parse), keyed off
    // a non-empty `drops` delta. Both paths yield a `Schema` that is validated
    // uniformly below.
    let parsed = if drops.is_empty() {
        let source = std::str::from_utf8(blob).map_err(|e| {
            ApplyError::Invariant(format!("UpsertSchema blob is not UTF-8 source text: {e}"))
        })?;
        parse_schema(source)
            .map_err(|e| ApplyError::Invariant(format!("UpsertSchema re-parse failed: {e:?}")))?
    } else {
        serde_json::from_slice(blob).map_err(|e| {
            ApplyError::Invariant(format!(
                "UpsertSchema drop blob is not valid schema json: {e}"
            ))
        })?
    };
    // Both phases re-validate, against different contexts, and neither is the
    // empty one.
    //
    // A plain UPLOAD is additive: it may reference types the active schema
    // already declares, so it validates against the same merge target the
    // handler used — otherwise WAL replay would reject a document the server
    // accepted.
    //
    // A destructive phase (REPLACE / DROP) carries the namespace's whole new
    // schema and overwrites the stored source, so it must NOT see the active
    // schema: resolving against the document being replaced would hide a
    // reference the new document leaves dangling, which is the one thing this
    // check exists to catch.
    //
    // It must still see the system schema. That was the bug: `Person`,
    // `Organization`, `Place` and the rest are declared once in the `brain`
    // namespace and referenced by every tenant document without being
    // re-declared, so validating "standalone" reported all of them as
    // undeclared. Every SCHEMA_DROP and every SCHEMA_REPLACE whose document
    // named a system entity type failed here as an `Invariant` — surfacing to
    // the caller as a 502 with the detail withheld, after the handler had
    // already accepted the request.
    let destructive = *replace_all || !drops.is_empty();
    let mode = if destructive {
        SourceMode::Replace
    } else {
        SourceMode::Merge
    };
    let context_scope = if destructive {
        DeclScope::base(SYSTEM_SCHEMA_NAMESPACE)
    } else {
        DeclScope::space(parsed.namespace.as_str(), *space)
    };
    let ctx = declared_context_wtxn(wtxn, context_scope)
        .map_err(|e| ApplyError::Metadata(format!("declared context: {e}")))?;
    let validated = validate_with(&parsed, &ctx).map_err(|errs| {
        ApplyError::Invariant(format!("UpsertSchema re-validate failed: {errs:?}"))
    })?;

    let namespace = validated.as_schema().namespace.clone();

    // Destructive delta first, then the (additive) upload — the same order the
    // REPLACE / DROP handlers use, so live apply and WAL replay converge on
    // identical state. UPLOAD carries an empty delta and skips both branches.
    let dropped = apply_schema_delta(
        wtxn,
        DeclScope::space(&namespace, *space),
        *replace_all,
        drops,
    )?;

    // The scope comes off the phase, not off the `Write` and not off any
    // state read in here. The entity-type registry is keyed
    // `(namespace, space, name)` and resolves `(ns, space) → (ns, base) →
    // (system, base)`, so where a definition lands decides who can ever see
    // it — and that decision belongs to the handler that accepted the
    // request, which is the only place the caller's explicit opt-in exists.
    //
    // This function is also the WAL-replay path's twin: recovery re-applies
    // from `SchemaUpdateBody.space`, which `wal_map` copies from this same
    // field. Reading the scope from anything but the phase would let replay
    // reach a different answer than the apply it has to reproduce.
    let version = schema_upload_with_mode(wtxn, &validated, *space, *created_at_unix_nanos, mode)
        .map_err(|e| ApplyError::Metadata(format!("schema_upload: {e}")))?;

    Ok(PhaseAck::UpsertedSchema {
        namespace,
        version,
        dropped: u32::try_from(dropped).unwrap_or(u32::MAX),
    })
}

/// Apply the destructive schema delta carried on an `UpsertSchema` phase.
///
/// `replace_all` (SCHEMA_REPLACE) drops every declared predicate / relation
/// type / extractor in the namespace; `drops` (SCHEMA_DROP) removes specific
/// declared targets. Both run before the additive `schema_upload`, so a plain
/// UPLOAD (empty delta) is a no-op here. Shared by the live apply path and WAL
/// recovery so the two never diverge.
fn apply_schema_delta(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    replace_all: bool,
    drops: &[(u8, String)],
) -> Result<usize, ApplyError> {
    let mut dropped = 0usize;
    if replace_all {
        dropped += predicate_drop_schema_declared(wtxn, scope)
            .map_err(|e| ApplyError::Metadata(format!("predicate drop-all: {e}")))?;
        dropped += relation_type_drop_schema_declared(wtxn, scope)
            .map_err(|e| ApplyError::Metadata(format!("relation_type drop-all: {e}")))?;
        dropped += extractor_drop_declared(wtxn, scope)
            .map_err(|e| ApplyError::Metadata(format!("extractor drop-all: {e}")))?;
    }
    for (kind, name) in drops {
        match *kind {
            schema_drop_target::PREDICATE => {
                if predicate_drop_one(wtxn, scope, name)
                    .map_err(|e| ApplyError::Metadata(format!("predicate drop {name:?}: {e}")))?
                    .is_some()
                {
                    dropped += 1;
                }
            }
            schema_drop_target::RELATION_TYPE => {
                if relation_type_drop_one(wtxn, scope, name)
                    .map_err(|e| ApplyError::Metadata(format!("relation_type drop {name:?}: {e}")))?
                    .is_some()
                {
                    dropped += 1;
                }
            }
            other => {
                return Err(ApplyError::Invariant(format!(
                    "UpsertSchema carried unknown drop kind {other}"
                )));
            }
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_metadata::entity::types::entity_type_lookup_rtxn;
    use brain_metadata::tables::scope::SPACE_BASE;
    use brain_metadata::MetadataDb;
    use tempfile::TempDir;

    use crate::write::{Phase, Write, WriteId};

    /// The declared type is `Widget` and deliberately NOT one of the system
    /// schema's built-ins (`Person`, `Organization`, `Project`, `Event`,
    /// `Place`, `Concept`). Those resolve from every scope through the
    /// chain's last rung, `(SYSTEM, SPACE_BASE)`, so naming one here would
    /// make "does the base resolve this?" answer yes whatever the upload did
    /// — a scope assertion that can never fail.
    const SOURCE: &str = r#"
namespace acme

define entity_type Widget {
}
"#;

    /// A space that is NOT the base, and a sibling of it, for the
    /// overlay-visibility assertions.
    fn space_a() -> [u8; 16] {
        brain_core::SpaceId::derive_from_string("acme", "tenant:user-a").into()
    }
    fn space_b() -> [u8; 16] {
        brain_core::SpaceId::derive_from_string("acme", "tenant:user-b").into()
    }

    fn upload_phase(space: [u8; 16]) -> Phase {
        Phase::UpsertSchema {
            namespace: "acme".into(),
            version: 1,
            space,
            blob: SOURCE.as_bytes().to_vec(),
            declared_predicates: Vec::new(),
            declared_relation_types: Vec::new(),
            declared_entity_types: Vec::new(),
            created_at_unix_nanos: 1_700_000_000_000,
            replace_all: false,
            drops: Vec::new(),
        }
    }

    /// Does `name` resolve for a reader at `space`? Walks the real
    /// `(ns, space) → (ns, base) → (system, base)` chain, so this is the
    /// question a reader actually asks — not a scope-exact probe.
    fn resolves_at(db: &MetadataDb, space: [u8; 16], name: &str) -> bool {
        let rtxn = db.read_txn().unwrap();
        let ns = brain_metadata::namespace::namespace_lookup_by_name(&rtxn, "acme")
            .unwrap()
            .expect("the upload interns its namespace");
        entity_type_lookup_rtxn(
            &rtxn,
            brain_metadata::TypeScope::space(ns.raw(), space),
            name,
        )
        .unwrap()
        .is_some()
    }

    #[test]
    fn upsert_schema_round_trips_and_increments_version() {
        let dir = TempDir::new().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();

        let phase = upload_phase(SPACE_BASE);
        let write = Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            phase.clone(),
        );

        {
            let wtxn = db.write_txn().unwrap();
            let ack = apply_upsert_schema(&wtxn, &phase, &write).unwrap();
            assert_eq!(
                ack,
                PhaseAck::UpsertedSchema {
                    namespace: "acme".into(),
                    version: 1,
                    dropped: 0,
                }
            );
            wtxn.commit().unwrap();
        }

        let rtxn = db.read_txn().unwrap();
        let active =
            brain_metadata::schema::store::schema_active(&rtxn, DeclScope::base("acme")).unwrap();
        assert_eq!(active, Some(1));
    }

    /// Apply a space-scoped phase and the WAL record built from that same
    /// phase into two separate databases, and require the two to agree about
    /// WHERE the declaration landed.
    ///
    /// This is the invariant that matters most about the scope living on the
    /// phase: replay has to reproduce the original apply, never decide a
    /// scope of its own. If `wal_map` dropped the space (as it did while the
    /// phase had no field to carry it) the replayed database would resolve
    /// `Widget` from the namespace base — visible to every sibling space —
    /// and the two sides of this assertion would diverge on the first
    /// restart, silently, long after the write was acked.
    #[test]
    fn wal_replay_of_a_space_scoped_upload_lands_in_the_same_space_as_the_live_apply() {
        use brain_metadata::recovery::phase_bodies::decode_schema_update;
        use brain_storage::recovery::MetadataSink;
        use brain_storage::wal::payload::WalPayload;

        let dir = TempDir::new().unwrap();
        let space = space_a();
        let phase = upload_phase(space);
        let write = Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            phase.clone(),
        );

        // Live apply.
        let live = MetadataDb::open(dir.path().join("live.redb")).unwrap();
        {
            let wtxn = live.write_txn().unwrap();
            apply_upsert_schema(&wtxn, &phase, &write).unwrap();
            wtxn.commit().unwrap();
        }

        // The WAL record the same phase journals, and the scope it carries.
        let payload = crate::writer::wal_map::phase_to_wal_payload(&phase, &write)
            .expect("UpsertSchema is WAL-mapped");
        let WalPayload::PhaseBody(record) = &payload else {
            panic!("expected a PhaseBody payload");
        };
        let body = decode_schema_update(&record.body).unwrap();
        assert_eq!(
            body.space, space,
            "the WAL body must record the scope the phase chose, verbatim"
        );

        // Replay into a fresh database, through the real recovery sink.
        let mut replayed = MetadataDb::open(dir.path().join("replayed.redb")).unwrap();
        replayed.apply(1, 1_700_000_000_000, &payload).unwrap();

        for (label, db) in [("live apply", &live), ("wal replay", &replayed)] {
            assert!(
                resolves_at(db, space, "Widget"),
                "{label}: the declaring space must resolve its own overlay"
            );
            assert!(
                !resolves_at(db, SPACE_BASE, "Widget"),
                "{label}: an overlay must not become the namespace base"
            );
            assert!(
                !resolves_at(db, space_b(), "Widget"),
                "{label}: a sibling space must not see another space's overlay"
            );
        }
    }

    /// The base-scoped counterpart, so the test above is pinning the scope
    /// and not just the fact that two code paths run the same function: the
    /// same document at `SPACE_BASE` is visible from every space, which is
    /// the opposite outcome from the same assertions.
    #[test]
    fn wal_replay_of_a_base_scoped_upload_stays_visible_from_every_space() {
        use brain_storage::recovery::MetadataSink;

        let dir = TempDir::new().unwrap();
        let phase = upload_phase(SPACE_BASE);
        let write = Write::single(
            WriteId::new(),
            brain_core::SpaceId::default(),
            phase.clone(),
        );
        let payload = crate::writer::wal_map::phase_to_wal_payload(&phase, &write)
            .expect("UpsertSchema is WAL-mapped");

        let mut replayed = MetadataDb::open(dir.path().join("replayed.redb")).unwrap();
        replayed.apply(1, 1_700_000_000_000, &payload).unwrap();

        assert!(resolves_at(&replayed, SPACE_BASE, "Widget"));
        assert!(
            resolves_at(&replayed, space_a(), "Widget"),
            "a namespace-base declaration resolves from any space in the namespace"
        );
        assert!(resolves_at(&replayed, space_b(), "Widget"));
    }
}
