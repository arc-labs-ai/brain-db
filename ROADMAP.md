# Roadmap

High-level implementation plan. Each phase is a step toward Brain v1.0. Detailed sub-task breakdowns live in [`docs/phases/`](docs/phases/) — this file is the index.

For autonomous-mode operating rules, see [`AUTONOMY.md`](AUTONOMY.md).

## v1.0 ships in two layers

- **Substrate (phases 0–14)** — vector memory store: WAL, HNSW, wire protocol, cognitive primitives, HTTP transport, observability, benchmarks, substrate acceptance. Tags out as `v0.9.x-substrate-rc` at Phase 14.
- **Knowledge layer (phases 15–24)** — typed entities, statements, relations, schema DSL, three-tier extractors (pattern → classifier → LLM), hybrid retrieval (semantic + lexical + graph with RRF fusion). Activates when a schema is declared; dormant otherwise.

The `v1.0.0` tag lands at the end of Phase 24, after the *combined* acceptance suite passes. A deployment that never calls `SCHEMA_UPLOAD` is a valid v1.0 deployment posture (substrate-only mode).

---

## Phase 0 — Workspace skeleton ✓ provided by starter

**Status:** Scaffolded by the starter template. Verify before moving on.

**Provided:**

- `Cargo.toml` workspace with shared dependency table.
- 12 stub crates under `crates/`.
- `rustfmt.toml`, `clippy.toml`, `rust-toolchain.toml`.
- `.github/workflows/ci.yml` running build, test, clippy, fmt, miri, audit.
- `.gitignore`, `justfile`, `config/dev.toml`.
- `fuzz/` directory.
- `.claude/` with settings, hooks, slash commands, subagents.

**Verify (before starting Phase 1):**

- [ ] `just verify` is green.
- [ ] CI is green on first push.
- [ ] Tag the latest commit: `git tag phase-0-complete`.

**No detailed phase doc** — the work is just verification.

---

## Phase 1 — Wire Protocol & Core Types

**One-line:** Frame format, opcode codecs, fuzz target.

**Detailed plan:** [`docs/phases/phase-01-wire-protocol.md`](docs/phases/phase-01-wire-protocol.md)

**Crates touched:** `brain-core`, `brain-protocol`.

**Sub-tasks:** 11.

**Exit:** every opcode round-trips; fuzz finds no panics; tag `phase-1-complete`.

---

## Phase 2 — Storage: Arena + WAL + Recovery

**One-line:** Memory-mapped vector arena, write-ahead log with group commit, crash recovery.

**Detailed plan:** [`docs/phases/phase-02-storage.md`](docs/phases/phase-02-storage.md)

**Crates touched:** `brain-storage`.

**Sub-tasks:** 12.

**Exit:** 1000-iteration random-kill recovery test passes; miri clean; tag `phase-2-complete`.

---

## Phase 3 — Metadata + Graph (redb)

**One-line:** All 13 redb tables; idempotency; recovery integration with Phase 2.

**Detailed plan:** [`docs/phases/phase-03-metadata.md`](docs/phases/phase-03-metadata.md)

**Crates touched:** `brain-metadata`.

**Sub-tasks:** 12.

**Exit:** all tables present and tested; cross-crate recovery test passes; tag `phase-3-complete`.

---

## Phase 4 — ANN Index (HNSW)

**One-line:** Wrap `hnsw_rs` with the spec's parameters and lifecycle.

**Detailed plan:** [`docs/phases/phase-04-ann-index.md`](docs/phases/phase-04-ann-index.md)

**Crates touched:** `brain-index`.

**Sub-tasks:** 8.

**Exit:** recall@10 ≥ 0.95 at 100K vectors; persistence round-trip works; tag `phase-4-complete`.

---

## Phase 5 — Embedding Layer

**One-line:** BGE-small via candle, batching, caching, determinism.

**Detailed plan:** [`docs/phases/phase-05-embedding.md`](docs/phases/phase-05-embedding.md)

**Crates touched:** `brain-embed`.

**Sub-tasks:** 7.

**Exit:** ≥ 1K texts/sec; deterministic; tag `phase-5-complete`.

---

## Phase 6 — Query Planner & Executor

**One-line:** Logical plan tree, cost model, pull-based executor.

**Detailed plan:** [`docs/phases/phase-06-planner.md`](docs/phases/phase-06-planner.md)

