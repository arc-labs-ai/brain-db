//! Typed CRUD + interning over the entity-type registry.
//!
//! Mirrors [`crate::schema::predicate`] and
//! [`crate::relation::types`]; every entity-type registration flows
//! through the shared apply path.
//!
//! Entity types are scoped by `(namespace_id, name)`.
//!
//! They used to be keyed on bare `name` alone, globally, which made the
//! registry a shared space every tenant wrote into: two namespaces
//! declaring `Builder` either silently shared one definition (identical
//! blobs) or the SECOND upload failed with `AlreadyExists` naming a type
//! the operator never declared. Built-ins (`Person`, `Organization`, …)
//! stay under `NamespaceId::SYSTEM` and remain visible to every tenant,
//! so a namespace resolves its own name first and falls back to the
//! shared one.
//!
//! The LABEL surface is deliberately unchanged: types are still offered
//! to the classifier as `brain:<Name>` regardless of owner, because that
//! is the format the classifier was tuned against. Scoping is a registry
//! concern, not a prompt one.

use brain_core::EntityTypeId;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::entity_type::{
    EntityTypeDefinition, ENTITY_TYPES_BY_SCOPE_TABLE, ENTITY_TYPES_TABLE, SPACE_BASE,
};

#[derive(thiserror::Error, Debug)]
pub enum EntityTypeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error(
        "entity_type name {name:?} already exists with id {existing_id:?} but constraints differ"
    )]
    AlreadyExists {
        name: String,
        existing_id: EntityTypeId,
    },
}

/// The system namespace that owns the built-in entity types.
const SYSTEM_NS: u32 = brain_core::NamespaceId::SYSTEM.raw();

/// Look up an entity_type visible to `(namespace_id, space)`: the space's
/// own overlay first, then the namespace's base declaration, then the
/// shared system built-in.
///
/// THE ORDER IS THE WHOLE POINT. Each step may shadow the one after it:
/// a tenant that declares `Person` gets THEIR `Person` rather than the
/// built-in; a space that declares `Person` gets ITS `Person` rather than
/// the tenant's. Consulting any of them earlier would let a more general
/// declaration silently shadow a more specific one, which is backwards —
/// the narrower scope is the one that was chosen deliberately.
///
/// Pass [`SPACE_BASE`] to ask the namespace-wide question, which is what
/// every caller that has no space in hand should do.
pub fn entity_type_lookup(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space: [u8; 16],
    name: &str,
) -> Result<Option<EntityTypeDefinition>, EntityTypeOpError> {
    for (ns, sp) in scope_chain(namespace_id, space) {
        if let Some(id) = scope_index_get(wtxn, ns, sp, name)? {
            let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
            // Bind before testing so the get-guard's borrow of `t` drops at
            // the semicolon, ahead of `t` itself.
            let row = t.get(&id)?.map(|g| g.value());
            if let Some(row) = row {
                return Ok(Some(row));
            }
        }
    }
    Ok(None)
}

/// Read-only counterpart to [`entity_type_lookup`]. Used by the
/// schema-upload pre-flight to classify each declared entity_type as
/// new/idempotent/conflict without opening a write transaction.
pub fn entity_type_lookup_rtxn(
    rtxn: &ReadTransaction,
    namespace_id: u32,
    space: [u8; 16],
    name: &str,
) -> Result<Option<EntityTypeDefinition>, EntityTypeOpError> {
    let idx = rtxn.open_table(ENTITY_TYPES_BY_SCOPE_TABLE)?;
    let types = rtxn.open_table(ENTITY_TYPES_TABLE)?;
    for (ns, sp) in scope_chain(namespace_id, space) {
        let id = idx.get(&(ns, sp, name))?.map(|g| g.value());
        if let Some(id) = id {
            let row = types.get(&id)?.map(|g| g.value());
            if let Some(row) = row {
                return Ok(Some(row));
            }
        }
    }
    Ok(None)
}

/// The display name of an entity type, by id.
///
/// Exists for error and log messages. A type violation that prints
/// `EntityTypeId(9) but ... is EntityTypeId(8)` is technically complete and
/// practically unreadable: the operator has to query the registry to learn
/// which schema rule they broke. The ids are the durable key; the name is
/// what makes the message actionable.
pub fn entity_type_name_by_id(
    wtxn: &WriteTransaction,
    id: EntityTypeId,
) -> Result<Option<String>, EntityTypeOpError> {
    let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
    let name = t.get(&id.raw())?.map(|g| g.value().name);
    Ok(name)
}

