# 17.8 — SDK Fact / Preference / Event builders

Hand-written fluent builders over the 7 statement wire opcodes (17.6
+ 17.7). Mirrors the entity SDK (16.8) — no derive macro yet; that
lands in phase 19 with `#[derive(BrainFact)]` etc.

Target ergonomics per spec §29/00 §"Typed statement API":

```rust
let fact = client.fact()
    .subject(priya.id)
    .predicate("role")
    .object_value("Engineering Manager")
    .evidence(vec![mem_x, mem_y])
    .confidence(0.9)
    .create().await?;

let prefs = client.statements()
    .where_subject(priya.id)
    .of_kind(StatementKind::Preference)
    .current_only()
    .with_min_confidence(0.7)
    .list().await?;
```

## Spec refs

- `spec/29_knowledge_sdk/00_purpose.md` §"Typed statement API" —
  target API surface.
- `spec/28_knowledge_wire_protocol/06_statement_frames.md` — wire
  shapes (already on the SDK via brain-protocol).
- `spec/19_statements/00_purpose.md` — kind / predicate / object
  invariants the builder enforces client-side before sending.

## Reads-only files (patterns to clone)

- `crates/brain-sdk-rust/src/knowledge/builder.rs` — `EntityClient`
  + `EntityCreateBuilder` precedent.
- `crates/brain-sdk-rust/src/knowledge/entity.rs` — `EntityHandle<T>`,
  `BrainEntityType`.
- `crates/brain-sdk-rust/src/knowledge/errors.rs` — typed error
  extension pattern.
- `crates/brain-protocol/src/knowledge/statement_{req,resp}.rs` —
  the wire shapes the builders construct.

## Key design decisions

### D1 — Kind-specific builders, shared internals

Three top-level entry points on `Client`:
- `client.fact() -> FactBuilder` (kind=Fact, rejects `event_at`).
- `client.preference() -> PreferenceBuilder` (kind=Preference, rejects
  `event_at`).
- `client.event() -> EventBuilder` (kind=Event, requires `event_at`).

Plus query / get / supersede / tombstone / retract surfaces:
- `client.statements() -> StatementsClient` — list / get / history /
  tombstone / retract.

Each kind-specific builder is its own struct so the type system
enforces the kind at compile time (no runtime "did you set event_at?"
guard for users that built a `FactBuilder`). Shared internals
(`subject` / `predicate` / `object` / `evidence` / `confidence` /
`request_id`) live in a private `StatementBuildShared` struct.

### D2 — `StatementHandle` — typed read-side projection

```rust
pub struct StatementHandle {
    pub id: StatementId,
    pub kind: StatementKind,
    pub subject: SubjectRef,
    pub predicate: String,        // canonical "ns:name"
    pub object: StatementObject,  // brain-core enum
    pub confidence: f32,
    pub evidence: EvidenceRef,
    pub version: u32,
    pub chain_root: StatementId,
    pub superseded_by: Option<StatementId>,
    pub supersedes: Option<StatementId>,
    pub tombstoned: bool,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub event_at_unix_nanos: Option<u64>,
    pub extracted_at_unix_nanos: u64,
}

impl StatementHandle {
    pub fn from_view(view: StatementView) -> Self;
    pub fn is_current(&self) -> bool;
}
```

Unlike `EntityHandle<T>` there's no per-kind type parameter — the
read side returns a uniform `StatementHandle` and callers branch on
`handle.kind` if they care. Phase 19's `#[derive(BrainFact)]` macro
will generate typed `Fact<RoleAttrs>` wrappers; v1 is the simple
uniform shape.

### D3 — `object_value` ergonomics: typed setters

Each builder exposes:
- `.object_entity(EntityId)` — sets `StatementObjectWire::EntityRef`.
- `.object_value(impl Into<StatementValueWire>)` — Text / Integer /
  Float / Bool / UnixNanos / Blob via `From` impls on `StatementValueWire`.
- `.object_memory(MemoryId)`.
- `.object_statement(StatementId)`.

Builder validates exactly one of these is set at `.create()` time;
otherwise `MissingObject` error.

### D4 — `evidence(Vec<MemoryId>)` + cap check client-side

```rust
.evidence(vec![mem_a, mem_b, mem_c])
```

The wire protocol caps at `INLINE_EVIDENCE_CAP = 8`. Builder rejects
> 8 at `.create()` time with `EvidenceTooManyInline`. Overflow path
(allocate an overflow id first) is a phase-22 add-evidence surface;
not exposed in v1 SDK.

### D5 — `client.preference().supersedes(prior_id).create()` shortcut

§29/00 shows explicit-supersede via the builder:

