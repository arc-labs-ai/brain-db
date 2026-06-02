> **SUPERSEDED (2026-05-30).** Brain is now a standalone database with no first-party SDK, client library, or JSON gateway. The public interface is the §04 wire protocol (CBOR payloads). This plan is retained for historical context only and is not being executed. See `docs/development/spec-standalone-db-proposal.md`.

# Plan — JSON Gateway + standalone Python SDK

## Decision recap (from the user)

- **Approach:** server-side **JSON-over-HTTP gateway** as a new client transport, then a **thin pure-Python SDK** on top. (Chosen over PyO3 bindings and over a hand-written rkyv-in-Python client.)
- **Timing:** **proceed now**, against the in-flux pre-v1.0 wire — accept rework when the wire changes.

### Roadmap deviation (must record)

`ROADMAP.md:79` scopes non-Rust SDKs to v1.x/v2 via **PyO3, once the wire is frozen**, and `ROADMAP.md:105` says "Don't accidentally implement them." This plan deliberately diverges: a JSON/HTTP transport is not in `spec/04` or `spec/06`, and we build it pre-freeze. Logged as a new `SD-` entry in `docs/development/spec-deviations.md` (see §Deviations).

`spec/06_sdk/00_purpose.md` §3 *does* leave the door open: "Other languages can use the wire protocol directly or generate bindings." The JSON gateway is a third path — an HTTP façade over the same op dispatch.

## Architecture — the reuse seam

A JSON request flows:

```
HTTP request (JSON body, Bearer token)
  → gateway route handler (brain-server/src/gateway/)
  → auth: API key → RequestScope → RequestCaller          [reuse network/auth.rs]
  → translate JSON → brain_protocol::RequestBody          [the real work: DTO layer]
  → ShardHandle::dispatch_op(req, caller)                 [THE SEAM — shard/mod.rs:951]
  → DispatchOutcome::{Single(ResponseBody) | Stream(Vec<ResponseBody>)}
  → translate ResponseBody → JSON                         [DTO layer]
  → HTTP response (single JSON, or NDJSON/SSE for Stream)
```

Everything below `dispatch_op` (handlers, apply layer, writer, WAL, indexes) is untouched. The gateway is a sibling of `admin/`.

Key references:
- Seam: `crates/brain-server/src/shard/mod.rs:951` (`ShardHandle::dispatch_op`).
- Request/response enums: `crates/brain-protocol/src/envelope/request.rs:73` (`RequestBody`), `.../response.rs:62` (`ResponseBody`).
- Outcome: `crates/brain-ops/src/dispatch.rs:174` (`DispatchOutcome`).
- Caller: `crates/brain-ops/src/dispatch.rs:21` (`RequestCaller`), built via `scope.to_caller(session_id)`.
- HTTP infra to mirror: `crates/brain-server/src/admin/mod.rs` (`AdminServer::bind`, `AdminState`), `crates/brain-http` (`Router`), wired in `crates/brain-server/src/main.rs:323`.
- Auth: `crates/brain-server/src/network/auth.rs` (`AuthStore`, `RequestScope`, `derive_scope_from_handshake`).

## Open forks — recommendations (confirm or redirect at approval)

1. **Transport = JSON-over-HTTP (recommended), not gRPC.**
   Reuses `brain-http`/hyper already in-tree; no new heavyweight deps (gRPC ⇒ tonic+prost+protobuf, none on the CLAUDE.md §6 approved list); fastest path to a *pure-Python* client (`httpx`, no codegen, no compiled artifact = maximally "standalone"); human-debuggable while the wire churns. gRPC's win (one `.proto` → Py/TS/Go codegen) only pays off when we want all three SDKs; revisit then.

2. **JSON↔wire translation = thin DTO layer in the gateway (recommended), not serde-on-wire-types.**
   Define serde structs in `brain-server/src/gateway/dto/` with `From`/`TryFrom` conversions to `brain_protocol` types. Keeps rkyv wire types free of a second codec, and lets JSON use human forms (UUID/MemoryId as strings, blobs as JSON objects) instead of leaking packed/rkyv representations. Cost: conversion boilerplate per op. (Alternative: `#[derive(serde::*)]` on `RequestBody`/`ResponseBody` — less boilerplate but couples the wire types to JSON and exposes packed IDs.)

3. **Python SDK location = `clients/python/` in this repo (recommended) for now.**
   Co-evolves with the in-flux wire; easy to keep in lockstep. Can split to its own repo (like `brain-eval`) once the wire freezes. Per the brain-eval memory: **no cross-repo CI/`just` targets** if it ever moves out.

## Scope — milestones

