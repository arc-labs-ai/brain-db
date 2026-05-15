# Open Questions

Top-level questions deferred to future versions or beyond. These are *known* unknowns; we made defensible choices for the knowledge layer but the decisions warrant revisiting once we have operational data.

## OQ-V2-1: Learned vs rule-based query routing

**the knowledge layer:** rule-based router (5 rules).
**Open:** train a learned router on labeled queries.
**Why deferred:** need real query traffic to label.
**Path:** future versions ships a learned router behind a feature flag; rules remain as fallback.

## OQ-V2-2: SPLADE-style sparse-neural retrieval

**the knowledge layer:** BM25 only for lexical retrieval (tantivy).
**Open:** add SPLADE as a fourth retriever for sparse-neural matching.
**Why deferred:** inference cost equivalent to dense; gains modest; complexity-to-value is poor for the knowledge layer.
**Path:** later releases — evaluate against real queries.

## OQ-V2-3: Full bitemporal time

**the knowledge layer:** valid time only (`valid_from`, `valid_to`).
**Open:** support as-of-transaction-time queries.
**Why deferred:** doubles per-statement storage cost; most users don't need it.
**Path:** future versions if users request; storage cost is the gate.

## OQ-V2-4: Multi-tenant schema isolation

**the knowledge layer:** one schema per deployment; entities are global within the deployment.
**Open:** per-tenant schemas with isolated entity spaces.
**Why deferred:** affects sharding, query routing, ID spaces; substantial change.
**Path:** v3 design discussion.

## OQ-V2-5: Statement derivation chains (meta-statements)

**the knowledge layer:** statements can have statement IDs in their evidence, with depth cap 3. But active derivation rules are not in the knowledge layer.
**Open:** rule-based derivation engine ("if X reports_to Y, then Y manages X").
**Why deferred:** rule engines invite scope creep; the knowledge layer keeps extraction LLM-driven.
**Path:** future versions experimental.

## OQ-V2-6: Federated knowledge graphs

**the knowledge layer:** single node.
**Open:** multi-node Brain with cross-node query.
**Why deferred:** Brain's value proposition is local-first; federation is a different system.
**Path:** v3.

## OQ-V2-7: Vector embeddings for relations

**the knowledge layer:** entities have embeddings; statements have embeddings; relations don't.
**Open:** embed relations for "find similar relationships" queries.
**Why deferred:** unclear use case; cost of additional HNSW.
**Path:** future versions if requested.

## OQ-V2-8: Schema-as-code language choice

**the knowledge layer:** custom DSL (the `schema.brain` format).
**Open:** alternative formats — YAML, TOML, or a Rust-embedded eDSL.
**Why deferred:** the custom DSL is readable and parseable; benefits of switching are marginal.
**Path:** community feedback decides.

## OQ-V2-9: Real-time extraction acknowledgment

**the knowledge layer:** ENCODE returns once memory is written. Extraction happens after. Client doesn't know when extraction is done.
**Open:** option to wait for synchronous extraction completion in ENCODE.
**Why deferred:** synchronous LLM extraction in ENCODE breaks the substrate's latency contract.
**Path:** future versions add ENCODE_AWAIT_EXTRACTION opcode that returns after pattern + classifier (skipping LLM).

## OQ-V2-10: External knowledge sources

**the knowledge layer:** all knowledge derived from memories within the substrate.
**Open:** import from external KGs (Wikidata, internal databases) as seed entities.
**Why deferred:** out of scope for cognitive substrate; users can ENCODE memories from external sources.
**Path:** v3 if users want first-class external KG bridges.

## OQ-V2-11: Active learning for ambiguous resolutions

**the knowledge layer:** ambiguous resolutions queue for human review.
**Open:** the substrate proposes resolutions when humans review, using an LLM, and learns from human corrections.
**Why deferred:** scope; involves training/feedback loops.
**Path:** future versions.

## OQ-V2-12: Cross-shard graph traversal

**the knowledge layer:** entities and statements sharded by subject. Graph traversal within a shard is fast; cross-shard is fan-out.
**Open:** denormalized cross-shard adjacency for fast multi-hop.
**Why deferred:** complexity; depends on real workloads.
**Path:** future versions if metrics show cross-shard hops are common.

## OQ-V2-13: Statement merging when contradictions resolve

**the knowledge layer:** contradictions are surfaced; user/agent resolves.
**Open:** auto-merge when contradictions have an obvious resolution (e.g., one is superseded).
**Why deferred:** auto-resolution risks silent data loss.
**Path:** future versions for high-confidence cases.

## OQ-V2-14: GUI for schema management and audit review

**the knowledge layer:** CLI and SDK only.
**Open:** web-based admin UI.
**Why deferred:** out of scope.
**Path:** separate project / community.

## OQ-V2-15: Multi-language support in extractors

**the knowledge layer:** built-in extractors assume English.
**Open:** multilingual NER, language-detection, per-language extractors.
**Why deferred:** built-in extractors are limited; users can ship their own LLM extractors that handle other languages.
**Path:** community-contributed extractors; future versions bundled.
