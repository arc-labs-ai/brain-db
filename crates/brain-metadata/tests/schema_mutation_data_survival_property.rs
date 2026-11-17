//! Stateful schema-mutation properties for populated metadata stores.
//!
//! The ordinary schema-mutation oracle stops at the control plane: it checks
//! the version, force flag, or live-row count returned by the mutation, but
//! does not read the rows that mutation touched. That gap has allowed five
//! data-integrity bugs to ship despite thousands of passing tests. In
//! particular, the force-drop path can remove a predicate declaration while
//! leaving live statements behind, and allocator tests on empty databases
//! cannot distinguish `max + 1` from a persistent high-water mark.
//!
//! This suite interleaves uploads, replacements, drops, writes, supersession,
//! and tombstoning over a populated `MetadataDb`. A parallel model remembers
//! the vocabulary each row was written with. After every operation it reads
//! every retained row back, verifies that ids within a registry are never
//! reused for another name, and exercises both statement and relation list
//! paths. The trace is deliberately part of every failure so a generated
//! sequence can be replayed against the database by hand.

use brain_core::{
    Entity, EntityId, EntityType, EvidenceEntry, EvidenceRef, ExtractorId, MemoryId, Relation,
    RelationId, SessionId, Statement, StatementId, StatementKind, StatementObject, StatementValue,
    SubjectRef, TombstoneReason,
};
use brain_metadata::entity::ops::{entity_put, normalize_name};
use brain_metadata::relation::ops::relation_reference_count_by_type;
use brain_metadata::relation::types::{
    relation_type_drop_one, relation_type_id_by_qname, relation_type_lookup_by_qname,
};
use brain_metadata::schema::apply::resolve_redeclarations;
use brain_metadata::schema::predicate::{
    predicate_drop_one, predicate_id_by_qname, predicate_intern_or_get, predicate_lookup_by_qname,
};
use brain_metadata::schema::store::{schema_upload, schema_upload_with_mode, SourceMode};
use brain_metadata::{
    relation_create, relation_get, relation_list_from, relation_list_to, relation_supersede,
    relation_tombstone, statement_create, statement_get, statement_list, statement_list_page,
    statement_supersede, statement_tombstone, DeclScope, MetadataDb, PredicateQnames,
    RelationListFilter, RelationTypeQnames, RowScope, StatementListFilter, StatementPageExtra,
};
use brain_protocol::schema::{parse_schema, validate, ValidatedSchema};
use proptest::prelude::*;

const NOW: u64 = 1_700_000_000_000_000_000;
const NS: &str = "acme";
const PREDICATES: [&str; 4] = ["prefers", "likes", "needs", "ghost"];
const RELATIONS: [&str; 3] = ["works_at", "knows", "visits"];

fn scope() -> RowScope {
    RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
}

fn open_db() -> (tempfile::TempDir, MetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("md.redb")).unwrap();
    (dir, db)
}

fn validated(src: &str) -> ValidatedSchema {
    validate(&parse_schema(src).expect("parse")).expect("validate")
}

fn schema(predicates: &[&str], relations: &[&str], changed: Option<&str>) -> ValidatedSchema {
    let mut src = String::from(
        "namespace acme\n\
         define entity_type Person { attributes {} }\n",
    );
    for name in predicates {
        let object = if changed == Some(*name) {
            "Value<number>"
        } else {
            "Value<text>"
        };
        src.push_str(&format!(
            "define predicate {name} {{ kind: Preference object: {object} }}\n"
        ));
    }
    for name in relations {
        src.push_str(&format!(
            "define relation_type {name} {{ from: Any to: Any cardinality: many-to-many }}\n"
        ));
    }
    validated(&src)
}

fn make_entity(db: &MetadataDb, name: &str) -> EntityId {
    let id = EntityId::new();
    let wtxn = db.write_txn().unwrap();
    entity_put(
        &wtxn,
        scope(),
        SessionId::DEFAULT,
        &Entity::new_active(
            id,
            EntityType::PERSON_ID,
            name.into(),
            normalize_name(name),
            NOW,
        ),
    )
    .unwrap();
    wtxn.commit().unwrap();
    id
}