/// The scopes a lookup consults, narrowest first:
///
/// 1. `(namespace, space)` — the space's own overlay. Skipped entirely
///    when the caller asked the namespace-wide question
///    (`space == SPACE_BASE`), so the common path is exactly the two
///    lookups it always was.
/// 2. `(namespace, SPACE_BASE)` — the tenant's base schema.
/// 3. `(SYSTEM, SPACE_BASE)` — the shared built-ins. A system-namespace
///    caller consults it once, not twice.
fn scope_chain(namespace_id: u32, space: [u8; 16]) -> impl Iterator<Item = (u32, [u8; 16])> {
    let overlay = (space != SPACE_BASE).then_some((namespace_id, space));
    let base = std::iter::once((namespace_id, SPACE_BASE));
    let system = (namespace_id != SYSTEM_NS).then_some((SYSTEM_NS, SPACE_BASE));
    overlay.into_iter().chain(base).chain(system)
}

fn scope_index_get(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space: [u8; 16],
    name: &str,
) -> Result<Option<u32>, EntityTypeOpError> {
    let idx = wtxn.open_table(ENTITY_TYPES_BY_SCOPE_TABLE)?;
    let got = idx.get(&(namespace_id, space, name))?.map(|g| g.value());
    Ok(got)
}

/// Snapshot the active entity-type names as zero-shot classifier labels —
/// `brain:<Name>` in stable id-order. The GLiNER tier reads this each drain
/// cycle so a user's `SCHEMA_UPLOAD` adding entity types reaches the classifier
/// on the next batch without a shard restart.
///
/// Entity types are a flat, namespaceless registry (see the module note), so
/// the `brain:` prefix is a cosmetic convention the resolver strips before
/// lookup (`resolve_entity_type`); a user-uploaded type ("Drug") is labeled
/// `brain:Drug` and still resolves to the bare row. The prefix/format matches
/// what the classifier was tuned against — don't change it without re-measuring
/// GLiNER zero-shot recall.
pub fn entity_type_label_qnames(rtxn: &ReadTransaction) -> Result<Vec<String>, EntityTypeOpError> {
    let t = rtxn.open_table(ENTITY_TYPES_TABLE)?;
    let mut rows: Vec<(u32, String)> = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        rows.push((k.value(), v.value().name));
    }
    rows.sort_by_key(|(id, _)| *id);
    Ok(rows
        .into_iter()
        .map(|(_, name)| format!("brain:{name}"))
        .collect())
}

/// Render the active entity-type labels as an LLM-prompt block — one
/// `- brain:<Name>` bullet per declared type, stable-sorted so the block
/// (and any prompt cache keyed on it) is deterministic across cycles.
/// Substituted into the LLM extractor prompt's `{DECLARED_ENTITY_TYPES}`
/// placeholder so the extractor's entity-type vocabulary tracks the ACTIVE
/// schema (system core + user `SCHEMA_UPLOAD`) at runtime instead of a
/// list baked into the prompt text.
///
/// Reuses [`entity_type_label_qnames`] for the label surface (`brain:<Name>`),
/// then re-sorts lexicographically so the block order is independent of id
/// allocation order.
pub fn render_declared_entity_types_block(
    rtxn: &ReadTransaction,
) -> Result<String, EntityTypeOpError> {
    let mut labels = entity_type_label_qnames(rtxn)?;
    labels.sort();
    let mut out = String::new();
    for label in labels {
        out.push_str("- ");
        out.push_str(&label);
        out.push('\n');
    }
    Ok(out)
}

