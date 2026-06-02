# 02.07 Statement

A **Statement** is a typed claim about an entity, carrying its own kind (Fact, Preference, Event), provenance, confidence, evidence list, and validity period. This file collects the statement record shape together with the supersession, contradiction, confidence-aggregation, and evidence-handling mechanics that drive its full lifecycle.

## Why three statement kinds

### Fact, Preference, Event

The typed graph distinguishes three statement kinds in its API. Internally they share a common storage schema; the distinction lives in mutation rules, default validity semantics, and query intent.

| Kind | What it captures | Mutation policy | Time default |
|---|---|---|---|
| **Fact** | Stable claims about the world. "Priya is the engineering manager." | Append-only, contradictable by higher-confidence newer Facts. | Valid-from extraction time, no valid-to until contradicted. |
| **Preference** | Revisable beliefs/choices. "Priya prefers async meetings." | Versioned via supersession. New preference supersedes old. | Valid-from extraction time, valid-to = `superseded_by.extracted_at`. |
| **Event** | Discrete occurrences at a moment. "Priya scheduled a planning session on Tuesday." | Immutable. Corrections add a new Event, never modify. | `event_at` is the moment; no valid range. |

### Why distinguish them

The three kinds are not three storage schemas. They are three *contracts* on top of one storage schema.

#### Mutation contracts differ

When new evidence arrives, what happens to existing data depends on the kind:

- **Fact + new contradicting Fact**: both stored. The planner returns the higher-confidence (or more recent at equal confidence) one. The contradicted Fact stays in the audit trail.
- **Preference + new Preference (same subject, predicate)**: the new one supersedes. The old one's `superseded_by` points to the new one. Queries for "current preference" return only the new one. History queries return the chain.
- **Event + similar Event**: stored as a second, independent Event. Events do not supersede each other — that's their whole point. If the user got the date wrong on the first Event, they record a corrective Fact ("the meeting was Wednesday, not Tuesday"), or insert a new Event with provenance noting the correction.

#### Query intent differs

Users ask different questions about each kind:

| Query pattern | Kind |
|---|---|
| "Who is X?" / "What is the role of X?" | Fact |
| "What does X prefer?" / "How does X like things done?" | Preference |
| "What did X do?" / "What happened on date D?" / "Show me the timeline." | Event |

If the user asks "what does Priya prefer," they almost certainly don't want a Fact about Priya's job title even if it has higher confidence. The kind filter resolves this without requiring elaborate query syntax.

#### Storage is unified

Internally, all three are rows in a single `statements` table:

```rust
struct StatementRow {
    id: StatementId,
    kind: StatementKind,      // Fact | Preference | Event
    subject: EntityId,
    predicate: PredicateId,
    object: StatementObject,
    confidence: f32,
    evidence: Vec<MemoryId>,
    extractor_id: ExtractorId,
    extracted_at: u64,
    
    // Time fields (kind-dependent meaning)
    valid_from: Option<u64>,      // Fact, Preference: when this becomes true
    valid_to: Option<u64>,        // Fact, Preference: when it stops being true
    event_at: Option<u64>,        // Event: when it occurred
    
    // Versioning (Preference only typically)
    version: u32,
    superseded_by: Option<StatementId>,
}
```

`kind` is the first column of compound indexes: queries that filter by kind don't pay for scanning other kinds' rows. Cross-kind queries still work; per-kind queries are fast.

### Why not just one statement type with a "mutability" flag?

Brain considered this. The argument for one type: simpler schema, fewer concepts to teach.

The argument against (and why three kinds won):

1. **Users mentally categorize**. "Fact vs preference vs event" maps to how people think about knowledge. A single type with flags doesn't help them at the schema level.

2. **Extractor schemas differ**. A pattern extractor producing Events ("met with X on T") has a different output shape from one producing Preferences ("X likes Y"). Strong typing prevents extractor misuse.

3. **Default query behavior differs**. By default, "Preference" queries return current versions only; "Event" queries return all events in a range; "Fact" queries return highest-confidence non-superseded. Defaults need a type to attach to.

4. **Validation differs**. An Event must have an `event_at`. A Preference can be superseded; a Fact's `superseded_by` field is unused. The validator's life is easier with three kinds.

Three kinds, one storage. The API surface carries the distinction; the implementation shares everything.

### Why not more kinds (e.g., Observation, Goal, Rule)?

Each kind adds:
- API surface (a new constructor, a new query filter)
- Validation logic
- Documentation burden
- Test surface

The benefit of a new kind is that users would otherwise abuse one of the existing three to express it. So the test is: can existing kinds express it cleanly with minor schema work?

| Candidate | Can existing kinds express it? |
|---|---|
| **Observation** ("I observed X") | Yes — an Event with predicate=observed. |
| **Goal** ("I want X") | Yes — a Preference with predicate=wants, or a Fact with predicate=goal. |
| **Rule** ("If X then Y") | No — but Rules are not facts about entities; they're programs. Belongs in the extractor / planner layer, not as statements. |
| **Hypothesis** ("X might be true") | Yes — a Fact with low confidence. |
| **Contradiction-marker** ("X and Y disagree") | Yes — a Fact with predicate=contradicts, subject=X, object=Y. |

Three kinds are enough. Brain resists the temptation to add more. If a user has a genuine sixth-kind use case, they encode it in `predicate` and tag with `kind=Fact` or whichever has the right mutation contract.

### Special case: contradicting Facts

Two Facts with the same `(subject, predicate)` but different `object` are *contradictions*, not supersessions. Both are stored. The planner exposes the conflict:

```rust
struct ContradictionView {
    statements: Vec<Statement>,    // all conflicting statements
    highest_confidence: StatementId,
    recommendation: ConflictResolution,  // by_confidence | by_recency | unresolved
}
```

This is one of the things a memory database *should* surface, not hide. The user (or an upstream agent) decides how to resolve it. Surfacing contradictions is a feature, not a bug.

### Special case: deleting a Preference

You don't. You record a new Preference that *supersedes* it, with the new object being a sentinel (`null` or `"none"` or whatever the schema permits). The supersession chain stays intact. If someone wants to know what Priya used to prefer and stopped preferring, the history is there.