fn statement_for(predicate: brain_core::PredicateId, subject: EntityId) -> Statement {
    Statement::new_root(
        StatementId::new(),
        StatementKind::Preference,
        SubjectRef::Entity(subject),
        predicate,
        StatementObject::Value(StatementValue::Text("value".into())),
        0.9,
        EvidenceRef::inline_from_slice(&[EvidenceEntry::from_parts(
            MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
            1.0,
            0,
            ExtractorId::default(),
        )]),
        ExtractorId::default(),
        NOW,
        0,
    )
}

fn relation_for(ty: brain_core::RelationTypeId, from: EntityId, to: EntityId) -> Relation {
    Relation::new_root(
        RelationId::new(),
        ty,
        from,
        to,
        0.9,
        vec![],
        ExtractorId::from(0),
        NOW,
        false,
    )
}

#[derive(Clone, Debug)]
enum Op {
    UploadSchema {
        mask: u8,
    },
    ReplaceSchema {
        mask: u8,
        changed: bool,
        omitted: bool,
    },
    DropPredicate {
        name: usize,
        force: bool,
    },
    DropRelationType {
        name: usize,
        force: bool,
    },
    CreateStatement {
        name: usize,
    },
    CreateRelation {
        name: usize,
    },
    SupersedeStatement {
        slot: usize,
    },
    SupersedeRelation {
        slot: usize,
    },
    TombstoneStatement {
        slot: usize,
    },
    TombstoneRelation {
        slot: usize,
    },
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1u8..=15).prop_map(|mask| Op::UploadSchema { mask }),
        (1u8..=15, any::<bool>(), any::<bool>()).prop_map(|(mask, changed, omitted)| {
            Op::ReplaceSchema {
                mask,
                changed,
                omitted,
            }
        }),
        (0usize..PREDICATES.len(), any::<bool>())
            .prop_map(|(name, force)| Op::DropPredicate { name, force }),
        (0usize..RELATIONS.len(), any::<bool>())
            .prop_map(|(name, force)| Op::DropRelationType { name, force }),
        (0usize..PREDICATES.len()).prop_map(|name| Op::CreateStatement { name }),
        (0usize..RELATIONS.len()).prop_map(|name| Op::CreateRelation { name }),
        (0usize..6).prop_map(|slot| Op::SupersedeStatement { slot }),
        (0usize..6).prop_map(|slot| Op::SupersedeRelation { slot }),
        (0usize..6).prop_map(|slot| Op::TombstoneStatement { slot }),
        (0usize..6).prop_map(|slot| Op::TombstoneRelation { slot }),
    ]
}

#[derive(Default)]
struct Model {
    statements: Vec<(StatementId, String)>,
    relations: Vec<(RelationId, String)>,
    predicate_ids: std::collections::BTreeMap<u32, String>,
    relation_type_ids: std::collections::BTreeMap<u32, String>,
}

fn qname(name: &str) -> String {
    format!("{NS}:{name}")
}

fn remember_id(
    ids: &mut std::collections::BTreeMap<u32, String>,
    id: u32,
    name: String,
    space: &str,
) -> Result<(), String> {
    if let Some(old) = ids.get(&id) {
        if old != &name {
            return Err(format!(
                "{space} id {id} was observed as {old:?} and then {name:?}"
            ));
        }
    } else {
        ids.insert(id, name);
    }
    Ok(())
}

fn model_rejection<E: std::fmt::Debug>(error: E) -> String {
    let debug = format!("{error:?}");
    if debug.contains("Storage") || debug.contains("Transaction") || debug.contains("Table") {
        panic!("metadata infrastructure failure: {debug}");
    }
    format!("rejected: {debug}")
}