**Crates touched:** `brain-planner`.

**Sub-tasks:** 8.

**Exit:** every operation type has a planner test; tag `phase-6-complete`.

---

## Phase 7 — Cognitive Operations

**One-line:** ENCODE, RECALL, PLAN, REASON, FORGET on top of the planner; idempotency.

**Detailed plan:** [`docs/phases/phase-07-operations.md`](docs/phases/phase-07-operations.md)

**Crates touched:** `brain-ops`.

**Sub-tasks:** 11.

**Exit:** correctness suite from spec §16/01 fully green; tag `phase-7-complete`.

---

## Phase 8 — Background Workers

**One-line:** All 12 workers running cooperatively.

**Detailed plan:** [`docs/phases/phase-08-workers.md`](docs/phases/phase-08-workers.md)

**Crates touched:** `brain-workers`.

**Sub-tasks:** 14.

**Exit:** each worker tested; performance regression test green; tag `phase-8-complete`.

---

## Phase 9 — `brain-server`: end-to-end wire-up

**One-line:** A runnable substrate. Tokio connection layer + Glommio shards.

**Detailed plan:** [`docs/phases/phase-09-server.md`](docs/phases/phase-09-server.md)

**Crates touched:** `brain-server`.

**Sub-tasks:** 10.

**Exit:** E2E smoke test passes 100 iterations; tag `phase-9-complete`.

---

## Phase 10 — Rust SDK & CLI

**One-line:** Polished `Client` + `brain-cli` covering every spec'd admin command.

**Detailed plan:** [`docs/phases/phase-10-sdk-cli.md`](docs/phases/phase-10-sdk-cli.md)

**Crates touched:** `brain-sdk-rust`, `brain-cli`.

**Sub-tasks:** 13.

**Exit:** SDK drives every operation; CLI covers every command; tag `phase-10-complete`.

---

## Phase 11 — `brain-http` (foundation HTTP/WS/SSE layer)

**One-line:** Brain-owned HTTP transport on hyper 1.x — replaces hand-rolled admin/CLI HTTP, adds WebSocket + SSE.

**Detailed plan:** [`docs/phases/phase-11-brain-http.md`](docs/phases/phase-11-brain-http.md)

**Crates touched:** new `brain-http`; migrations in `brain-server`, `brain-cli`.

**Sub-tasks:** 8.

**Exit:** admin hand-roll deleted; SSE + WebSocket working end-to-end; tag `phase-11-complete`.

---

## Phase 12 — Observability

**One-line:** Production-grade telemetry surface — full metrics taxonomy, structured JSON logs, OpenTelemetry tracing, dashboards, alerts.

**Detailed plan:** [`docs/phases/phase-12-observability.md`](docs/phases/phase-12-observability.md)

**Crates touched:** all (instrumentation), plus `docs/analytics/dashboards/`, `docs/analytics/alerts/`.

**Sub-tasks:** 6.

**Exit:** every spec'd `brain_*` metric emitted; JSON log schema matches spec §14/02; OTel spans cover request lifecycle; reference Grafana dashboards + Alertmanager rules ship in-tree; tag `phase-12-complete`.

---

## Phase 13 — Benchmarks & Chaos

**One-line:** Measure-and-stress: criterion benches for every operation, load generator, chaos harness, soak rig.

**Detailed plan:** [`docs/phases/phase-13-benchmarks.md`](docs/phases/phase-13-benchmarks.md)

**Crates touched:** `benches/`, `tests/chaos/`, `tests/soak/`.

**Sub-tasks:** 4.

**Exit:** every operation has a criterion baseline that hits the spec §16 targets on reference hardware; chaos suite covers kill / I/O fault / network / corruption scenarios; tag `phase-13-complete`.

---

## Phase 14 — Substrate Acceptance & `v0.9.x-substrate-rc`

**One-line:** Run all 10 substrate acceptance gates, runbook-validate, doc pass, tag substrate release-candidate.

**Detailed plan:** [`docs/phases/phase-14-acceptance-release.md`](docs/phases/phase-14-acceptance-release.md)

**Crates touched:** `acceptance/`, `docs/runbooks/`, READMEs, CHANGELOG.

**Sub-tasks:** 5.

**Exit:** gates 1-10 green; 48 h soak result recorded; runbooks executed against a chaos scenario; `cargo doc` clean; tag `phase-14-complete` and `v0.9.x-substrate-rc`. **The `v1.0.0` tag is deferred to Phase 24** (combined substrate + knowledge-layer release).