```rust
let new_pref = client.preference()
    .subject(priya.id)
    .predicate("prefers")
    .object_value("written agendas")
    .supersedes(pref.id)
    .create().await?;
```

If `supersedes` is set, the builder sends `STATEMENT_SUPERSEDE`
instead of `STATEMENT_CREATE`. The response shape differs
(`StatementSupersedeResponse` vs `StatementCreateResponse`); we
round-trip a GET in either case to return a uniform `StatementHandle`.

Same shortcut available on `FactBuilder` (explicit Fact supersede).

### D6 — Default predicate namespace

For convenience, `.predicate("role")` without a colon is interpreted
as the deployment's default namespace. Spec doesn't fully nail this
down — v1 of the SDK requires the explicit `"ns:name"` form and
errors out on a bare name. Documented in the builder docs.

This is conservative: callers who forget the namespace get a clear
error rather than a silent default that becomes wrong when their
schema lands.

### D7 — `StatementsClient` query builder

```rust
pub struct StatementsClient<'a> {
    client: &'a Client,
}

impl StatementsClient<'_> {
    pub fn list(&self) -> StatementListBuilder<'_>;
    pub async fn get(&self, id: StatementId) -> Result<Option<StatementHandle>, ClientError>;
    pub async fn get_current(&self, id: StatementId) -> Result<Option<StatementHandle>, ClientError>;
    pub async fn history(&self, anchor: StatementId) -> Result<Vec<StatementHandle>, ClientError>;
    pub async fn tombstone(&self, id: StatementId, reason: TombstoneReason, message: String) -> Result<u64, ClientError>;
    pub async fn retract(&self, id: StatementId, reason: TombstoneReason, message: String) -> Result<u64, ClientError>;
}
```

`StatementListBuilder` chains: `.where_subject(id)`, `.where_predicate(s)`,
`.of_kind(StatementKind)`, `.current_only()`, `.include_tombstoned()`,
`.with_min_confidence(f)`, `.time_range(start, end)`, `.limit(n)`,
then `.list().await`.

## Plan

### Step 1 — Module skeleton

New file `crates/brain-sdk-rust/src/knowledge/statement.rs`. Imports
from brain-core + brain-protocol; mirrors `entity.rs` shape.

Types:
- `StatementHandle` value type + `from_view` constructor.
- `StatementsClient<'a>` entry-point.
- `FactBuilder<'a>`, `PreferenceBuilder<'a>`, `EventBuilder<'a>`.
- `StatementListBuilder<'a>`.
- `StatementValueWire` re-export for ergonomic literal construction.

### Step 2 — Builder internals

```rust
struct StatementBuildShared {
    subject: Option<WireUuid>,
    predicate: Option<String>,
    object: Option<StatementObjectWire>,
    evidence: Vec<[u8; 16]>,
    confidence: f32,                    // default 0.5
    extractor_id: u32,                  // default 0 (user-authored)
    schema_version: u32,                // default 0
    valid_from_unix_nanos: u64,
    valid_to_unix_nanos: u64,
    supersedes: Option<StatementId>,
    request_id: Option<[u8; 16]>,
}
```

Common setters on each builder forward to a private `&mut shared`
borrow. `event_at_unix_nanos: u64` lives only on `EventBuilder`.

### Step 3 — `Client::fact()` / `.preference()` / `.event()` / `.statements()`

Add 4 methods to `impl Client` (in `client/mod.rs` or in the builder
module via an extension trait — match `EntityClient` precedent which
adds `pub fn entity<T>()` to `Client`). Each returns the appropriate
builder.

### Step 4 — `.create()` / `.send()`

`create()` (per spec §29/00 — note the spec uses `.create()` while
the entity SDK uses `.send()`; we go with the spec text for the
statement SDK):

1. Assemble `StatementCreateRequest` from `StatementBuildShared` +
   kind-specific overrides.
2. Validate (subject set, predicate non-empty + contains `:`, object
   set, evidence ≤ 8, confidence in [0,1], Event requires event_at).
3. If `supersedes.is_some()`: send `STATEMENT_SUPERSEDE` (embedded
   create payload).
4. Else: send `STATEMENT_CREATE`.
5. Round-trip a `STATEMENT_GET(follow_supersession=false)` to fetch
   the full StatementView.
6. Project to `StatementHandle`.

### Step 5 — `StatementListBuilder`

Same single-page snapshot pattern as `EntityListBuilder` (limit
1..=1000; cursor pagination defers to phase 23). Errors on builder-
side limit violations.

### Step 6 — Errors

Extend `ClientError` mapping in `crates/brain-sdk-rust/src/knowledge/errors.rs`:

```rust
pub enum StatementErrorKind {
    NotFound,
    PredicateUnknown,
    SubjectUnknown,
    ContradictsExisting,
    ObjectTypeMismatch,
    Internal,
    Other,
}

pub trait ClientErrorStatementExt {
    fn statement_error_kind(&self) -> StatementErrorKind;
}
```