fn assert_invariants(
    db: &MetadataDb,
    model: &mut Model,
    entities: &[EntityId],
) -> Result<(), String> {
    let rtxn = db.read_txn().map_err(|e| e.to_string())?;
    let mut pq = PredicateQnames::new();
    for (id, written) in &model.statements {
        let row = statement_get(&rtxn, *id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("missing statement {id:?}"))?;
        let got = pq.resolve(&rtxn, row.predicate);
        if got != *written && !got.is_empty() {
            return Err(format!(
                "statement {id:?}: wrote {written:?}, resolved {got:?}"
            ));
        }
        if !got.is_empty() {
            remember_id(
                &mut model.predicate_ids,
                row.predicate.raw(),
                got.clone(),
                "predicate",
            )?;
        }
    }
    let mut rq = RelationTypeQnames::new();
    for (id, written) in &model.relations {
        let row = relation_get(&rtxn, *id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("missing relation {id:?}"))?;
        let got = rq.resolve(&rtxn, row.relation_type);
        if got != *written && !got.is_empty() {
            return Err(format!(
                "relation {id:?}: wrote {written:?}, resolved {got:?}"
            ));
        }
        if !got.is_empty() {
            remember_id(
                &mut model.relation_type_ids,
                row.relation_type.raw(),
                got.clone(),
                "relation type",
            )?;
        }
    }
    for name in PREDICATES {
        if let Some(row) = predicate_lookup_by_qname(&rtxn, DeclScope::base(NS), name)
            .map_err(|e| e.to_string())?
        {
            remember_id(
                &mut model.predicate_ids,
                row.id.raw(),
                qname(name),
                "predicate",
            )?;
        }
    }
    for name in RELATIONS {
        if let Some(row) = relation_type_lookup_by_qname(&rtxn, DeclScope::base(NS), name)
            .map_err(|e| e.to_string())?
        {
            remember_id(
                &mut model.relation_type_ids,
                row.id.raw(),
                qname(name),
                "relation type",
            )?;
        }
    }
    let listed = statement_list(&rtxn, scope(), &StatementListFilter::default())
        .map_err(|e| e.to_string())?;
    let paged = statement_list_page(
        &rtxn,
        scope(),
        &StatementListFilter::default(),
        &StatementPageExtra {
            include_tombstoned: true,
            ..StatementPageExtra::default()
        },
        None,
        32,
    )
    .map_err(|e| e.to_string())?;
    for (id, _) in &model.statements {
        if !listed.iter().any(|row| row.id == *id) && !paged.rows.iter().any(|row| row.id == *id) {
            return Err(format!(
                "statement {id:?} vanished from list/page read paths"
            ));
        }
    }
    let filter = RelationListFilter::default();
    let mut listed_relation_ids = std::collections::BTreeSet::new();
    for entity in entities {
        let from =
            relation_list_from(&rtxn, scope(), *entity, &filter).map_err(|e| e.to_string())?;
        let to = relation_list_to(&rtxn, scope(), *entity, &filter).map_err(|e| e.to_string())?;
        listed_relation_ids.extend(from.into_iter().map(|row| row.id));
        listed_relation_ids.extend(to.into_iter().map(|row| row.id));
    }
    for (id, _) in &model.relations {
        if relation_get(&rtxn, *id)
            .map_err(|e| e.to_string())?
            .is_some()
            && !listed_relation_ids.contains(id)
        {
            return Err(format!(
                "relation {id:?} is not surfaced by either list direction"
            ));
        }
    }
    Ok(())
}