---

# Knowledge layer (phases 15–24)

These phases turn Brain from a vector memory store into a cognitive database with typed entities, statements, relations, schema-driven extraction, and hybrid retrieval. Estimated 58–83 days of focused work. Phases 16–22 can partially overlap once Phase 15 is done. See [`docs/phases/README.md`](docs/phases/README.md) for the full dependency DAG.

---

## Phase 15 — Knowledge storage extensions

**One-line:** New redb tables, WAL frame types, on-disk artifact paths (tantivy/HNSW/LLM cache), schema-declared flag. Binary boots; substrate behaves identically.

**Detailed plan:** [`docs/phases/phase-15-knowledge-storage.md`](docs/phases/phase-15-knowledge-storage.md)

**Crates touched:** `brain-metadata`, `brain-storage`, `brain-server`.

**Sub-tasks:** 6. **Exit:** substrate-only regression suite stays green; tag `phase-15-complete`.

---

## Phase 16 — Entity layer ✓

**One-line:** Entity table, type system, entity HNSW (declared; resolver wiring in phase 21), resolver tiers 1 (exact / alias) and 2 (trigram fuzzy). Tiers 3 (embedding) and 4 (LLM) stubbed for phase 21.

**Detailed plan:** [`docs/phases/phase-16-entities.md`](docs/phases/phase-16-entities.md)

**Crates touched:** `brain-core`, `brain-metadata`, `brain-index`, `brain-protocol`, `brain-server`, `brain-sdk-rust`.

**Sub-tasks:** 9. **Exit:** entity create / merge / unmerge / rename / resolve / list / tombstone all work via wire + SDK; tag `phase-16-complete`.

**Delivered:**

- 9 entity wire opcodes (`0x0130–0x0138`) end-to-end through `brain-protocol`, `brain-ops`, `brain-server`.
- Knowledge namespace introduced at high-byte `0x01` (wire opcode widened to `u16` in 16.6a — pre-v1.0 wire change documented in §03/12 §0).
- Hand-written entity SDK over `Person` (typed `EntityHandle<T>` + 5 builders for all 9 ops + `ClientErrorEntityExt`). Derive macros defer to phase 19.
- `MergeRecord` v2 + `entity_merge_ops` (full diff captured for grace-period unmerge). Statement / relation re-route deferred to phases 17 / 18 sweeps.
- §28 knowledge wire protocol section brought to §03-depth (15 detail files, ~135 KB of spec).
- §18 entities backfilled with merge / unmerge / GC mechanics (§03 / §04 / §05).
- Adversarial-input resolver tests + create→merge→unmerge→rename lifecycle integration test + criterion bench for tier-1 / tier-2 perf.
- 14 substrate `SubscriptionEvent` event types extended for knowledge layer; event emission wired across all six mutating entity handlers.

**Deferred to later phases (tracked in `spec/28/09_open_questions.md` + `spec/18/06_open_questions.md`):**

- Resolver tier 3 (embedding) — phase 21 when entity HNSW is wired into the resolver.
- Tier 4 (LLM-tier) — phase 21.
- Cursor pagination + multi-frame streaming for `ENTITY_LIST` — phase 23.
- Statement / relation re-routing during merge — phases 17 / 18.
- Derive macro `#[derive(BrainEntity)]` — phase 19.

---

## Phase 17 — Statement layer ✓

**One-line:** Statement table; three kinds (Fact, Preference, Event); supersession chains; contradiction surfacing; statement HNSW (declared; populator in phase 21); per-kind noisy-OR confidence aggregation.

**Detailed plan:** [`docs/phases/phase-17-statements.md`](docs/phases/phase-17-statements.md)

**Crates touched:** `brain-core`, `brain-metadata`, `brain-index`, `brain-protocol`, `brain-ops`, `brain-server`, `brain-sdk-rust`.

**Sub-tasks:** 11. **Exit:** all three kinds work end-to-end via wire + SDK; supersession chains traverse; contradictions surface (not auto-resolved); confidence aggregation drops in; tag `phase-17-complete`.

**Delivered:**

