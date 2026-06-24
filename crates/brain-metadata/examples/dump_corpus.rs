//! Read-only dump of a shard's `metadata.redb` — entities, statements,
//! relations, predicates, and row counts. Used to inspect what the write
//! path actually built on disk.
//!
//!   cargo run -p brain-metadata --example dump_corpus -- <path/to/metadata.redb>

use std::collections::HashMap;

use brain_core::{EntityTypeId, PredicateId, StatementObject, StatementValue, SubjectRef};
use brain_metadata::relation::ops::{relation_list_from, RelationListFilter};
use brain_metadata::relation::types::relation_type_get;
use brain_metadata::statement::{statement_list, StatementListFilter};
use brain_metadata::tables::entity::ENTITIES_TABLE;
use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::relation::RELATION_METADATA_TABLE;
use brain_metadata::tables::statement::STATEMENTS_TABLE;
use brain_metadata::{entity_list_by_type, predicate_get, predicate_list};
use redb::{
    Database, MultimapTableHandle, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableHandle,
};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_corpus <metadata.redb>");
    let db = Database::open(&path).expect("open redb");
    let rtxn = db.begin_read().expect("read txn");

    // ---- row counts -----------------------------------------------------
    let count = |name: &str, len: u64| println!("  {name:<28} {len}");
    println!("== table row counts ==");
    count(
        "memories",
        rtxn.open_table(MEMORIES_TABLE)
            .map(|t| t.len().unwrap())
            .unwrap_or(0),
    );
    count(
        "entities",
        rtxn.open_table(ENTITIES_TABLE)
            .map(|t| t.len().unwrap())
            .unwrap_or(0),
    );
    count(
        "statements",
        rtxn.open_table(STATEMENTS_TABLE)
            .map(|t| t.len().unwrap())
            .unwrap_or(0),
    );
    count(
        "relations",
        rtxn.open_table(RELATION_METADATA_TABLE)
            .map(|t| t.len().unwrap())
            .unwrap_or(0),
    );

    // ---- full table inventory (every redb table + row count) ------------
    println!("\n== ALL redb tables (name : rows) ==");
    let mut tnames: Vec<(String, u64)> = Vec::new();
    if let Ok(handles) = rtxn.list_tables() {
        for h in handles {
            let name = h.name().to_string();
            let len = rtxn
                .open_untyped_table(h)
                .map(|t| t.len().unwrap_or(0))
                .unwrap_or(0);
            tnames.push((name, len));
        }
    }
    if let Ok(handles) = rtxn.list_multimap_tables() {
        for h in handles {
            let name = format!("{} (multimap)", h.name());
            let len = rtxn
                .open_untyped_multimap_table(h)
                .map(|t| t.len().unwrap_or(0))
                .unwrap_or(0);
            tnames.push((name, len));
        }
    }
    tnames.sort();
    for (name, len) in &tnames {
        let marker = if *len > 0 { "" } else { "  (empty)" };
        println!("  {name:<44} {len}{marker}");
    }

    // ---- predicate qname lookup table -----------------------------------
    let preds = predicate_list(&rtxn, None).expect("predicate_list");
    let qname_of: HashMap<u32, String> = preds
        .iter()
        .map(|p| (p.id.raw(), format!("{}:{}", p.namespace, p.name)))
        .collect();

    // ---- entities (by type) ---------------------------------------------
    println!("\n== entities ==");
    let type_rows: Vec<(u32, String)> = {
        let t = rtxn.open_table(ENTITY_TYPES_TABLE).expect("types");
        t.iter()
            .unwrap()
            .filter_map(|e| e.ok().map(|(k, v)| (k.value(), v.value().name)))
            .collect()
    };
    for (tid, tname) in &type_rows {
        let ents = entity_list_by_type(&rtxn, EntityTypeId::from(*tid)).unwrap_or_default();
        for e in ents {
            println!("  [{tname}] {}", e.canonical_name);
        }
    }

    // ---- statements -----------------------------------------------------
    println!("\n== statements (subject — predicate — object [kind] @event) ==");
    let stmts = statement_list(&rtxn, &StatementListFilter::default()).expect("statement_list");
    let name_of = |id: brain_core::EntityId| -> String {
        let t = rtxn.open_table(ENTITIES_TABLE).ok();
        t.and_then(|t| t.get(&id.to_bytes()).ok().flatten().map(|g| g.value().canonical_name))
            // An entity id with no row is the agent self-entity (first-person
            // facts use `EntityId::from(agent_id)` without minting a row, the
            // same identity MATERIALIZE_PROCEDURAL reads) or another unrooted id.
            .unwrap_or_else(|| format!("<self/agent {id:?}>"))
    };
    for s in &stmts {
        let subj = match s.subject {
            SubjectRef::Entity(e) => name_of(e),
            SubjectRef::Memory(_) => "<memory>".into(),
            SubjectRef::Pending(_) => "<pending>".into(),
        };
        let pred = qname_of
            .get(&s.predicate.raw())
            .cloned()
            .or_else(|| {
                predicate_get(&rtxn, PredicateId::from(s.predicate.raw()))
                    .ok()
                    .flatten()
                    .map(|p| format!("{}:{}", p.namespace, p.name))
            })
            .unwrap_or_else(|| format!("pred#{}", s.predicate.raw()));
        let obj = match &s.object {
            StatementObject::Entity(e) => format!("[E] {}", name_of(*e)),
            StatementObject::Value(StatementValue::Text(t)) => format!("\"{t}\""),
            StatementObject::Value(StatementValue::UnixNanos(n)) => format!("@{n}"),
            StatementObject::Value(v) => format!("{v:?}"),
            other => format!("{other:?}"),
        };
        let ev = s
            .event_at_unix_nanos
            .map(|n| format!(" @event={n}"))
            .unwrap_or_default();
        println!("  {subj} — {pred} — {obj} [{:?}]{ev}", s.kind);
    }

    // ---- relations (entity ↔ entity, the relations table) ---------------
    println!("\n== relations (from — type — to) ==");
    let rt_name = |id: brain_core::RelationTypeId| -> String {
        relation_type_get(&rtxn, id)
            .ok()
            .flatten()
            .map(|rt| format!("{}:{}", rt.namespace, rt.name))
            .unwrap_or_else(|| format!("rtype#{}", id.raw()))
    };
    let mut seen_rel = std::collections::HashSet::new();
    for (tid, _) in &type_rows {
        for e in entity_list_by_type(&rtxn, EntityTypeId::from(*tid)).unwrap_or_default() {
            for r in
                relation_list_from(&rtxn, e.id, &RelationListFilter::default()).unwrap_or_default()
            {
                if seen_rel.insert(r.id) {
                    println!(
                        "  {} — {} — {}",
                        name_of(r.from_entity),
                        rt_name(r.relation_type),
                        name_of(r.to_entity)
                    );
                }
            }
        }
    }

    // ---- coined predicates (non-behavior) -------------------------------
    println!("\n== predicates coined this run (excluding seeded behavior_*) ==");
    for p in &preds {
        if p.namespace == "brain" && p.name.starts_with("behavior_") {
            continue;
        }
        println!("  {}:{}", p.namespace, p.name);
    }
}
