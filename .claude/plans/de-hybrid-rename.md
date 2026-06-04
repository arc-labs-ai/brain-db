# De-"hybrid" rename — align names to business operation

**Authorized by owner** (2026-06-04): drop "hybrid" everywhere — spec (read-only,
owner-authorized), brain crates, brain-shell. SDKs never used "hybrid" → untouched.

## Why
After the substrate/knowledge consolidation there is **one** read pipeline. Within
`RECALL` there is no "hybrid vs non-hybrid" path (spec §05/03: "the only RECALL
path"). "hybrid" survived as (a) the fused-pipeline concept and (b) the
`RECALL_HYBRID` opcode name. Both get renamed to business-accurate names.

## Canonical rename map (apply verbatim, context-classified)

### Bucket A — the RECALL_HYBRID opcode → QUERY_TEXT
Opcode **numbers unchanged** (0x0163 req / 0x01E3 resp) → wire bytes + SDK
conformance corpus do not move. This is an identifier/type/doc rename only.
- `RECALL_HYBRID` → `QUERY_TEXT`
- `RecallHybridRequest` → `QueryTextRequest`; `RecallHybridResponse` → `QueryTextResponse`
- `RecallHybridReq` → `QueryTextReq`; `RecallHybridResp` → `QueryTextResp`
- `RequestBody::RecallHybrid` → `RequestBody::QueryText`; `ResponseBody::RecallHybrid` → `ResponseBody::QueryText`
- `Self::RecallHybrid` → `Self::QueryText`
- `handle_recall_hybrid` → `handle_query_text`
- test fns `recall_hybrid_request_round_trips`/`..._response_...`/`recall_hybrid_returns_memory_only_results` → `query_text_*`

### Bucket B — the fused pipeline concept → retrieval
"hybrid" meaning the semantic+lexical+graph RRF-fused read pipeline.
- module `hybrid` → `retrieval` (dir + `pub mod`, `brain_planner::hybrid::`, `crate::hybrid::`, `super::hybrid`)
- `HybridExecutorContext` → `RetrievalExecutorContext`
- `HybridUnavailable` → `RetrievalUnavailable`
- idents: `is_hybrid_response`→`is_retrieval_response`, `attach_hybrid_mocks`→`attach_retrieval_mocks`,
  `build_fixture_with_hybrid_mocks`→`build_fixture_with_retrieval_mocks`, `assert_hybrid`→`assert_retrieval`,
  `*uses_hybrid_path`→`*uses_retrieval_path`, `*routes_through_hybrid_pipeline`→`*routes_through_retrieval_pipeline`,
  `*routes_to_hybrid_*`→`*routes_to_retrieval_*`, `recall_p95_hybrid_*`→`recall_p95_retrieval_*`,
  `shutdown_mid_hybrid_recall_*`→`shutdown_mid_retrieval_recall_*`, `hybrid_count`→`retrieval_count`,
  `hybrid_query_surfaces_*`→`retrieval_surfaces_*`, `hybrid_explain_*`→`retrieval_explain_*`,
  `hybrid_trace_*`→`retrieval_trace_*`, `*through_hybrid_recall`→`*through_retrieval_recall`,
  `bench_hybrid_three_retriever`→`bench_retrieval_three_retriever`, `bench_hybrid_router_degraded`→`bench_retrieval_router_degraded`
- prose/strings: "hybrid path/hit/recall/runs/executor/pipeline/API"→"retrieval ...",
  "hybrid retrieval"→"retrieval", "Hybrid-default RECALL routing"→"Retrieval-default RECALL routing"

### File renames (git mv)
- `crates/brain-planner/src/hybrid` → `crates/brain-planner/src/retrieval`
- `crates/brain-planner/benches/hybrid_query.rs` → `.../benches/retrieval.rs` + Cargo.toml `[[bench]] name`
- `crates/brain-server/tests/recall_hybrid_routing.rs` → `.../recall_routing.rs`
- `crates/brain-server/tests/hybrid_phase_exit.rs` → `.../retrieval_phase_exit.rs`

### Mixed-meaning lines (both buckets on one line — read context)
- `brain-ops/handlers/query.rs:202` "RECALL_HYBRID — narrow projection over the hybrid path" → "QUERY_TEXT — narrow projection over the retrieval path"
- `:214` "Wire-level QUERY/RECALL_HYBRID is the low-level hybrid API" → "Wire-level QUERY/QUERY_TEXT is the low-level retrieval API"
- file cross-refs in comments (`recall_concurrent.rs:64`, `hybrid_phase_exit.rs:16`) → point at renamed files

## Order
1. git mv the 4 paths.
2. brain-protocol (contract) — edit + `cargo check -p brain-protocol` on macOS.
3. Fan out consumers: brain-ops, brain-planner, brain-server(tests), brain-workers, brain-index, brain-rerank, brain-embed.
4. spec/ docs (owner-authorized edit of read-only tree).
5. brain-shell (separate repo) — help.rs + render comments; also fix the stale
   "substrate path vs hybrid path" dual-mode copy (contradicts spec one-path).
6. Gate: `just docker-verify` (Linux). brain-protocol + brain-shell verify on macOS.

## Verification constraints
- Most crates are Linux-only (glommio) → final gate is Docker. Only brain-protocol
  + brain-shell compile on macOS.
- SDK conformance corpus must stay green (numbers unchanged) — sanity-check no .rs
  in brain-sdk references "hybrid" (confirmed: zero).