- §19 statements section brought to §03-depth (8 files; supersession / contradiction / storage / confidence / evidence / open questions / references).
- §28/06 statement frames already at §03-depth from phase 16; new opcodes wired end-to-end.
- 7 statement wire opcodes (`0x0140–0x0146`) + responses (`0x01C0–0x01C6`) end-to-end through `brain-protocol`, `brain-ops`, `brain-server`.
- `brain-core` value types: `Statement` / `StatementObject` (tagged union: Entity / Value / Memory / Statement) / `StatementValue` (Text / Integer / Float / Bool / UnixNanos / Blob) / `EvidenceRef` (Inline / Overflow) / `SubjectRef` / `TombstoneReason` / `Predicate`.
- Predicate registry + interning in `brain-metadata`, with built-ins `brain:is_a / has_name / mentions / related_to / prefers / scheduled` seeded at `MetadataDb::open`. `predicate_intern` is idempotent on identical constraints; `AlreadyExists` on diverging shapes.
- `statement_ops`: CRUD + supersession (auto for Preference, explicit for Fact) + contradiction surface (Facts) + tombstone / retract + chain history + filtered list. All operations atomic within one redb txn.
- Statement HNSW declared in `brain-index` (M=32, ef_construction=200, ef_search=128 per spec §26/00); populator deferred to phase 21 with the embedding worker.
- Confidence aggregation per spec §19/04: noisy-OR with per-kind decay (Fact 365d half-life / Preference 60d / Event none). Wired into `statement_create` / `_supersede` when inline evidence carries per-entry metadata; wire callers keep their supplied confidence until phase 22's ADD_EVIDENCE op.
- Hand-written statement SDK: `client.fact() / .preference() / .event() / .statements()`. Uniform `StatementHandle` read-side; derive macros defer to phase 19.
- Integration test suite: 11 wire-smoke tests + 13-step lifecycle + 9 mock-server SDK tests + statement_ops criterion bench.

**Deferred to later phases (tracked in `spec/19_statements/06_open_questions.md` + `spec/28/09_open_questions.md`):**

- Statement HNSW embedding worker — phase 21.
- `STATEMENT_ADD_EVIDENCE` opcode (richer per-entry metadata wire path) — phase 22.
- Confidence-sweep worker + bucket re-indexing on confidence delta > 0.05 — phase 21.
- Hybrid query router (RRF fusion) for statement semantic search — phase 23.
- Cursor pagination + multi-frame streaming for `STATEMENT_LIST` / `STATEMENT_HISTORY` — phase 23.
- Discrete `STATEMENT_CONTRADICTED` event + dedicated contradiction audit table — phase 22-23.
- Multi-value `by_subject` index for contradictory active Facts — phase 23.
- Per-predicate decay overrides (vs kind-level defaults) — phase 19 schema DSL.
- Cross-shard `statements_by_object_entity` reverse-index writes — phase 23.
- `#[derive(BrainFact)]` macro + typed `Fact<T>` SDK wrappers — phase 19.

---

## Phase 18 — Relation layer ✓

**One-line:** Relation table; relation-type registry with cardinality + symmetric flags; cardinality-driven auto-supersession; canonical symmetric ordering with dual-index population; iterative BFS traversal with cycle detection and depth/branching caps.

**Detailed plan:** [`docs/phases/phase-18-relations.md`](docs/phases/phase-18-relations.md)

**Crates touched:** `brain-core`, `brain-metadata`, `brain-protocol`, `brain-ops`, `brain-server`, `brain-sdk-rust`.

**Sub-tasks:** 10. **Exit:** all 4 cardinality variants work end-to-end via wire + SDK; symmetric relations dual-indexed; traverse terminates on cycles within depth/branching caps; tag `phase-18-complete`.

**Delivered:**