fn apply(
    db: &MetadataDb,
    entities: &[EntityId],
    model: &mut Model,
    op: Op,
    trace: &mut Vec<String>,
) {
    let line = format!("{op:?}");
    let result: Result<String, String> = (|| match op {
        Op::UploadSchema { mask } => {
            let ps: Vec<_> = PREDICATES
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let rs: Vec<_> = RELATIONS
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let w = db.write_txn().unwrap();
            let before_p: Vec<_> = ps
                .iter()
                .map(|n| predicate_id_by_qname(&w, DeclScope::base(NS), n).map(|x| (*n, x)))
                .collect::<Result<_, _>>()
                .unwrap();
            let before_r: Vec<_> = rs
                .iter()
                .map(|n| relation_type_id_by_qname(&w, DeclScope::base(NS), n).map(|x| (*n, x)))
                .collect::<Result<_, _>>()
                .unwrap();
            let v = schema_upload(
                &w,
                &schema(&ps, &rs, None),
                brain_metadata::tables::scope::SPACE_BASE,
                NOW,
            )
            .map_err(model_rejection)?;
            for (n, old) in before_p {
                if let Some(old) = old {
                    let new = predicate_id_by_qname(&w, DeclScope::base(NS), n).unwrap();
                    if new != Some(old) {
                        return Err(format!(
                            "predicate {n} changed id from {} to {new:?}",
                            old.raw()
                        ));
                    }
                }
            }
            for (n, old) in before_r {
                if let Some(old) = old {
                    let new = relation_type_id_by_qname(&w, DeclScope::base(NS), n).unwrap();
                    if new != Some(old) {
                        return Err(format!(
                            "relation type {n} changed id from {} to {new:?}",
                            old.raw()
                        ));
                    }
                }
            }
            w.commit().unwrap();
            Ok(format!("version={v}"))
        }
        Op::ReplaceSchema {
            mask,
            changed,
            omitted,
        } => {
            let ps: Vec<_> = PREDICATES
                .iter()
                .enumerate()
                .filter(|(i, _)| !omitted || mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let rs: Vec<_> = RELATIONS
                .iter()
                .enumerate()
                .filter(|(i, _)| !omitted || mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect();
            let w = db.write_txn().unwrap();
            let v = schema(
                &ps,
                &rs,
                if changed {
                    Some(PREDICATES[(mask as usize) % PREDICATES.len()])
                } else {
                    None
                },
            );
            let before_p: Vec<_> = ps
                .iter()
                .map(|n| predicate_id_by_qname(&w, DeclScope::base(NS), n).map(|x| (*n, x)))
                .collect::<Result<_, _>>()
                .unwrap();
            let before_r: Vec<_> = rs
                .iter()
                .map(|n| relation_type_id_by_qname(&w, DeclScope::base(NS), n).map(|x| (*n, x)))
                .collect::<Result<_, _>>()
                .unwrap();
            let red =
                resolve_redeclarations(&w, &v, brain_metadata::tables::scope::SPACE_BASE, NOW)
                    .map_err(model_rejection)?;
            brain_metadata::schema::predicate::predicate_drop_schema_declared(
                &w,
                DeclScope::base(NS),
                &red.predicates,
            )
            .map_err(model_rejection)?;
            brain_metadata::relation::types::relation_type_drop_schema_declared(
                &w,
                DeclScope::base(NS),
                &red.relation_types,
            )
            .map_err(model_rejection)?;
            let n = schema_upload_with_mode(
                &w,
                &v,
                brain_metadata::tables::scope::SPACE_BASE,
                NOW,
                SourceMode::Replace,
            )
            .map_err(model_rejection)?;
            if !changed {
                for (name, old) in before_p {
                    if let Some(old) = old {
                        let new = predicate_id_by_qname(&w, DeclScope::base(NS), name)
                            .unwrap()
                            .map(|x| x.raw());
                        if new != Some(old.raw()) {
                            return Err(format!(
                                "redeclared predicate {name} changed id from {} to {new:?}",
                                old.raw()
                            ));
                        }
                    }
                }
                for (name, old) in before_r {
                    if let Some(old) = old {
                        let new = relation_type_id_by_qname(&w, DeclScope::base(NS), name)
                            .unwrap()
                            .map(|x| x.raw());
                        if new != Some(old.raw()) {
                            return Err(format!(
                                "redeclared {name} changed id from {} to {new:?}",
                                old.raw()
                            ));
                        }
                    }
                }
            }
            w.commit().unwrap();
            Ok(format!("version={n}"))
        }
        Op::DropPredicate { name, force } => {
            let w = db.write_txn().unwrap();
            let id = predicate_id_by_qname(&w, DeclScope::base(NS), PREDICATES[name]).unwrap();
            if let Some(id) = id {
                let referencing =
                    brain_metadata::statement::crud::statement_reference_count_by_predicate(
                        &w,
                        id,
                        usize::MAX,
                    )
                    .unwrap();
                if referencing != 0 && !force {
                    drop(w);
                    return Ok(format!(
                        "id={} referencing={referencing} skipped: reference gate refused drop",
                        id.raw()
                    ));
                }
            }
            let got = predicate_drop_one(&w, DeclScope::base(NS), PREDICATES[name])
                .map_err(model_rejection)?;
            w.commit().unwrap();
            Ok(format!("id={:?} force={force}", got.map(|x| x.raw())))
        }
        Op::DropRelationType { name, force } => {
            let w = db.write_txn().unwrap();
            let id = relation_type_id_by_qname(&w, DeclScope::base(NS), RELATIONS[name]).unwrap();
            if let Some(id) = id {
                let referencing = relation_reference_count_by_type(&w, id, usize::MAX).unwrap();
                if referencing != 0 && !force {
                    drop(w);
                    return Ok(format!(
                        "id={} referencing={referencing} skipped: reference gate refused drop",
                        id.raw()
                    ));
                }
            }
            let got = relation_type_drop_one(&w, DeclScope::base(NS), RELATIONS[name])
                .map_err(model_rejection)?;
            w.commit().unwrap();
            Ok(format!("id={:?} force={force}", got.map(|x| x.raw())))
        }
        Op::CreateStatement { name } => {
            let w = db.write_txn().unwrap();
            let pid = predicate_intern_or_get(&w, DeclScope::base(NS), PREDICATES[name], 0, NOW)
                .map_err(model_rejection)?;
            let s = statement_for(pid, entities[0]);
            let id = statement_create(&w, scope(), SessionId::DEFAULT, &s, NOW)
                .map_err(model_rejection)?;
            w.commit().unwrap();
            model.statements.push((id, qname(PREDICATES[name])));
            Ok(format!("sid={id:?} pid={}", pid.raw()))
        }
        Op::CreateRelation { name } => {
            let w = db.write_txn().unwrap();
            let ty = relation_type_id_by_qname(&w, DeclScope::base(NS), RELATIONS[name]).unwrap();
            let Some(ty) = ty else {
                drop(w);
                return Ok("type absent; skipped".into());
            };
            let r = relation_for(ty, entities[0], entities[1]);
            let id = relation_create(&w, scope(), SessionId::DEFAULT, &r, NOW)
                .map_err(model_rejection)?;
            w.commit().unwrap();
            model.relations.push((id, qname(RELATIONS[name])));
            Ok(format!("rid={id:?} type={}", ty.raw()))
        }
        Op::SupersedeStatement { slot } => {
            let Some((old, written)) = model
                .statements
                .get(slot % model.statements.len().max(1))
                .cloned()
            else {
                return Ok("no statement; skipped".into());
            };
            let rtxn = db.read_txn().unwrap();
            let Some(row) = statement_get(&rtxn, old).unwrap() else {
                return Ok("statement absent; skipped".into());
            };
            let mut next = row.clone();
            next.id = StatementId::new();
            drop(rtxn);
            let w = db.write_txn().unwrap();
            let id = statement_supersede(&w, scope(), SessionId::DEFAULT, old, &next, NOW)
                .map_err(model_rejection)?;
            w.commit().unwrap();
            model.statements.push((id, written));
            Ok(format!("old={old:?} new={id:?}"))
        }
        Op::SupersedeRelation { slot } => {
            let Some((old, written)) = model
                .relations
                .get(slot % model.relations.len().max(1))
                .cloned()
            else {
                return Ok("no relation; skipped".into());
            };
            let rtxn = db.read_txn().unwrap();
            let Some(row) = relation_get(&rtxn, old).unwrap() else {
                return Ok("relation absent; skipped".into());
            };
            let mut next = row.clone();
            next.id = RelationId::new();
            drop(rtxn);
            let w = db.write_txn().unwrap();
            let id = relation_supersede(&w, scope(), SessionId::DEFAULT, old, &next, NOW)
                .map_err(model_rejection)?;
            w.commit().unwrap();
            model.relations.push((id, written));
            Ok(format!("old={old:?} new={id:?}"))
        }
        Op::TombstoneStatement { slot } => {
            let Some((id, _)) = model
                .statements
                .get(slot % model.statements.len().max(1))
                .cloned()
            else {
                return Ok("no statement; skipped".into());
            };
            let w = db.write_txn().unwrap();
            statement_tombstone(&w, id, TombstoneReason::UserRequest, NOW)
                .map_err(model_rejection)?;
            w.commit().unwrap();
            Ok(format!("sid={id:?}"))
        }
        Op::TombstoneRelation { slot } => {
            let Some((id, _)) = model
                .relations
                .get(slot % model.relations.len().max(1))
                .cloned()
            else {
                return Ok("no relation; skipped".into());
            };
            let w = db.write_txn().unwrap();
            relation_tombstone(&w, id, NOW).map_err(model_rejection)?;
            w.commit().unwrap();
            Ok(format!("rid={id:?}"))
        }
    })();
    trace.push(format!("{line} => {}", result.unwrap_or_else(|e| e)));
}

fn run_sequence(ops: Vec<Op>) -> Result<(), String> {
    let (_dir, db) = open_db();
    let entities = [
        make_entity(&db, "alice"),
        make_entity(&db, "bob"),
        make_entity(&db, "carol"),
    ];
    let mut model = Model::default();
    let mut trace = Vec::new();
    for (step, op) in ops.into_iter().enumerate() {
        apply(&db, &entities, &mut model, op, &mut trace);
        if let Err(e) = assert_invariants(&db, &mut model, &entities) {
            return Err(format!(
                "invariant failure at step {step}: {e}\n--- operation trace ---\n{}",
                trace.join("\n")
            ));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]
    #[test]
    fn schema_mutation_preserves_data_names(ops in prop::collection::vec(arb_op(), 8..=60)) {
        let result = run_sequence(ops);
        if let Err(error) = result {
            prop_assert!(false, "{error}");
        }
    }
}

#[test]
fn dropping_the_highest_numbered_predicate_then_declaring_a_new_one_leaves_old_rows_named() {
    let (_dir, db) = open_db();
    let subject = make_entity(&db, "subject");
    let w = db.write_txn().unwrap();
    schema_upload(
        &w,
        &schema(&["prefers", "likes"], &[], None),
        brain_metadata::tables::scope::SPACE_BASE,
        NOW,
    )
    .unwrap();
    w.commit().unwrap();
    let mut model = Model::default();
    for name in ["prefers", "likes"] {
        let w = db.write_txn().unwrap();
        let p = predicate_intern_or_get(&w, DeclScope::base(NS), name, 0, NOW).unwrap();
        let s = statement_for(p, subject);
        let id = statement_create(&w, scope(), SessionId::DEFAULT, &s, NOW).unwrap();
        w.commit().unwrap();
        model.statements.push((id, qname(name)));
    }
    let w = db.write_txn().unwrap();
    predicate_drop_one(&w, DeclScope::base(NS), "likes").unwrap();
    schema_upload(
        &w,
        &schema(&["ghost"], &[], None),
        brain_metadata::tables::scope::SPACE_BASE,
        NOW,
    )
    .unwrap();
    w.commit().unwrap();
    assert_invariants(&db, &mut model, &[subject]).unwrap();
}

#[test]
fn replacing_a_schema_that_redeclares_the_same_names_keeps_its_rows_named() {
    let (_dir, db) = open_db();
    let subject = make_entity(&db, "subject");
    let other = make_entity(&db, "other");
    let initial = schema(&["prefers"], &["works_at"], None);
    let w = db.write_txn().unwrap();
    schema_upload(&w, &initial, brain_metadata::tables::scope::SPACE_BASE, NOW).unwrap();
    w.commit().unwrap();
    let w = db.write_txn().unwrap();
    let p = predicate_id_by_qname(&w, DeclScope::base(NS), "prefers")
        .unwrap()
        .unwrap();
    let r = relation_type_id_by_qname(&w, DeclScope::base(NS), "works_at")
        .unwrap()
        .unwrap();
    let sid = statement_create(
        &w,
        scope(),
        SessionId::DEFAULT,
        &statement_for(p, subject),
        NOW,
    )
    .unwrap();
    let rid = relation_create(
        &w,
        scope(),
        SessionId::DEFAULT,
        &relation_for(r, subject, other),
        NOW,
    )
    .unwrap();
    w.commit().unwrap();
    let w = db.write_txn().unwrap();
    let replacement = schema(&["prefers"], &["works_at"], None);
    let red = resolve_redeclarations(
        &w,
        &replacement,
        brain_metadata::tables::scope::SPACE_BASE,
        NOW,
    )
    .unwrap();
    brain_metadata::schema::predicate::predicate_drop_schema_declared(
        &w,
        DeclScope::base(NS),
        &red.predicates,
    )
    .unwrap();
    brain_metadata::relation::types::relation_type_drop_schema_declared(
        &w,
        DeclScope::base(NS),
        &red.relation_types,
    )
    .unwrap();
    schema_upload_with_mode(
        &w,
        &replacement,
        brain_metadata::tables::scope::SPACE_BASE,
        NOW,
        SourceMode::Replace,
    )
    .unwrap();
    w.commit().unwrap();
    let rtxn = db.read_txn().unwrap();
    let mut pq = PredicateQnames::new();
    let mut rq = RelationTypeQnames::new();
    assert_eq!(
        pq.resolve(&rtxn, statement_get(&rtxn, sid).unwrap().unwrap().predicate),
        qname("prefers")
    );
    assert_eq!(
        rq.resolve(
            &rtxn,
            relation_get(&rtxn, rid).unwrap().unwrap().relation_type
        ),
        qname("works_at")
    );
}