Scoped so milestone 2 delivers the spec's exact "hello world" (`encode`/`recall`/`forget`) fast.

### M0 — Gateway scaffold (Rust)
- `crates/brain-server/src/gateway/` (folder-per-concern: `mod.rs`, `server.rs`, `auth.rs`, `routes/`, `dto/`, `error.rs`).
- New client-facing listener bound to a configurable `gateway_addr` (default `127.0.0.1:9090`), spawned in `main.rs` alongside the admin/metrics servers; `GatewayState { shards: Arc<Vec<ShardHandle>>, auth_store, ... }`.
- Auth middleware: `Authorization: Bearer <api-key>` → `AuthStore` lookup → `RequestScope` → `RequestCaller` (session_id = zeros; agent_id from scope, never client-spoofed).
- JSON error envelope mapping `DispatchError`/`ResponseBody::Error` → HTTP status + `{error, code, retryable}` (mirrors `spec/06_sdk/02_core_api.md` §13 error taxonomy).
- Health: reuse existing `/healthz`.

### M1 — Substrate cognitive ops over JSON
- DTOs + conversions + routes for: `encode`, `recall`, `forget`, `link`, `unlink`, `get_capabilities`, and `plan`/`reason`.
- IDs as strings (MemoryId, AgentId, ContextId). `RequestId` accepted via body or `Idempotency-Key` header for state-mutating ops (`spec/06_sdk/04` §3).
- Streaming: `recall`/`plan`/`reason` that return `DispatchOutcome::Stream` → **NDJSON** (`application/x-ndjson`, one result object per line) for v1; SSE is a later option. `Single` outcomes → one JSON object.
- Integration tests against the in-process harness (`brain-server/tests/support_harness`).

### M2 — Pure-Python SDK (substrate)
- `clients/python/` package `brain`: `Client(base_url=..., auth=..., timeout=...)`, async-first (`httpx.AsyncClient`) + sync wrapper; `encode/recall/forget/link/unlink/get_capabilities/plan/reason`.
- Result/error types (pydantic or dataclasses), keyword-arg API matching `spec/06_sdk/02` exactly.
- Retries w/ backoff + jitter + auto `RequestId` (`spec/06_sdk/04` §1–§6); NDJSON streaming as `async for`.
- `pyproject.toml`, `pytest` suite against a spawned server (or recorded fixtures). The spec's hello-world runs verbatim.

### M3 — Typed-graph ops (deferred within this initiative)
- entity/statement/relation/schema/query/extractor admin.
- Harder bit: `attributes_blob`/`properties_blob` are **nested rkyv** (`spec/04_wire_protocol/02` §20.1). Gateway must encode JSON attrs → `BTreeMap<String, StatementValueWire>` rkyv blob (and decode on read). Evidence inline/overflow, predicate strings, sentinel-zero time fields all need DTO handling.

### M4 — Polish
- Observability (per-request spans/metrics), config docs, end-to-end tutorial, optional SSE, optional TLS.

## Deviations to record (docs/development/spec-deviations.md)
- **SD-6.x-1: JSON/HTTP client gateway + pure-Python SDK ahead of roadmap.** Spec defines only the binary rkyv wire (`spec/04`); roadmap defers non-Rust SDKs to PyO3 post-freeze (`ROADMAP.md:79`). We add an HTTP transport façade now. Rationale: user-directed; unblock a Python consumer + exercise the op surface over JSON before freeze; pure-Python is impractical over rkyv. Reconcile at wire-freeze: either promote the gateway to an official HTTP transport (amend `spec/04`+`spec/06`) or supersede with PyO3.

## Risks / watch-items
- **Wire in flux** → DTOs churn. Mitigation: centralize all translation in `gateway/dto/`; start with the most-stable substrate ops.
- **Auth bypasses the wire handshake** → enforce scope strictly from the API key; never trust a client-supplied agent_id.
- **Streaming semantics differ** (no wire flow-control window over HTTP) → v1 uses simple NDJSON; document the backpressure difference.
- **Nested-rkyv blobs** (M3) are the real complexity; isolated from M1/M2.
- **No new Rust deps** expected (hyper/serde/serde_json already present). Python deps (`httpx`, optional `pydantic`) are outside the Rust dep policy.

## Done-when (milestone gates)
- **M0/M1:** `curl` round-trips encode→recall→forget through the gateway against the in-process harness; auth rejects missing/invalid keys; `just verify` green.
- **M2:** the spec's Python hello-world (`spec/06_sdk/00` §12) runs end-to-end against a live server; Python `pytest` green.

## Next step
Awaiting approval on the three forks (§Open forks) before writing code. On approval, start at M0.
