# Knowledge SDK

## Phase scope

The SDK lands across multiple phases:

| Phase | Scope | Status |
|---|---|---|
| 16.8 | Hand-written `Entity` helpers for the built-in `Person` type — all 9 entity opcodes (CREATE / GET / UPDATE / RENAME / MERGE / UNMERGE / RESOLVE / LIST / TOMBSTONE). No derive macro. | complete |
| 17.8 | Statement helpers (Fact / Preference / Event builders) — `client.fact()` / `.preference()` / `.event()` / `.statements()`. Hand-written; no derive macro. | complete |
| 18.8 | Relation helpers — `client.relation::<T>()` / `.relations()` + traversal builder. Hand-written; derive macro deferred to phase 19. | complete |
| 19 | `BrainEntity` / `BrainRelation` / `BrainFact` derive macros + programmatic `SchemaBuilder` + `client.schema().upload() / .validate() / .get() / .list()` (depends on schema DSL). | this phase |
| 22-23 | Fluent query builder + subscribe extensions. | later |

The examples below show the **target** ergonomics (post-derive-macro). The 16.8 implementation provides the same opcode coverage via hand-written builders against the `Person` type defined in `brain_core::knowledge::EntityType::PERSON_ID`. Phase 19's derive macro generalises the surface to any user-declared type.

## What the SDK exposes

The Rust SDK (`brain-sdk-rust`) exposes:
- Typed entity APIs (Entity, EntityType).
- Typed statement APIs (Fact, Preference, Event).
- Typed relation APIs.
- Schema definition helpers.
- A fluent query builder for hybrid queries.

the substrate SDK calls (encode, recall, plan, forget) continue to work unchanged.

## Crate structure

```
brain-sdk-rust/
  src/
    lib.rs                  ── re-exports
    client.rs               ── Connection, retry, transport
    substrate/                ── memory operations (encode, recall, plan, reason, forget)
    knowledge/
      schema.rs             ── schema builder (programmatic alternative to DSL)
      entity.rs             ── Entity, EntityType helpers
      statement.rs          ── Fact, Preference, Event builders
      relation.rs           ── Relation builders
      query.rs              ── fluent query builder
      types.rs              ── Value, ObjectRef, etc.
      derive.rs             ── derive macros (BrainEntity, BrainFact)
```

## Typed entity API

```rust
use brain_sdk::knowledge::*;

#[derive(BrainEntity)]
#[brain(entity_type = "Person", namespace = "acme")]
struct Person {
    #[brain(required, unique)]
    email: String,
    role: Option<String>,
    team: Option<String>,
}

// Create
let priya = client.entity::<Person>()
    .canonical_name("Priya Patel")
    .alias("Priya")
    .alias("priya@example.com")
    .with(|p| {
        p.email = "priya@example.com".into();
        p.role = Some("Engineering Manager".into());
    })
    .create()
    .await?;

// Lookup
let entity = client.entity::<Person>()
    .get(priya.id)
    .await?;

// Resolve
let resolved = client.entity::<Person>()
    .resolve("Priya")
    .with_context("the engineering manager")
    .await?;

// Update
client.entity(priya.id)
    .rename("Priya Singh")
    .set(|p| p.team = Some("Platform".into()))
    .commit()
    .await?;
```

The derive macro generates: serialization, schema metadata, type-safe constructors, attribute validators.

## Typed statement API

```rust
// Facts
let fact = client.fact()
    .subject(priya.id)
    .predicate("role")
    .object_value("Engineering Manager")
    .evidence(vec![mem_x, mem_y])
    .confidence(0.9)
    .create()
    .await?;

// Preferences
let pref = client.preference()
    .subject(priya.id)
    .predicate("prefers")
    .object_value("async meetings")
    .evidence(vec![mem_z])
    .confidence(0.87)
    .create()
    .await?;

// Events
let event = client.event()
    .subject(priya.id)
    .predicate("scheduled")
    .object_value("planning session")
    .event_at(time)
    .evidence(vec![mem_w])
    .confidence(0.95)
    .create()
    .await?;

// Query statements
let prefs = client.statements()
    .where_subject(priya.id)
    .of_kind(StatementKind::Preference)
    .current_only()
    .with_min_confidence(0.7)
    .list()
    .await?;

// Supersede
let new_pref = client.preference()
    .subject(priya.id)
    .predicate("prefers")
    .object_value("written agendas")
    .evidence(vec![mem_new])
    .confidence(0.92)
    .supersedes(pref.id)
    .create()
    .await?;
```