- §20 relations section brought to §03-depth (8 files; cardinality / symmetric / storage / traversal / evidence / open questions / references).
- §28/07 relation frames already at §03-depth from phase 16; new opcodes wired end-to-end.
- 7 relation wire opcodes (`0x0150–0x0156`) + responses (`0x01D0–0x01D6`) end-to-end through `brain-protocol`, `brain-ops`, `brain-server`.
- `brain-core` value types: `Relation` (18 fields with `chain_root` + supersession), `RelationType` (with `cardinality`, `is_symmetric`, optional `from_type / to_type` constraints), `canonical_pair` helper for symmetric byte-wise ordering.
- Relation-type registry + interning in `brain-metadata` with built-ins `brain:related_to` (ManyToMany asymmetric), `brain:reports_to` (ManyToOne), `brain:co_authored` (symmetric ManyToMany) seeded at `MetadataDb::open`.
- `relation_ops`: CRUD + cardinality-driven auto-supersession (ManyToMany / ManyToOne / OneToMany / OneToOne) + symmetric canonicalisation + tombstone + chain history + filtered list (by_from / by_to with type filter). All operations atomic within one redb txn.
- `relation_traversal`: iterative BFS with `DEFAULT_MAX_DEPTH = 3` (cap 5), `DEFAULT_MAX_BRANCHING = 1000` (cap 10_000), visited-set cycle detection, tracing::warn on truncation, `TraversalDirection` (Outgoing / Incoming / Both).
- `RelationMetadata` rkyv shape widened with `chain_root_bytes`; archive id bumped to v2.
- Hand-written relation SDK: `client.relation()` / `.relations()` with uniform `RelationHandle` + `TraversalPath` value types. Derive macros defer to phase 19.
- Integration test suite: 11 wire-smoke tests + 6-step lifecycle test + 8 mock-server SDK tests + criterion bench against §16/02 §2.4 perf targets.
- New event type: `RelationTombstoned` added to `EventType` enum + `KnowledgeEventPayload`. Created / Superseded events also wired through brain-ops handlers.

**Deferred to later phases (tracked in `spec/20_relations/06_open_questions.md` + `spec/28/09_open_questions.md`):**