/// Intern an entity_type by name. Idempotent on identical
/// `schema_blob`; refuses to clobber a pre-existing row with a
/// diverging blob.
///
/// Allocates id = `max(existing) + 1` on first registration. Person
/// gets id `1` because it's the first item in the system schema
pub fn entity_type_intern(
    wtxn: &WriteTransaction,
    namespace_id: u32,
    space: [u8; 16],
    name: &str,
    schema_blob: Vec<u8>,
    now_unix_nanos: u64,
) -> Result<EntityTypeId, EntityTypeOpError> {
    // Only THIS EXACT SCOPE's own row can conflict. A built-in of the same
    // name is not a conflict — declaring `Person` in your namespace is a
    // legitimate override, and before scoping it was an error naming a
    // type the operator had never written. The same now holds one level
    // down: a space declaring a name its namespace already declares is an
    // override, not a collision, and gets its own id. That is what makes
    // the overlay an overlay rather than a second chance to conflict.
    if let Some(id) = scope_index_get(wtxn, namespace_id, space, name)? {
        let existing = {
            let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
            let row = t.get(&id)?.map(|g| g.value());
            row
        };
        if let Some(existing) = existing {
            if existing.schema_blob == schema_blob {
                return Ok(existing.id());
            }
            return Err(EntityTypeOpError::AlreadyExists {
                name: name.to_string(),
                existing_id: existing.id(),
            });
        }
    }

    // Fresh registration.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _v) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("EntityTypeId space exhausted")
    };

    let row = EntityTypeDefinition::new(
        EntityTypeId::from(next_id_raw),
        namespace_id,
        name.to_string(),
        schema_blob,
        now_unix_nanos,
    );
    {
        let mut t = wtxn.open_table(ENTITY_TYPES_TABLE)?;
        t.insert(&row.entity_type_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(ENTITY_TYPES_BY_SCOPE_TABLE)?;
        idx.insert(&(namespace_id, space, name), &row.entity_type_id)?;
    }
    Ok(EntityTypeId::from(next_id_raw))
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    // ── Space overlay ──────────────────────────────────────────────────────
    //
    // The chain is `(ns, space) → (ns, SPACE_BASE) → (SYSTEM, SPACE_BASE)`.
    // Every test below pins one rung of it or one way it could be got wrong.

    const TENANT_ONE: u32 = 71;
    const TENANT_TWO: u32 = 72;
    const SPACE_A: [u8; 16] = [0xA1; 16];
    const SPACE_B: [u8; 16] = [0xB2; 16];

    fn intern(db: &redb::Database, ns: u32, space: [u8; 16], name: &str) -> EntityTypeId {
        let wtxn = db.begin_write().unwrap();
        let id = entity_type_intern(&wtxn, ns, space, name, Vec::new(), NOW).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn lookup(db: &redb::Database, ns: u32, space: [u8; 16], name: &str) -> Option<EntityTypeId> {
        let wtxn = db.begin_write().unwrap();
        let got = entity_type_lookup(&wtxn, ns, space, name)
            .unwrap()
            .map(|d| d.id());
        wtxn.commit().unwrap();
        got
    }

    #[test]
    fn a_space_sees_its_namespaces_base_schema() {
        // The ordinary case, and the one that must not regress: a tenant
        // uploads one schema and every space in it works against that schema
        // without uploading anything of its own.
        let (_d, db) = open_db();
        let base = intern(&db, TENANT_ONE, SPACE_BASE, "Vendor");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Vendor"), Some(base));
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_B, "Vendor"), Some(base));
    }

    #[test]
    fn a_space_overlay_shadows_the_namespace_base() {
        // The narrower declaration wins. Resolving the base first would make
        // an overlay unreachable, which is the whole feature inverted.
        let (_d, db) = open_db();
        let base = intern(&db, TENANT_ONE, SPACE_BASE, "Vendor");
        let overlay = intern(&db, TENANT_ONE, SPACE_A, "Vendor");
        assert_ne!(overlay, base, "an overlay is its own row, not the base's");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Vendor"), Some(overlay));
        // …and the base is untouched for everyone else.
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_B, "Vendor"), Some(base));
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_BASE, "Vendor"), Some(base));
    }

    #[test]
    fn one_spaces_overlay_is_invisible_to_another() {
        // The isolation claim, stated directly. Nothing but SPACE_A may see
        // a type only SPACE_A declared.
        let (_d, db) = open_db();
        let only_a = intern(&db, TENANT_ONE, SPACE_A, "Stage");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Stage"), Some(only_a));
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_B, "Stage"), None);
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_BASE, "Stage"), None);
    }

    #[test]
    fn the_system_builtins_remain_visible_from_inside_a_space() {
        // The pre-existing bottom rung still answers through the new top one.
        let (_d, db) = open_db();
        let builtin = intern(&db, SYSTEM_NS, SPACE_BASE, "Person");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Person"), Some(builtin));
    }

    #[test]
    fn a_space_may_shadow_a_system_builtin() {
        // Same rule one rung further: the most specific declaration wins,
        // even against a built-in.
        let (_d, db) = open_db();
        let builtin = intern(&db, SYSTEM_NS, SPACE_BASE, "Person");
        let mine = intern(&db, TENANT_ONE, SPACE_A, "Person");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Person"), Some(mine));
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_B, "Person"), Some(builtin));
    }

    #[test]
    fn tenant_isolation_survives_the_new_key() {
        // The guarantee the previous widening bought. Adding a third key
        // component must not have reopened it.
        let (_d, db) = open_db();
        let acme = intern(&db, TENANT_ONE, SPACE_BASE, "Vendor");
        let other = intern(&db, TENANT_TWO, SPACE_BASE, "Vendor");
        assert_ne!(acme, other);
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_A, "Vendor"), Some(acme));
        assert_eq!(lookup(&db, TENANT_TWO, SPACE_A, "Vendor"), Some(other));
    }

    #[test]
    fn the_same_name_in_two_spaces_gets_two_ids() {
        // Overlays are independent declarations, not a shared row. If these
        // collapsed to one id, a statement typed against one space's `Deal`
        // would validate against the other's.
        let (_d, db) = open_db();
        let a = intern(&db, TENANT_ONE, SPACE_A, "Deal");
        let b = intern(&db, TENANT_ONE, SPACE_B, "Deal");
        assert_ne!(a, b);
    }

    #[test]
    fn interning_twice_in_one_scope_is_idempotent() {
        // Re-uploading an unchanged schema must not mint a second row.
        let (_d, db) = open_db();
        let first = intern(&db, TENANT_ONE, SPACE_A, "Vendor");
        let again = intern(&db, TENANT_ONE, SPACE_A, "Vendor");
        assert_eq!(first, again);
    }

    #[test]
    fn an_overlay_does_not_satisfy_a_base_lookup() {
        // The asymmetry is deliberate: a space may borrow from its tenant,
        // never the other way round. A caller holding no space — the write
        // path's auto-discovery, every namespace-wide query — must not be
        // answered from some arbitrary space's private declaration.
        let (_d, db) = open_db();
        intern(&db, TENANT_ONE, SPACE_A, "Vendor");
        assert_eq!(lookup(&db, TENANT_ONE, SPACE_BASE, "Vendor"), None);
    }

    #[test]
    fn a_system_caller_does_not_consult_the_chain_twice() {
        // `scope_chain` drops the system rung for a system caller. Exercised
        // because the dedupe is a `then_some` that is easy to lose.
        let (_d, db) = open_db();
        let builtin = intern(&db, SYSTEM_NS, SPACE_BASE, "Person");
        assert_eq!(lookup(&db, SYSTEM_NS, SPACE_BASE, "Person"), Some(builtin));
        assert_eq!(scope_chain(SYSTEM_NS, SPACE_BASE).count(), 1);
        // A non-system caller with no space: own base + system.
        assert_eq!(scope_chain(TENANT_ONE, SPACE_BASE).count(), 2);
        // With a space: overlay + own base + system.
        assert_eq!(scope_chain(TENANT_ONE, SPACE_A).count(), 3);
    }

    #[test]
    fn scope_chain_is_ordered_narrowest_first() {
        // Order IS the shadowing rule, so it is asserted rather than implied
        // by the lookup tests above.
        let chain: Vec<_> = scope_chain(TENANT_ONE, SPACE_A).collect();
        assert_eq!(
            chain,
            vec![
                (TENANT_ONE, SPACE_A),
                (TENANT_ONE, SPACE_BASE),
                (SYSTEM_NS, SPACE_BASE)
            ]
        );
    }

    #[test]
    fn render_block_lists_labels_sorted_with_brain_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            // Intern out of lexical order so the sort is exercised.
            entity_type_intern(
                &wtxn,
                brain_core::NamespaceId::SYSTEM.raw(),
                SPACE_BASE,
                "Person",
                Vec::new(),
                NOW,
            )
            .unwrap();
            entity_type_intern(
                &wtxn,
                brain_core::NamespaceId::SYSTEM.raw(),
                SPACE_BASE,
                "Drug",
                Vec::new(),
                NOW,
            )
            .unwrap();
            entity_type_intern(
                &wtxn,
                brain_core::NamespaceId::SYSTEM.raw(),
                SPACE_BASE,
                "Organization",
                Vec::new(),
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let block = render_declared_entity_types_block(&rtxn).unwrap();

        // Every declared type appears as a `- brain:<Name>` bullet.
        assert!(block.contains("- brain:Person\n"), "{block}");
        assert!(block.contains("- brain:Drug\n"), "{block}");
        assert!(block.contains("- brain:Organization\n"), "{block}");

        // Lexicographic order regardless of id allocation order.
        let drug = block.find("brain:Drug").unwrap();
        let org = block.find("brain:Organization").unwrap();
        let person = block.find("brain:Person").unwrap();
        assert!(drug < org && org < person, "not sorted: {block}");
    }

    #[test]
    fn render_block_empty_when_no_types() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        // Touch the table so it exists, but intern nothing.
        {
            let wtxn = db.begin_write().unwrap();
            let _ = wtxn.open_table(ENTITY_TYPES_TABLE).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert!(render_declared_entity_types_block(&rtxn)
            .unwrap()
            .is_empty());
    }

    // ── tenant isolation ────────────────────────────────────────────────

    const ACME: u32 = 7;
    const GLOBEX: u32 = 8;

    #[test]
    fn two_tenants_may_declare_the_same_type_name() {
        // The reported bug: entity types lived in one global name space.
        // Globex declaring `Builder` after Acme did either silently reused
        // Acme's definition or failed with `AlreadyExists` naming a type
        // Globex had never written.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let acme_id = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![1], NOW).unwrap();
        let globex_id =
            entity_type_intern(&wtxn, GLOBEX, SPACE_BASE, "Builder", vec![2, 2], NOW).unwrap();
        wtxn.commit().unwrap();

        assert_ne!(
            acme_id, globex_id,
            "same name in two tenants must be two rows"
        );

        let wtxn = db.begin_write().unwrap();
        assert_eq!(
            entity_type_lookup(&wtxn, ACME, SPACE_BASE, "Builder")
                .unwrap()
                .unwrap()
                .schema_blob,
            vec![1]
        );
        assert_eq!(
            entity_type_lookup(&wtxn, GLOBEX, SPACE_BASE, "Builder")
                .unwrap()
                .unwrap()
                .schema_blob,
            vec![2, 2]
        );
    }

    #[test]
    fn re_declaring_your_own_type_identically_is_idempotent() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let first = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![1], NOW).unwrap();
        let again = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![1], NOW).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(
            first, again,
            "re-uploading an unchanged schema must not mint"
        );
    }

    #[test]
    fn redeclaring_your_own_type_differently_still_conflicts() {
        // Scoping must not weaken the guard WITHIN a tenant: changing a
        // type's definition under the same name is still a conflict the
        // operator has to resolve, just no longer one another tenant can
        // cause.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![1], NOW).unwrap();
        let err = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![9], NOW).unwrap_err();
        assert!(
            matches!(err, EntityTypeOpError::AlreadyExists { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn builtins_are_visible_to_every_tenant() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let person =
            entity_type_intern(&wtxn, SYSTEM_NS, SPACE_BASE, "Person", Vec::new(), NOW).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        for ns in [ACME, GLOBEX] {
            assert_eq!(
                entity_type_lookup(&wtxn, ns, SPACE_BASE, "Person")
                    .unwrap()
                    .unwrap()
                    .id(),
                person,
                "a tenant that declared nothing still sees the built-ins",
            );
        }
    }

    #[test]
    fn a_tenants_own_type_shadows_the_builtin_of_the_same_name() {
        // Own-namespace-first ordering. If the system namespace were
        // consulted first, declaring `Person` would appear to succeed and
        // then silently resolve to the built-in forever.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let builtin =
            entity_type_intern(&wtxn, SYSTEM_NS, SPACE_BASE, "Person", Vec::new(), NOW).unwrap();
        let own = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Person", vec![42], NOW).unwrap();
        wtxn.commit().unwrap();
        assert_ne!(builtin, own);

        let wtxn = db.begin_write().unwrap();
        assert_eq!(
            entity_type_lookup(&wtxn, ACME, SPACE_BASE, "Person")
                .unwrap()
                .unwrap()
                .id(),
            own
        );
        assert_eq!(
            entity_type_lookup(&wtxn, GLOBEX, SPACE_BASE, "Person")
                .unwrap()
                .unwrap()
                .id(),
            builtin
        );
    }

    #[test]
    fn read_txn_lookup_agrees_with_write_txn_lookup() {
        // The pre-flight classifier uses the rtxn variant; if the two
        // disagreed, an upload could pass pre-flight and then conflict.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        entity_type_intern(&wtxn, SYSTEM_NS, SPACE_BASE, "Person", Vec::new(), NOW).unwrap();
        let own = entity_type_intern(&wtxn, ACME, SPACE_BASE, "Builder", vec![1], NOW).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            entity_type_lookup_rtxn(&rtxn, ACME, SPACE_BASE, "Builder")
                .unwrap()
                .unwrap()
                .id(),
            own
        );
        assert!(
            entity_type_lookup_rtxn(&rtxn, GLOBEX, SPACE_BASE, "Builder")
                .unwrap()
                .is_none()
        );
        assert!(entity_type_lookup_rtxn(&rtxn, GLOBEX, SPACE_BASE, "Person")
            .unwrap()
            .is_some());
    }
}