## Typed relation API

```rust
#[derive(BrainRelation)]
#[brain(from = "Person", to = "Person", cardinality = "many-to-one")]
struct ReportsTo;

let rel = client.relation::<ReportsTo>()
    .from(bob.id)
    .to(priya.id)
    .evidence(vec![mem_a])
    .confidence(0.95)
    .create()
    .await?;

// Traverse
let reports = client.relation::<ReportsTo>()
    .traverse_from(priya.id)
    .reverse_direction()             // who reports TO priya
    .depth(2)
    .execute()
    .await?;
```

## Fluent query builder

```rust
// Hybrid query
let results = client.query()
    .text("budget pushback from leadership")
    .with_entity(priya.id)
    .of_kinds(&[StatementKind::Fact, StatementKind::Event])
    .where_time(TimeRange::last(Duration::days(30)))
    .with_min_confidence(0.6)
    .retrievers(Retrievers::Auto)          // let router decide
    .limit(20)
    .execute()
    .await?;

for result in results {
    match result.item {
        ItemRef::Statement(s) => { /* ... */ },
        ItemRef::Memory(m) => { /* ... */ },
        ItemRef::Relation(r) => { /* ... */ },
        ItemRef::Entity(e) => { /* ... */ },
    }
    // result.contributing_retrievers: explain which retrievers surfaced this
}

// Explain (no execution)
let plan = client.query()
    .text("budget pushback")
    .with_entity(priya.id)
    .explain()
    .await?;

println!("Plan: {}", plan.summary);
println!("Estimated cost: {} ms", plan.estimated_cost_ms);

// Trace (execute with debug info)
let traced = client.query()
    .text("...")
    .trace()
    .await?;

for retriever in traced.retriever_traces {
    println!("{}: {} ms, {} items, top: {:?}",
             retriever.name, retriever.latency_ms,
             retriever.total_items, retriever.top_3);
}
```

## Schema management

Programmatic alternative to the DSL:

```rust
let schema = SchemaBuilder::new("acme")
    .entity_type::<Person>()
    .entity_type::<Project>()
    .predicate("role", StatementKind::Fact, ObjectType::Value(ValueType::Text))
    .predicate("prefers", StatementKind::Preference, ObjectType::Value(ValueType::Text))
    .relation_type::<ReportsTo>()
    .extractor(
        ExtractorBuilder::pattern("person_mentions")
            .target_entity_type::<Person>()
            .pattern(r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2})\b")
            .confidence(0.7)
    )
    .extractor(
        ExtractorBuilder::llm("preferences")
            .target_statement_kind(StatementKind::Preference)
            .model("claude-haiku-4-5")
            .prompt("...")
            .schema_json(include_str!("preference_schema.json"))
            .cache_enabled(true)
    )
    .build()?;

client.schema().upload(&schema).await?;
```

The derive macros on `Person` and `Project` contribute their entity-type definitions automatically.

DSL text upload also supported:

```rust
let schema_text = std::fs::read_to_string("schema.brain")?;
client.schema().upload_text(&schema_text).await?;
```

## Subscribe extensions

```rust
let mut stream = client.subscribe()
    .events(&[
        EventKind::StatementCreated,
        EventKind::EntityMerged,
        EventKind::ExtractionFailed,
    ])
    .filter_entity(priya.id)             // only events involving Priya
    .start()
    .await?;

while let Some(event) = stream.next().await {
    match event {
        Event::StatementCreated { id, kind, subject, predicate, ... } => { ... },
        ...
    }
}
```

## Compatibility

the substrate SDK code continues to work:

```rust
// substrate-only style — works without a schema
let mid = client.encode("Priya likes async meetings").await?;
let memories = client.recall("Priya preferences").await?;
```

When a schema is declared, RECALL goes through the hybrid retriever. Returns the same `Vec<Memory>` shape; metadata fields gain new fields (contributing_retrievers, etc.) but old fields stay.

## Error handling

```rust
match client.entity::<Person>().resolve("Priya").await {
    Ok(ResolutionOutcome::Resolved { entity, confidence, .. }) => { ... },
    Ok(ResolutionOutcome::Ambiguous { candidates, .. }) => {
        // ask user to disambiguate
    },
    Ok(ResolutionOutcome::Created { entity }) => { ... },
    Err(BrainError::EntityTypeMismatch { .. }) => { ... },
    Err(e) => { ... },
}
```

All error types are typed (no string-matching) and serialize cleanly across the wire.