- Cross-shard `RELATION_TRAVERSE` coordination — phase 23.
- Streaming TRAVERSE response (per-frame) + cursor pagination on LIST_FROM / LIST_TO — phase 23.
- Weight-aware shortest-path traversal — post-v1.0.
- `RELATION_RETRACT` opcode (hard delete with grace period) — phase 22.
- Entity-merge relation re-routing — phase 23 (or after phase 18 if scope allows).
- `RelationCardinalityConflict` discrete event — phase 23.
- Bulk-mode cardinality skip flag for extractor backfills — phase 22.
- `relations_by_type` index for type-only admin queries — phase 22 if demanded.
- Per-relation-type FORGET cascade configurability — phase 22.
- `#[derive(BrainRelation)]` macro + typed `Relation<T>` SDK wrappers — phase 19.
- Cardinality auto-supersede event emission (handler doesn't yet surface the inner supersede that `relation_create` performs) — phase 22.

---

## Phase 19 — Schema DSL ✓

**One-line:** Parser + validator + per-namespace versioning for the declarative schema language; system-schema bootstrap replaces hand-seeded built-ins; SDK schema builders.

**Detailed plan:** [`docs/phases/phase-19-schema-dsl.md`](docs/phases/phase-19-schema-dsl.md) (superseded by `.claude/plans/phase-19*.md` per-sub-task plans).

**Crates touched:** `brain-protocol`, `brain-metadata`, `brain-ops`, `brain-sdk-rust`, `brain-server`.

**Sub-tasks:** 9. **Exit:** schema upload validates + versions per namespace; system schema seeds on first open; subsequent entity / predicate / relation registrations flow through the parse → validate → persist → intern fan-out; tag `phase-19-complete`.

**Scope cut:** **No migration plan computation** per user direction (no existing deployments to migrate). Tracked as `spec/21_schema_dsl/07_open_questions.md` Q3.

**Delivered:**

- §21 schema-DSL section brought to §03-depth (9 files: ast / validator / namespaces / versioning / system_schema / open_questions / references).
- §16/02 §2.6 schema-layer perf targets added (UPLOAD / VALIDATE / GET / LIST); phase-gate renumbered.
- §29/00 SDK phase-scope flipped: 19.8 SchemaBuilder + client.schema() ✓.
- 4 schema wire opcodes (`SCHEMA_UPLOAD` / `_GET` / `_LIST` / `_VALIDATE` at `0x0120-0x0123`, responses at `0x01A0-0x01A3`) end-to-end through `brain-protocol`, `brain-ops`, `brain-server`.
- Schema AST in `brain-protocol::schema`: value-typed (serde, no rkyv) — `Schema`, `SchemaItem`, `EntityTypeDef`, `PredicateDef`, `RelationTypeDef`, `ExtractorDef` + supporting enums.
- Pest 2.7 parser (`grammar.pest` + recursive-descent visitor) mirrors the §21/01 EBNF: namespaces, attribute modifiers, predicate kind/object, relation cardinality + symmetric + properties, extractor kind/target + 14 per-kind config fields, heredoc strings, regex literals, JSON capture for `examples:` / `schema:`, condition expressions with and/or/matches, comments + CRLF + trailing commas. `ParseError` carries 1-based line/col.
- Static validator (`validate(&Schema)` + `validate_system_schema(&Schema)`): namespace + duplicates + type refs + predicate kind/object compatibility + cardinality/symmetric + attribute rules (`unique` not on Ref, default-type compat) + extractor required-field/range checks. `ValidatedSchema` newtype proves validation cleared. Accumulates all errors (no first-error short-circuit).
- Per-namespace schema persistence in `brain-metadata::schema_store`: `(namespace, version)` → `SchemaVersionRow` (rkyv) + `namespace -> u32` active pointer. `schema_upload` is atomic — bumps version + writes row + active pointer + fans out into entity_type / predicate / relation_type intern paths.
- System schema bootstrap (load-bearing): `brain-metadata/src/system_schema/schema.brain` is `include_str!`-embedded; parsed + validated + applied at `MetadataDb::open`. Replaces `BUILTIN_PREDICATES` / `BUILTIN_RELATION_TYPES` / `seed_builtin_entity_types` from 16.1 / 17.3 / 18.3 — every built-in registration now flows through the parser + validator + schema_upload, sharing code paths with user uploads. Built-in IDs (`Person == EntityTypeId(1)`, etc) preserved.
- `SchemaUpdatedEvent.namespace` field added; emitted post-commit on UPLOAD.
- SDK `client.schema()` entry with `.upload(&Schema)` / `.upload_text(text)` / `.validate(text)` / `.get(ns, v)` / `.list(ns)`; `SchemaBuilder::new(ns).entity_type(...).predicate(...).relation_type(...).build()` fluent assembler; canonical DSL printer for AST → text round-trip.
- Integration test suite: 8 wire-smoke tests + 1 phase-exit lifecycle test + 6 mock-server SDK tests + schema_ops criterion bench (parse + validate + upload at 50-definition fixture, plus get / list).

**Deferred to later phases (tracked in `spec/21_schema_dsl/07_open_questions.md`):**

- Migration plan computation, schema diff / keep-re-extract-tombstone semantics — out of v1 scope; revisit in v1.1+ (§21/07 Q3).
- `#[derive(BrainEntity)]` / `BrainFact` / `BrainPreference` / `BrainEvent` / `BrainRelation` proc macros — phase 19b or phase 21 (§21/07 Q13).
- Multi-document schemas per namespace + `use other_ns;` cross-namespace imports — post-v1 (§21/07 Q2, Q6).
- Source spans threaded through the AST → validator → wire — phase 19+ improvement (§21/07 Q4 / Q15).
- Per-namespace entity-type ID space (`brain:Person` vs `acme:Person`) — needed once user entity types arrive (later sub-tasks).
- Schema deletion / rollback (§21/07 Q9).
- Validator-version migration when validator rules change (§21/07 Q10).
- Binary-bootstrap migration when system schema content changes across binaries (§21/07 Q11).
- Admin-only authorization for `0x0120-0x0123` (§21/07 Q15 / §28/05 §8) — phase 21 admin.
- Stream-paginated `SCHEMA_LIST` — phase 23.
- `EXTRACTOR_LIST` / `_DISABLE` / `_ENABLE` (`0x0124-0x0126`) — phase 20.

---

## Phase 20 — Pattern + classifier extractors

**One-line:** Extractor framework; pattern (regex) + classifier (small model) tiers run on ENCODE; built-ins (`brain.entity_mentions`, basic NER); extraction audit log.

**Detailed plan:** [`docs/phases/phase-20-pattern-classifier-extractors.md`](docs/phases/phase-20-pattern-classifier-extractors.md)

**Crates touched:** new `brain-extractors`; `brain-core`, `brain-metadata`, `brain-server`.

**Sub-tasks:** 8. **Exit:** ENCODE P99 ≤ 20 ms with extractors active; audit log queryable; tag `phase-20-complete`.

---

## Phase 21 — LLM extractor

**One-line:** LLM extractor kind with cache, retry-once, cost budget, schema-validated output; resolver tier 4 activates.

**Detailed plan:** [`docs/phases/phase-21-llm-extractor.md`](docs/phases/phase-21-llm-extractor.md)

**Crates touched:** new `brain-llm`; `brain-extractors`, `brain-workers`, `brain-metadata`.

**Sub-tasks:** 9. **Exit:** mock-LLM end-to-end test green; real-LLM gated behind opt-in env var; tag `phase-21-complete`.

---

## Phase 22 — Tantivy / lexical retrieval

**One-line:** Tantivy BM25 over memory text + statement text; LexicalRetriever; index workers maintain on writes.

**Detailed plan:** [`docs/phases/phase-22-tantivy-lexical.md`](docs/phases/phase-22-tantivy-lexical.md)

**Crates touched:** `brain-index`, `brain-workers`.

**Sub-tasks:** ~7. **Exit:** lexical recall@10 ≥ targets on reference workload; tag `phase-22-complete`.

---

## Phase 23 — Hybrid query engine

**One-line:** Query router (5 rules), RRF fusion (`k=60`), filter chain (type / temporal / confidence), EXPLAIN/TRACE; `RECALL` transparently uses hybrid path when a schema is declared.

**Detailed plan:** [`docs/phases/phase-23-hybrid-query.md`](docs/phases/phase-23-hybrid-query.md)

**Crates touched:** `brain-planner`, `brain-ops`, `brain-server`, `brain-sdk-rust`.

**Sub-tasks:** ~9. **Exit:** hybrid recall@10 beats semantic-only baseline; EXPLAIN/TRACE structured outputs; tag `phase-23-complete`.

---

## Phase 24 — Sweepers, knowledge acceptance & `v1.0.0`

**One-line:** Backfill worker, FORGET cascade, supersession sweeper, stale-extraction detector, LLM cache sweeper, schema migration runner, schema-toggle runbook, full combined acceptance suite, release.

**Detailed plan:** [`docs/phases/phase-24-acceptance.md`](docs/phases/phase-24-acceptance.md)

**Crates touched:** `brain-workers`, `acceptance/`, `docs/runbooks/`, READMEs, CHANGELOG.

**Sub-tasks:** 12. **Exit:** full functional + performance + storage + operational + schema-toggle acceptance criteria pass; substrate regression continues to pass; tag `phase-24-complete` and `v1.0.0`.

---

## Strict ordering

Phase N+1 doesn't start before Phase N is exited and tagged. The dependencies aren't soft preferences — they're real:

- Phase 1's `Frame` is consumed by Phase 9's connection layer.
- Phase 2's `MetadataSink` trait is implemented by Phase 3.
- Phase 4's `HnswIndex` requires Phase 2's slot reads and Phase 3's tombstone state.
- Phase 7 wires Phases 2-6 together.
- Phase 9 wires everything.
- Phase 11 provides the HTTP substrate Phase 12 instruments on.
- Phase 12 emits the metrics Phase 13 measures.
- Phase 13's chaos rig produces the recovery evidence Phase 14's acceptance gates consume.
- Phase 14's substrate-rc is the foundation Phase 15 builds on without disturbing.
- Phase 15's storage layout is consumed by every knowledge-layer phase.
- Phases 16, 17, 18 layer up the data model; 19 declares it via DSL.
- Phases 20, 21 produce typed data into the model; 22 indexes it for keyword search.
- Phase 23 fuses semantic + lexical + graph retrievers.
- Phase 24's combined acceptance suite is what `v1.0.0` ships against.

Phases 16 through 22 may partially overlap (see DAG in [`docs/phases/README.md`](docs/phases/README.md)). Strict ordering still applies across the substrate/knowledge boundary at Phases 14 → 15.

Skipping ahead means stubbing types you'll have to revisit. Don't.

## How to track progress

- Each completed sub-task is a commit (per [`AUTONOMY.md`](AUTONOMY.md) §5).
- Each completed phase is a tag (`phase-N-complete`).
- Each phase doc has its own `[ ] / [x]` checkboxes per sub-task.
- `git log --oneline | grep "^[a-f0-9]* [0-9]*\."` shows all completed sub-tasks.
- `/status` (slash command) summarizes current position.

## Known limitations of v1

Documented up front so the scope is honest:

- **Single-node only.** Multi-node clustering is v2.
- **No replication.** Backups (snapshots) only. v2.
- **Rust SDK only.** Python/TypeScript/Go are v1.x.
- **Linux only.** Glommio + io_uring don't run elsewhere.

These aren't bugs — they're scope boundaries. Don't accidentally implement them.