If the user genuinely wants the Preference gone forever (e.g., they encoded it by accident, or for privacy reasons), they invoke `FORGET_STATEMENT` (hard, with the same grace-period semantics as Brain's hard FORGET on memories). This is rare and audited.

## Full statement row

The unified row carries every field needed across all three kinds. Kind-specific contracts decide which fields are populated and how mutations behave.

```rust
struct Statement {
    id: StatementId,                 // UUIDv7
    kind: StatementKind,             // Fact | Preference | Event

    // Subject + predicate are required for all kinds
    subject: SubjectRef,             // EntityId or Pending(AuditId)
    predicate: PredicateId,          // interned namespaced string

    // Object: tagged union
    object: StatementObject,

    // Provenance and confidence (all kinds)
    confidence: f32,                 // [0, 1]
    evidence: EvidenceRef,           // inline (Vec<MemoryId>) or overflow pointer
    extractor_id: ExtractorId,
    extracted_at: u64,               // unix micros
    schema_version: u32,             // version of schema under which this was extracted

    // Time fields (kind-dependent)
    valid_from: Option<u64>,         // Fact, Preference
    valid_to: Option<u64>,           // Fact, Preference (None = still valid)
    event_at: Option<u64>,           // Event (required for Event kind)

    // Versioning
    version: u32,                    // increments on supersession
    superseded_by: Option<StatementId>,
    supersedes: Option<StatementId>, // back-pointer; for chain traversal

    // Status flags
    tombstoned: bool,                // soft-delete
    tombstoned_at: Option<u64>,
    tombstone_reason: Option<TombstoneReason>,
}

enum StatementObject {
    Entity(EntityId),
    Value(rkyv_Value),               // typed literal
    Memory(MemoryId),
    Statement(StatementId),          // meta-statement
}

enum SubjectRef {
    Entity(EntityId),
    Pending(AuditId),                // resolution pending
}

enum EvidenceRef {
    Inline(SmallVec<MemoryId, 8>),   // up to 8 inline
    Overflow(EvidenceOverflowId),    // points to evidence_overflow table
}

enum TombstoneReason {
    SourceMemoryForgotten,
    UserRequest,
    SchemaInvalidation,
    ExtractorRetraction,
}
```

The unified row averages ~256 bytes (fixed fields ~160, object tagged union typically 16–64, inline evidence 0–128). For deployments with very large evidence lists, the `evidence_overflow` table absorbs the size.

## Kind-specific contracts

### Fact

- **Object**: typically `Entity` (predicate=role, predicate=manages) or `Value` (predicate=email, predicate=birth_year).
- **`valid_from`**: optional; if not provided, defaults to `extracted_at`.
- **`valid_to`**: optional; remains `None` until contradicted or explicitly retracted.
- **`event_at`**: must be `None`.
- **Contradiction handling**: another Fact with same `(subject, predicate)` but different `object`, where the timestamps suggest concurrency (overlapping valid intervals), is a *contradiction*. Both are stored. Queries by default return the higher-confidence one and indicate the contradiction.
- **Supersession**: rare; only when an explicit "this was wrong" Fact retracts.

### Preference

- **Object**: typically `Value` (predicate=prefers with object="async_meetings") or `Entity`.
- **`valid_from`**: defaults to `extracted_at`.
- **`valid_to`**: defaults to `superseded_by.extracted_at` when superseded.
- **`event_at`**: must be `None`.
- **Supersession**: new Preference with same `(subject, predicate)` *supersedes* the previous one. The previous one's `superseded_by` is set; new one's `supersedes` points back. Chain is queryable.
- **Default query**: only return current (non-superseded) Preferences.

### Event

- **Object**: typically `Entity` (predicate=scheduled, predicate=met_with) or `Value` (predicate=said, with the said-thing as value).
- **`event_at`**: required.
- **`valid_from`, `valid_to`**: must be `None` (Events are point-in-time, not validity ranges).
- **Supersession**: not allowed. Corrections are new Events with provenance noting the correction.
- **Default query**: events in a time range, sorted by `event_at`.

## Predicate vocabulary

Predicates are namespaced strings interned into a `predicates` table:

```rust
struct Predicate {
    id: PredicateId,
    namespace: String,               // e.g. "brain", "user", "crm"
    name: String,                    // e.g. "prefers", "role", "scheduled"
    kind_constraint: Option<StatementKind>,  // if set, only valid for this kind
    object_type_constraint: Option<ObjectTypeConstraint>,
    schema_version: u32,
    description: String,
}
```

Namespacing prevents collisions across schemas. Two deployments using `prefers` for different semantics can be disambiguated by namespace (`brain:prefers` vs `crm:prefers`).

**Built-in predicates** (namespace `brain:`):
- `is_a` — entity type assertion (e.g., Fact: (Priya, is_a, Person))
- `has_name` — entity naming
- `mentions` — generic Fact
- `related_to` — generic Relation

**User-declared predicates**: declared in the schema DSL. See the schema section.

### Predicate origin

Every interned predicate carries a `SchemaOrigin` tag indicating how it entered the registry:

```rust
enum SchemaOrigin {
    SchemaDeclared { version: u32 },           // declared by SCHEMA_UPLOAD; subject to strict validation
    ImplicitFromWrite { first_seen_lsn: u64 }, // interned on first STATEMENT_CREATE in open-vocabulary mode
}
```

- `SchemaDeclared`: the predicate was introduced by a `SCHEMA_UPLOAD` at the given version. Its `kind_constraint`, `object_type_constraint`, and supersession contract are enforced; unknown qnames sent in this namespace are rejected with `PredicateNotInSchema` (0x004B).
- `ImplicitFromWrite`: the predicate was interned the first time a `STATEMENT_CREATE` named it in a namespace without an active schema. Implicit predicates have no kind or object-type constraint and never enforce supersession on writes. Statements written against an implicit predicate carry the `IMPLICIT_PREDICATE` flag for tooling visibility.

When a `SCHEMA_UPLOAD` declares a predicate whose qname already exists with `ImplicitFromWrite`, the existing `PredicateId` is preserved and the origin flips to `SchemaDeclared{version}`. Previously written statements keep their stored ids; the post-upload flagging sweep marks rows whose predicate is not in the new active schema with `OUTSIDE_ACTIVE_SCHEMA`.

**Object type constraint** lets the schema enforce that, e.g., `manages` always has an `Entity<Person>` as object:

```
define predicate manages {
    kind: Fact
    object: Entity<Person>
}
```

## Statement supersession

How a statement is replaced by a new version while preserving the audit chain. The mechanism backs `STATEMENT_SUPERSEDE` (`0x0142`) wire op + the auto-supersession that fires on `STATEMENT_CREATE` for Preferences.

Cross-references:
- §"Kind-specific contracts" above — which kinds allow supersession.
- [`../10_metadata/00_purpose.md`](../10_metadata/00_purpose.md) — broader provenance + versioning model.
- [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §5 — `STATEMENT_SUPERSEDE` wire shape.

### The data model

Each `Statement` carries three supersession-related fields:

```rust
struct Statement {
    // ...
    version: u32,                          // 1 for chain root; +1 per supersession
    superseded_by: Option<StatementId>,    // forward link
    supersedes: Option<StatementId>,       // back-pointer
    // ...
}
```

Plus a derived `chain_root`: the `StatementId` of the original (first) statement in the chain. **Stored as a separate index entry** in `STATEMENT_CHAIN_TABLE` keyed by `(chain_root, version) → StatementId.to_bytes()` (see [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md)).

The chain is therefore queryable two ways:

- **By starting from any statement in the chain:** walk forward via `superseded_by` or backward via `supersedes`.
- **By chain root:** range-scan `STATEMENT_CHAIN_TABLE` at prefix `(chain_root, *)` — returns the full chain in version order.

The second form is what `STATEMENT_HISTORY` exposes.

### Which kinds support supersession

| Kind | Supersession allowed | Trigger |
|---|---|---|
| Fact | Explicit only (rare; "this Fact was wrong") | `STATEMENT_SUPERSEDE` opcode |
| Preference | Yes (the common case) | Auto-fires on `STATEMENT_CREATE` of a Preference with same `(subject, predicate)` |
| Event | **No** — Events are point-in-time; corrections are new Events with provenance notes | (would be rejected) |

The kind-specific rule lives at the validation layer in `statement_ops::statement_create` and `::statement_supersede`. Wire shape ([`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §5) doesn't enforce this; the handler does.

#### Why Preferences auto-supersede

A Preference like `(Priya, prefers, async_meetings)` represents a **current belief**. When a new Preference with the same `(subject, predicate)` arrives, the previous one is no longer current — it's history. Brain's job is to **keep the chain intact**, not pick winners.

So on `STATEMENT_CREATE` for kind=Preference:

1. Look up the current Preference with same `(subject, predicate)` via `statements_by_subject` (filter: `kind=Preference, superseded_by IS NULL, tombstoned=false, valid_to_in_range(now)`).
2. If one exists: supersede it atomically inside the same redb txn that creates the new one.
3. If none: just create.

This auto-step keeps callers from having to issue two opcodes for the common case.

#### Why Facts don't auto-supersede

A new Fact with same `(subject, predicate)` but **different object** is a **contradiction**, not a supersession — both are stored. See §"Contradiction handling" below. The resolver / human decides which is right.

A new Fact with same `(subject, predicate)` and **same object** is a duplicate. The wire-side `request_id` idempotency layer typically dedupes; if it falls through, the handler returns `Conflict` rather than auto-superseding (no signal that the new is "better" than the old).

Explicit `STATEMENT_SUPERSEDE` on a Fact says "I'm replacing this; here's the new statement" — caller takes responsibility.

#### Why Events never supersede

Events are point-in-time records. "Priya scheduled the planning meeting at 14:00" is a fact about a moment; if it was wrong, you don't *replace* it, you author a new Event ("correction: scheduled at 15:00, prior record was wrong") and the original stays as a record of what was thought at that time.

`STATEMENT_SUPERSEDE` on an Event returns `INVALID_ARGUMENT`.

### Mechanics — `statement_supersede(old_id, new_statement, now)`

Single redb write transaction. All steps atomic:

```text
1. Load old statement.
2. Pre-conditions:
   - old must exist                                    → STATEMENT_NOT_FOUND
   - old.tombstoned must be false                      → INVALID_ARGUMENT
   - old.superseded_by must be None                    → INVALID_ARGUMENT (already superseded)
   - old.kind must not be Event                        → INVALID_ARGUMENT
   - new_statement.kind must equal old.kind            → INVALID_ARGUMENT
   - new_statement.subject must equal old.subject      → INVALID_ARGUMENT
   - new_statement.predicate must equal old.predicate  → INVALID_ARGUMENT
   - new_statement.id must not exist yet               → IdempotencyConflict
3. Allocate new statement_id (UUIDv7).
4. Compute chain_root:
   - if old.supersedes.is_none():   chain_root = old.id
   - else:                          chain_root = STATEMENT_CHAIN_TABLE.lookup(old.id).chain_root
5. Compute version:
   - version = old.version + 1
6. Set fields:
   - new.version = version
   - new.supersedes = Some(old.id)
   - new.chain_root = chain_root
   - new.superseded_by = None
7. Insert new into STATEMENTS_TABLE + all secondary indexes (per
   spec/02_data_model/../10_metadata/03_substrate_tables.md).
8. Update old in place:
   - old.superseded_by = Some(new.id)
   - if old has valid_to_unix_nanos field (Fact / Preference):
       old.valid_to = new.extracted_at
   - old.record_invalidated_at_unix_nanos = Some(now)  // record time end (see ../10_metadata/03_substrate_tables.md §1.1a)
   - re-index old in STATEMENTS_BY_SUBJECT_TABLE since the
     `is_current` bit (key column 4) flipped from 1 to 0.
9. Insert (chain_root, version) → new.id into STATEMENT_CHAIN_TABLE.
10. Commit.
11. Post-commit: emit STATEMENT_SUPERSEDED event on the SUBSCRIBE
    channel with old_id, new_id, chain_root.
```

#### The `is_current` bit in `STATEMENTS_BY_SUBJECT_TABLE`

The key shape is `(subject, kind, predicate_id, is_current)` where `is_current` is `1` iff `superseded_by.is_none() && !tombstoned && valid_at(now)`. The bit lets the "current state" query be a point-lookup at `(subject, kind, predicate_id, 1)` rather than a scan-and-filter.

When `statement_supersede` flips `old.superseded_by` to `Some(_)`, it must also flip `old`'s entry in this index from `is_current=1` to `is_current=0`. That's a remove + insert in the same redb txn.

#### valid_to inheritance

For Fact / Preference, `valid_to` defaults to `None` (open-ended). When `old` is superseded by `new`, Brain sets `old.valid_to = new.extracted_at_unix_nanos` (i.e. "old was valid up until new arrived").

If `old` had an explicit `valid_to_unix_nanos != 0` set by the caller (e.g. "this fact was only valid through end of 2026"), Brain **preserves** that value rather than overwriting — the explicit constraint wins.

The supersede logic:

```text
if old.valid_to_unix_nanos == 0 and old.kind != Event:
    old.valid_to_unix_nanos = new.extracted_at_unix_nanos
```

Events have `valid_to_unix_nanos = 0` permanently (events are point-in-time per §00); the rule above is gated on kind for that reason.

### Chain traversal

`STATEMENT_HISTORY` (opcode `0x0145`) walks the chain in version order:

```text
For chain_root = anchor_id_or_followed_chain_root:
    range-scan STATEMENT_CHAIN_TABLE at prefix (chain_root, *)
    for each (chain_root, version) -> statement_id:
        load Statement
        emit
```

Returns the full chain ordered by `version` ascending. The wire-side shape is in [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §8.

#### Anchor flexibility

`STATEMENT_HISTORY` accepts **either** a chain root id **or** any statement id in the chain:

```text
if anchor exists in STATEMENT_CHAIN_TABLE as key.0 (i.e. it's a chain_root):
    use anchor as chain_root directly.
else:
    load Statement(anchor); use anchor.chain_root.
```

This lets callers pass `superseded_by`-chained ids without having to find the root first.

### Lookup performance

The `(chain_root, version)` index gives:

- Full-chain history: 1 prefix scan, O(version count) seeks.
- Current statement (version = max): 1 reverse prefix scan, O(1) effective.
- N-th version: 1 point lookup.

Brain's redb b-tree has predictable per-seek cost (~µs). Even 100-version chains traverse in under 1 ms.

### Versioning invariants

For any chain:

- Versions are dense `1..=N` (no gaps).
- Exactly one statement per chain has `superseded_by.is_none() && !tombstoned` (the "current" entry). This is the one returned by `is_current=1` index lookups.
- After tombstoning the current entry, *no* statement in the chain has `is_current=1`. Queries by `(subject, predicate)` return empty.
- After unretiring (a hypothetical future op — not in v1.0): re-flips `is_current=1`. Tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

These invariants are enforced by `statement_ops` and verified by unit tests in the statement-ops module.

### Cross-shard supersession

Statements are sharded by `subject` EntityId. Supersession typically stays within one shard because subject doesn't change.

Edge case: `statement_supersede` is called with `old_id` whose subject differs from `new_statement.subject` — rejected per Mechanics step 2 ("`new_statement.subject` must equal `old.subject`"). Cross-shard chains are not possible by construction.

### The five-tier supersession ladder

For Preferences arriving via `STATEMENT_CREATE`, the auto-supersession step needs to decide whether the new statement **supersedes** the existing current row, **coexists** with it (different surface form, same intent — both legitimate), or **contradicts** it (signalling a conflict to the operator).

The decision runs a five-tier ladder. Each tier is consulted in order; the first tier that resolves the case wins.

| Tier | Trigger | Outcome |
|---|---|---|
| **0** | Exact match on `(subject, predicate_id)` | Force supersession (stateful kinds) or `Conflict` (idempotent kinds) |
| **1** | Statement-HNSW cosine ≥ 0.92 between the new statement and the current row's embedding | Auto-supersede |
| **2** | Statement-HNSW cosine in `[0.82, 0.92)` | LLM judge call returning `SUPERSEDES` / `COEXISTS` / `CONTRADICTS` |
| **3** | Statement-HNSW cosine < 0.82 | Coexist — keep both rows current |

Tier 0 is the deterministic fast path: identical `(subject, predicate_id)` with the same kind is the canonical "this is the new value" case the auto-supersession path was designed for.

Tier 1 catches near-synonym Preferences where the surface form drifted but the meaning didn't ("prefers async meetings" vs "prefers asynchronous meetings"). The cosine threshold reflects the statement-HNSW's typical noise floor.

Tier 2 is the **LLM-judge band**. Cosines this close are ambiguous on similarity alone; the judge sees both statements + the subject's recent context and returns a typed verdict. The judge prompt is documented in [`../11_extractors/01_extractor_tiers.md`](../11_extractors/01_extractor_tiers.md) §"Supersession judge". A `SUPERSEDES` verdict drives the normal supersession path; `COEXISTS` keeps both rows; `CONTRADICTS` writes a contradiction record per §"Contradiction handling" below and surfaces it on the audit channel.

Tier 3 is the "clearly different" fast path: low cosine means the two statements are about different enough things that no decision is warranted — both stay current, the audit record carries the cosine for future tuning.

Cost discipline: only Tier 2 invokes the LLM. The HNSW NN lookup the ladder relies on is the same statement HNSW populated by the `statement_embed` worker — Tiers 1-3 are cheap rank-ordering on a vector already in the index.

### Supersession audit trail

Every supersession writes an audit record to `entity_resolution_audit` (re-used as a generic audit table — kind discriminator `STATEMENT_SUPERSEDED`). Tracks who superseded whom, when, why. Retained indefinitely per §24.

## Contradiction handling

When two active Facts have the same `(subject, predicate)` but different `object`, they **contradict**. Brain **never auto-resolves**; it surfaces the conflict and lets the caller / human decide.

Cross-references:
- §"Kind-specific contracts" above — Fact-only rule.
- §"Why Facts don't auto-supersede" above — why Facts don't auto-supersede.

### The detection rule

A pair of statements `S1`, `S2` contradicts iff **all** of:

1. `S1.kind == StatementKind::Fact && S2.kind == StatementKind::Fact`.
2. `S1.subject == S2.subject`.
3. `S1.predicate_id == S2.predicate_id`.
4. `S1.object != S2.object` (tagged-union inequality — different variant or different inner value).
5. Both are **active**: not tombstoned, not superseded.
6. Their validity intervals overlap (`valid_from..=valid_to` ranges intersect with `now` or with each other for as-of queries).

Preference and Event are explicitly excluded:

- Preferences supersede each other — no contradiction.
- Events are point-in-time — two events at different times aren't a contradiction; two events at the same time about the same subject + predicate are recorded as-is (Brain trusts the source).

### The non-action

When `statement_create` is called and would produce a Fact that contradicts an existing active Fact, Brain **stores it anyway**. Both Facts coexist. Neither is "right"; Brain has no authority to decide.

Wire-side: `STATEMENT_CREATE` returns success; the response carries the new `StatementId` like any other create. The contradiction is **not** signalled in the success path — clients that care must explicitly query.

#### Why not error on contradiction?

Two reasons:

1. **Brain doesn't know what's true.** It receives claims from extractors / agents / humans. Refusing the conflicting claim would silently drop information.
2. **Contradiction is signal.** Two contradictory claims means the upstream source has an inconsistency. The right response is to surface it, not hide it.

The trade-off: query consumers must be aware that contradictions exist. The default query (`STATEMENT_LIST where_subject(x) of_kind(Fact)` returning current Facts) returns **all** non-superseded non-tombstoned active Facts, which may include contradictory pairs. Consumers post-process or rank.

### The surface op

`statements_contradicting(rtxn, subject, predicate) -> Vec<Statement>` (in `brain-metadata::statement_ops`):

```text
results = STATEMENTS_BY_SUBJECT_TABLE.range(
    (subject, StatementKind::Fact as u8, predicate_id, 1)..=
    (subject, StatementKind::Fact as u8, predicate_id, 1),
)
contradicting = []
for s in results:
    if s is active and overlaps_validity(s, now):
        contradicting.push(s)
if contradicting.iter().map(|s| &s.object).collect::<HashSet>().len() <= 1:
    return []   // single object => no contradiction
return contradicting
```

Returns:
- Empty if no active Facts with that `(subject, predicate)`.
- Empty if all active Facts agree (`object` equal across all).
- The set of disagreeing Facts otherwise — caller decides how to surface.

#### Wire surface

In v1.0 contradiction inspection is internal to Brain (used by query routing here) and by the admin op `ADMIN_LIST_PENDING_RESOLUTIONS` ([`../04_wire_protocol/09_typed_graph_admin.md`](../04_wire_protocol/09_typed_graph_admin.md) §4). There is **no** `STATEMENT_LIST_CONTRADICTIONS` wire opcode in v1.

Clients that want contradictions:

- Call `STATEMENT_LIST` with `subject + predicate + only_current=true`.
- Inspect the returned set; if more than one distinct `object`, the set is contradictory.

The query router exposes contradictions in `QUERY_TRACE` debug output. Production callers route there.

### Resolving contradictions

Three operator-facing options for resolving:

#### Tombstone the wrong one

`STATEMENT_TOMBSTONE` on the incorrect Fact. The other remains active; the chain stays intact for audit.

#### Supersede both

`STATEMENT_SUPERSEDE` on **each** Fact with a single new Fact that authoritatively settles the dispute. Each gets a different chain — they were independent original Facts. The new Fact must reference both via its `evidence` field (or just inherit evidence from one).

#### Retract one

`STATEMENT_RETRACT` (hard delete) on the wrong Fact. Removes the row after the grace period. Used when the wrong Fact was authored in error and shouldn't persist in audit.

Brain **doesn't pick** — operators / agents pick via these explicit opcodes.

### Detection during create

`statement_create` for a Fact runs the contradiction check inside the same redb txn that inserts. The check is **read-only** — it surfaces the conflict to the caller-emitted event but does **not** block the insert.

```text
At step "validate" in statement_create (after subject/predicate check):
    if kind == Fact:
        check = statements_contradicting(rtxn, subject, predicate_id)
        if !check.is_empty():
            // Add the new statement to the check set; if the new
            // object differs from all existing, this is a fresh
            // contradiction.
            contradicting_now = check + [new]
            distinct = contradicting_now.iter().map(|s| &s.object).collect::<HashSet>().len()
            if distinct > 1:
                contradiction_audit_record(rtxn, subject, predicate_id, contradicting_now)
                // No error — insert proceeds.
```

`contradiction_audit_record` writes a row to `entity_resolution_audit` (re-used) so operators can find unresolved contradictions via `ADMIN_LIST_PENDING_RESOLUTIONS`.

#### Event emission

The post-commit event is **still** `STATEMENT_CREATED` (not a special "contradicting" event). Consumers that subscribe to statement events get the create event and can run their own contradiction check if they care.

A future phase may add `STATEMENT_CONTRADICTED` as a discrete event; v1.0 keeps the surface minimal. Tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

### Time-bounded contradictions

Two Facts with non-overlapping `valid_from..=valid_to` intervals don't contradict (they describe different periods). E.g.:

- F1: `(Priya, role, "engineer")` valid 2025-01-01 → 2025-06-30.
- F2: `(Priya, role, "manager")` valid 2025-07-01 → ongoing.

These are sequential, not contradictory. The detection rule step 6 handles this — non-overlapping intervals fail the overlap test.

Edge case: implicit `valid_to = None` means "still valid". F1 with `valid_to=None` plus F2 with `valid_from=2025-07-01` do overlap (F1 extends through 2025-07-01+). Brain treats this as a contradiction unless the operator explicitly closed F1 (which is what `STATEMENT_SUPERSEDE` does — sets `old.valid_to = new.extracted_at`).

### Confidence and contradictions

Brain **ranks** contradicting Facts by confidence in the default query path. Two Facts with confidences 0.95 and 0.42 — the higher one sorts first.

Consumers can ignore the lower-confidence claim if confidence is below their tolerance threshold. Brain's job is to surface the disagreement; the consumer's job is to weight.

The confidence is **not** updated by the contradiction. Each Fact's confidence reflects its own evidence. The contradiction itself doesn't mean either is wrong — it means the evidence disagrees.

### Contradiction tests

- Contradictory pair created: both stored, both queryable, both `is_current=1`.
- `statements_contradicting()` returns both; ordering by confidence.
- Tombstone one: contradiction set returns just the remaining one (no contradiction now).
- Supersede one with a new Fact: chain intact; new Fact contradicts the other if object still differs.
- Preference with same `(subject, predicate)` but different `object`: auto-supersedes the prior, **no** contradiction recorded.
- Event with same `(subject, predicate)` repeated: both stored, **no** contradiction (Events don't contradict).

Test file: `crates/brain-server/tests/knowledge_statement_contradiction.rs` (lands 17.10).

## Confidence aggregation

How a statement's confidence is computed from its evidence + age. The mechanic backs the `confidence` field that every read path returns and that query routing ranks on.

Cross-references:
- §"Full statement row" above — `confidence: f32` field.
- §"Evidence" below — evidence model the aggregation runs over.

### The formula

```
confidence(S, now) = 1 - Π (1 - c_i · decay(age_i, kind))
                    i ∈ S.evidence
```

Where:

- `c_i` ∈ `[0, 1]` — the i-th evidence entry's own confidence (set by the source: extractor, agent, human author).
- `age_i = now - evidence_i.timestamp` — how old the evidence is.
- `decay(age, kind)` ∈ `[0, 1]` — the per-kind decay function.

Bounds:

- Empty evidence (`S.evidence.is_empty()`): confidence = `0.0` (no evidence, no support).
- Single evidence with `c_1 = 1.0`, no decay: confidence = `1.0`.
- Independent evidence aggregates **superlinearly** — two pieces of 0.9-confidence evidence yield `1 - (0.1 · 0.1) = 0.99`, not 0.9 + 0.9 capped.

The formula is the **noisy-OR** model: each evidence is an independent vote; the probability that **at least one** is correct is `1 minus the product of probabilities that all are wrong`.

### Why noisy-OR

Brain doesn't know whether evidence is correlated. Treating each piece independently is **conservative** — it over-attributes weight to repeated identical observations. Two extractions of the same fact from the same memory shouldn't yield 0.99 confidence; they're not independent.

Implementation cost: detect duplicates and treat as one. Brain does this at the evidence-add step:

```text
For each new evidence to add to S:
    if S.evidence already contains the same (memory_id, extractor_id, source_kind):
        skip   // duplicate; same vote counted once
```

After dedup, the noisy-OR holds. Cross-source overlap (different extractors confirming each other) is still treated as independent — slight optimism, accepted in v1.

### Decay functions

Per-statement-kind decay reflects the different epistemic profiles:

#### Fact — slow decay

```
decay_fact(age) = exp(-age / FACT_HALF_LIFE)
```

Default `FACT_HALF_LIFE = 365 days` (~1 year). After a year, a single piece of evidence contributes half its original weight. After 2 years, a quarter. After 5 years, ~5%.

Facts are stable claims; old facts are still mostly true.

#### Preference — faster decay

```
decay_pref(age) = exp(-age / PREF_HALF_LIFE)
```

Default `PREF_HALF_LIFE = 60 days` (~2 months). Preferences change; old preferences should fade quickly so a stale extraction doesn't keep ranking high.

#### Event — no decay

```
decay_event(age) = 1.0
```

Events are point-in-time. Their evidence doesn't get less reliable with age — the moment happened and the records exist. The confidence reflects how confident Brain is that the event happened, which doesn't change.

(But: the event's *relevance* to current state may fade. That's a query-time concern, not a confidence-storage concern.)

#### Override knobs

`ConfidenceConfig` (constructed per deployment, defaults match the above):

```rust
pub struct ConfidenceConfig {
    pub fact_half_life_seconds: u64,    // default 31_536_000 (365 days)
    pub pref_half_life_seconds: u64,    // default 5_184_000  (60 days)
    pub event_decay_disabled: bool,     // default true
}
```

The schema DSL allows per-predicate overrides (some predicates decay faster than their kind's default). Brain ships the kind-level defaults only.

### Recomputation triggers

`confidence` is recomputed when:

| Trigger | Hot path? |
|---|---|
| `statement_create` | yes — sets initial confidence based on evidence + 0 age |
| `statement_supersede` | yes — new statement gets fresh confidence; old's stays frozen at supersession time |
| Evidence added (post-creation) | yes — via `statement_add_evidence` op (deferred; tracked in [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md)) |
| Memory forget cascading evidence removal | yes — confidence recomputed without the removed entry |
| **Time-only** (age increases) | **no** — confidence is **not** lazily recomputed at read time |

The last point is significant. The stored `confidence` field is a **snapshot at last touch**. Read queries return that snapshot. Confidence "decay" therefore manifests via:

- The next time the statement is touched (supersede, evidence change), confidence is recomputed against `now`.
- The periodic **confidence_sweep** worker iterates aging statements and refreshes.
- Query-time recomputation is opt-in via a future `recompute_at_read` flag — not in v1.0.

This trade buys speed (reads don't run the formula) at the cost of slight staleness for statements that haven't been touched in a long time.

#### The confidence_sweep worker

`confidence_sweep` is the periodic refresh path. It runs as a low-priority sweeper (cadence configurable, default hourly), chunked-scans `STATEMENTS_TABLE`, recomputes `aggregate_confidence(evidence, now, kind, config)`, and writes back the new value if it differs from the stored value by more than the `0.05` index-churn threshold (see "Bucketing for indexes" below). Long-running deployments rely on it to drag stale Fact / Preference confidence down the decay curve so the ranker doesn't keep over-weighting aged evidence.

See [`../15_background_workers/06_typed_graph_workers.md`](../15_background_workers/06_typed_graph_workers.md) for the worker's batch cap, dry-run, and idempotency contract — it follows the standard sweeper discipline.

### The formula in code

`brain-core::knowledge::confidence`:

```rust
pub fn aggregate_confidence(
    evidence: &[EvidenceEntry],
    now_unix_nanos: u64,
    kind: StatementKind,
    config: &ConfidenceConfig,
) -> f32 {
    if evidence.is_empty() {
        return 0.0;
    }
    let mut product = 1.0f32;
    for e in evidence {
        let age_secs = ((now_unix_nanos.saturating_sub(e.timestamp_unix_nanos)) / 1_000_000_000) as f32;
        let decay = match kind {
            StatementKind::Event => 1.0,
            StatementKind::Fact => (-age_secs / config.fact_half_life_seconds as f32).exp(),
            StatementKind::Preference => (-age_secs / config.pref_half_life_seconds as f32).exp(),
        };
        let weighted = (e.confidence * decay).clamp(0.0, 1.0);
        product *= 1.0 - weighted;
    }
    1.0 - product
}

pub struct EvidenceEntry {
    pub memory_id: MemoryId,
    pub confidence: f32,                // [0, 1]
    pub timestamp_unix_nanos: u64,      // when the evidence was first observed
}
```

Pure function — no I/O, no state, no async. Called by `statement_ops::statement_create` (and supersede / evidence_change paths).

#### Edge cases

- Single evidence with `confidence = 0.0` → result `0.0` (decay doesn't matter).
- Single evidence with `confidence = 1.0`, age 0 → result `1.0`.
- Two evidence both 0.5, no decay → `1 - (0.5 · 0.5) = 0.75`.
- 100 evidence each 0.1, no decay → `1 - (0.9)^100 ≈ 0.9999734`. Yes, that's high; that's the noisy-OR.
- All evidence wiped by decay (`weighted ≈ 0`) → `product ≈ 1` → result `≈ 0`.
- Future timestamps (clock skew): `age_secs` saturates to 0 via `saturating_sub`; decay = 1.0.

### Bucketing for indexes

`STATEMENTS_BY_PREDICATE_TABLE` uses a `confidence_bucket: u8` derived from `floor(confidence * 10).clamp(0, 10)`:

| Confidence | Bucket |
|---|---|
| 0.00 - 0.10 | 0 |
| 0.10 - 0.20 | 1 |
| ... | ... |
| 0.90 - 1.00 | 9 |
| 1.00 (boundary) | 10 |

When confidence is recomputed and the bucket changes, the index entry must be removed-from-old-bucket and inserted-to-new. `statement_ops` handles this whenever confidence changes by more than 0.05 (avoids index churn on tiny adjustments).

### Confidence in queries

The default `STATEMENT_LIST` order is by confidence descending — high-confidence facts surface first. The query router uses confidence as one input to RRF fusion alongside semantic similarity, lexical relevance, and graph proximity.

`min_confidence` filter on `STATEMENT_LIST` and `QUERY` opcodes lets callers gate on a threshold. Default threshold per-deployment, configurable via `brain.query.min_confidence`.

### Confidence tests

The implementation test cases:

- Empty evidence → 0.0.
- Single evidence c=1.0 age=0 kind=Fact → 1.0.
- Two evidence c=0.9 each, no decay → exactly `1 - (0.1)² = 0.99`.
- Fact at 1-year age (half-life=1y), c=0.9 → `0.9 · 0.5 = 0.45`, single-evidence confidence `0.45`.
- Preference at 60-day age (half-life=60d), c=0.9 → `0.45` similar.
- Event at 5-year age, c=0.9 → `0.9` (no decay).
- 100 evidence each 0.1 no decay → ≥ 0.99.
- Future timestamp clock skew → saturates to 0 age, full confidence.
- Property: confidence is monotonic in number of evidence (more evidence never decreases confidence assuming all have c ≥ 0).
- Property: confidence stays in `[0, 1]` for any input.

Test file: `crates/brain-core/src/knowledge/confidence.rs::tests` (~10 unit tests).

## Evidence

How statements reference the memories / sources they derive from, with overflow handling for high-evidence statements and FORGET cascade for memory deletion.

Cross-references:
- §"Full statement row" above — `evidence: EvidenceRef`.
- §"Confidence aggregation" above — confidence aggregates over evidence.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §1.6, §1.8 — `STATEMENTS_BY_EVIDENCE_TABLE`, `EVIDENCE_OVERFLOW_TABLE`.
- [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md) §2.3 — wire shape.

### The model

A statement's `evidence` is a list of pointers to the memories (and metadata) that the statement was derived from. Two variants:

```rust
pub enum EvidenceRef {
    Inline(SmallVec<EvidenceEntry, 8>),    // up to 8 entries
    Overflow(EvidenceOverflowId),          // pointer to evidence_overflow row
}

pub struct EvidenceEntry {
    pub memory_id: MemoryId,
    pub confidence: f32,                   // [0, 1] — source-supplied
    pub timestamp_unix_nanos: u64,         // when observed
    pub extractor_id: u32,                 // 0 for user-authored
}
```

- **Inline** for the common case (most statements derive from 1-5 memories).
- **Overflow** when > 8 evidence entries — pointer indirection to a separate row.

### Why 8

Cap is small enough to fit in one cache line (8 × 24 bytes ≈ 192 bytes) and covers the long tail of common cases. Brain's pattern extractor typically produces 1-3 evidence per statement; LLM extractor typically 2-5. Operator-authored statements typically reference 1.

Statements with > 8 evidence are usually:

- Aggregated claims from many memories ("Priya prefers X" backed by 50 conversations).
- Long-lived Preferences that get reaffirmed over time.

For these, overflow is the right path.

### Inline → Overflow promotion

When `statement_create` is called with ≥ 9 evidence entries:

1. Allocate `EvidenceOverflowId` (UUIDv7).
2. Write `EvidenceOverflow { memory_ids: Vec<...>, extractor_ids: Vec<u32>, confidences: Vec<f32>, timestamps: Vec<u64> }` to `EVIDENCE_OVERFLOW_TABLE`.
3. Set `Statement.evidence = EvidenceRef::Overflow(overflow_id)`.

Subsequent reads dereference the pointer transparently — the client / handler decodes overflow rows into the same `EvidenceEntry` shape callers see for inline.

#### Add-evidence promotion

A future `STATEMENT_ADD_EVIDENCE` op (not in v1.0) appends new evidence to an existing statement. When the post-append count crosses 8, promote inline → overflow inside the same redb txn. Tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md).

For v1.0: evidence is set at create / supersede time only.

### Overflow row shape

```rust
pub struct EvidenceOverflow {
    pub memory_ids: Vec<[u8; 16]>,
    pub extractor_ids: Vec<u32>,
    pub confidences: Vec<f32>,
    pub timestamps: Vec<u64>,
}
```

4 parallel vectors of the same length (one entry across all = one `EvidenceEntry`). Stored as a single redb value; rkyv-archived; `check_bytes` validates on read.

Cap per overflow row: 1000 evidence entries (~32 KB). Above that, statements use **multiple chained overflow rows** — implementation detail in `brain-metadata::statement_ops` (only the first overflow id is stored on the statement; subsequent rows are chained via `next_chunk_id`).

V1.0 supports a single overflow row per statement (up to 1000 entries). Multi-chunk evidence is a phase-22 extension (when bulk extractor backfills create high-evidence claims).

### Reverse index — `STATEMENTS_BY_EVIDENCE`

Per evidence entry on each statement, one row is written to `STATEMENTS_BY_EVIDENCE_TABLE`:

```
key:   (memory_id_bytes: [u8; 16], statement_id_bytes: [u8; 16])
value: ()
```

Inline + overflow contribute equally — when a statement has 50 overflow entries, 50 rows go into the reverse index.

This is what makes FORGET cascade O(K) where K is the number of dependent statements: range-scan `(memory_id, *)`.

### FORGET cascade

When `FORGET memory_id` (opcode `0x0024`) is called:

1. **Hard mode** (the memory is gone, not just marked): for each statement referencing `memory_id`:
   - Look up via `STATEMENTS_BY_EVIDENCE_TABLE` prefix scan.
   - Per dependent statement S:
     - Remove the evidence entry referencing `memory_id` (inline path: rewrite Statement; overflow path: rewrite EvidenceOverflow).
     - Recompute `S.confidence`.
     - If `S.evidence.is_empty()` after removal:
       - Tombstone S with `reason = SourceMemoryForgotten`.
     - Else:
       - Update bucket in `STATEMENTS_BY_PREDICATE_TABLE` if confidence_bucket changed.
   - Remove the reverse-index row from `STATEMENTS_BY_EVIDENCE_TABLE`.

2. **Soft mode** (the memory is tombstoned but not yet reclaimed): same as hard for confidence-recomputation purposes, but the reverse-index row stays so Brain can replay if the memory is restored within grace.

The cascade runs **inside** the FORGET op's redb txn for atomicity. For memories with > 1000 dependent statements, the cascade batches into multiple txns (rare; tracked in [`.../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md)).

### Evidence integrity

Every evidence entry's `memory_id` must reference an existing (active or tombstoned) memory at creation time:

```text
For each new evidence at statement_create / supersede:
    if !MEMORY_EXISTS(memory_id):
        return INVALID_ARGUMENT
```

Evidence to forgotten memories is invalid; the cascade above cleans up.

Evidence to memories in OTHER shards is allowed. The cross-shard reverse-index entry lives on the memory's shard (so that shard's FORGET cascade finds the dependency). Brain implements the cross-shard write path via the existing routing mechanism.

### Evidence vs `extractor_id`

`extractor_id` lives on each `EvidenceEntry` and identifies **which extractor produced this evidence**. Values:

- `0` — user-authored (no extractor; the statement came from an explicit client write by an agent or human).
- `≥ 1` — registered extractor id (per [`../11_extractors/`](../11_extractors/00_purpose.md)).

Brain uses `extractor_id` for:

- **Audit** — "which extractor's output drove this claim?"
- **Per-extractor governance** — when an extractor is retracted (`EXTRACTOR_DISABLE`), all its evidence remains but downstream consumers see the `extractor_id` and can filter or down-weight.
- **Confidence calibration** — different extractors have different reliability profiles; future versions weight by extractor.

### Evidence tests

- Inline evidence round-trip: create with 3 evidence; read back 3.
- Overflow promotion: create with 9 evidence; read back 9 (overflow row written + read transparently).
- Mixed: create with 5 evidence, then (future op) add 4 more — must promote.
- Reverse index: after create with N evidence, `STATEMENTS_BY_EVIDENCE` has N rows under each memory_id.
- FORGET cascade: forget M1 referenced by 5 statements; each statement's confidence recomputes; if confidence drops to ε near zero, tombstone fires.
- FORGET cascade with empty-evidence outcome: forget the only-evidence memory; statement tombstones with `SourceMemoryForgotten`.
- Evidence integrity: create with non-existent `memory_id` → `INVALID_ARGUMENT`.
- Cross-shard evidence: statement on shard A, evidence memory on shard B; reverse index row written to shard B.

Test files:

- Unit: `crates/brain-metadata/src/statement_ops.rs::tests` for inline / overflow path.
- Cascade: `crates/brain-server/tests/knowledge_forget_cascade.rs` (Linux-only; needs in-process server for cross-shard).

### Evidence sizing

For a deployment with M statements, average evidence count N:

- Inline path (N ≤ 8): ~24 · N bytes per statement (no overflow row).
- Overflow path (N > 8): ~50 bytes (overflow id + bookkeeping) + 1 row × ~24·N bytes in `EVIDENCE_OVERFLOW_TABLE`.

`STATEMENTS_BY_EVIDENCE_TABLE`: ~32 bytes × N · M total. For 10M statements × 3 avg evidence: ~1 GB.