### Step 7 — Re-exports

`crates/brain-sdk-rust/src/knowledge/mod.rs`:
- `pub mod statement;`
- `pub use statement::{StatementHandle, StatementsClient, FactBuilder,
   PreferenceBuilder, EventBuilder, StatementListBuilder};`

`crates/brain-sdk-rust/src/lib.rs` top-level re-exports for the most
common types.

### Step 8 — Tests

Colocated unit tests in `statement.rs` for builder logic that doesn't
need a server:

- `fact_builder_requires_subject`.
- `fact_builder_rejects_event_at`.
- `event_builder_requires_event_at`.
- `preference_builder_supersede_routes_to_supersede_op`.
- `evidence_cap_rejected_at_create`.
- `predicate_must_contain_colon`.
- `object_value_from_string` — `From<&str>` for StatementValueWire.
- `list_builder_limit_validation`.
- `statement_handle_from_view_round_trip`.
- `statement_handle_is_current_logic`.

End-to-end mock-server tests + post-commit event assertions land
in 17.10.

## Files written

| Path | Change |
|---|---|
| `crates/brain-sdk-rust/src/knowledge/statement.rs` | New. ~700 lines (3 kind builders + list builder + StatementHandle + StatementsClient + ~10 unit tests). |
| `crates/brain-sdk-rust/src/knowledge/mod.rs` | Add module + re-exports. |
| `crates/brain-sdk-rust/src/knowledge/errors.rs` | Add `StatementErrorKind` + `ClientErrorStatementExt`. |
| `crates/brain-sdk-rust/src/knowledge/builder.rs` (or new spot) | Add `Client::fact() / .preference() / .event() / .statements()` entry methods. |
| `crates/brain-sdk-rust/src/lib.rs` | Top-level re-exports for the most common types. |

## Verification gate

```
cargo test -p brain-sdk-rust knowledge::statement
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy -p brain-sdk-rust --all-targets -- -D warnings
```

## Commit message draft

```
feat(brain-sdk-rust): statement builders + StatementHandle (17.8)

Fluent builders over the 7 statement wire opcodes (17.6/17.7):
- client.fact() / .preference() / .event() — kind-specific builders.
- client.statements() — StatementsClient with list / get /
  get_current / history / tombstone / retract.

Each kind builder exposes .subject / .predicate / .object_entity /
.object_value / .object_memory / .object_statement / .evidence /
.confidence / .valid_from / .valid_to / .supersedes / .request_id.
EventBuilder additionally has .event_at(t).

If .supersedes(prior_id) is set, .create() routes to
STATEMENT_SUPERSEDE instead of STATEMENT_CREATE; either path
round-trips a GET to return a uniform StatementHandle.

StatementListBuilder chains the same filters as the wire op
(where_subject / where_predicate / of_kind / current_only /
include_tombstoned / with_min_confidence / time_range / limit).
Cursor pagination + true streaming defers to phase 23.

ClientErrorStatementExt classifies wire errors (NotFound /
PredicateUnknown / SubjectUnknown / ContradictsExisting /
ObjectTypeMismatch).

~10 builder-logic unit tests; end-to-end tests in 17.10.

Plan: .claude/plans/phase-17-task-08.md.
```

## Risks

- **Spec uses `.create()` while entity SDK uses `.send()`.** We
  follow the spec for statement builders. The asymmetry is small
  but documented in module docs. Could harmonise in a phase-19
  cleanup once both are in user hands.
- **StatementHandle uniform vs typed.** §29/00 shows post-derive-
  macro typed `Fact<RoleAttrs>`. v1 SDK returns the uniform handle;
  phase 19 macro adds the wrappers. No code-breaking gap — derive
  macro generates `impl From<StatementHandle> for Fact<…>`.
- **Predicate qname validation client-side** duplicates server-side
  validation. Client-side fails fast (no network round-trip); server
  is the source of truth. Both must agree on the grammar; identical
  rules (`[a-z][a-z0-9_]*` per ns + name).
- **Evidence cap is wire-layer**. Surfacing in the builder gives a
  clean error; the wire shape already rejects > 8 inline.

## Out of scope (this sub-task)

- `#[derive(BrainFact)]` macro — phase 19.
- Typed `Fact<T>` / `Preference<T>` / `Event<T>` wrappers — phase 19.
- Add-evidence opcode + overflow allocation — phase 22.
- Subscribe-event handle for `StatementCreated/Superseded/Tombstoned`
  — substrate subscribe surface already covers; SDK convenience
  wrappers come with phase 23.
- End-to-end mock-server integration tests — 17.10.
