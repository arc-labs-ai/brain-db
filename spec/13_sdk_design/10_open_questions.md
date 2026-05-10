# 13.10 Open Questions

SDK questions unresolved as of this spec version.

---

## OQ-SDK-1: SDK in WASM environments

**Issue.** Users want to run the SDK in browsers (WASM) and edge environments (Cloudflare Workers, Deno).

**Options.**

a) **Native only (status quo).** SDKs require Node.js / browser ecosystems for JavaScript variants.

b) **WASM-compatible.** A subset of the SDK works in WASM (TCP not allowed; needs WebSocket fallback).

**Recommendation.** Add a WebSocket transport option in v1.x. Brain's wire protocol over WebSocket is straightforward.

---

## OQ-SDK-2: Static vs dynamic dispatch

**Issue.** In Rust, builder patterns can use static (compile-time) or dynamic dispatch. Dynamic is more flexible; static is faster.

**Options.**

a) **Static (status quo).** Each operation type is its own builder.

b) **Dynamic.** A single builder with operation type as a field.

**Recommendation.** Stay with static. The performance and ergonomics are better.

---

## OQ-SDK-3: Auto-batching

**Issue.** Multiple ENCODE calls in quick succession could be batched. The SDK could auto-detect and batch.

**Options.**

a) **No auto-batching (status quo).** Explicit `encode_batch` for batching.

b) **Auto-batch based on submission rate.** Up to N ms; batch then send.

**Recommendation.** Defer. Auto-batching adds latency and complicates timing semantics. Explicit batching is clearer.

---

## OQ-SDK-4: Client-side caching

**Issue.** Repeated RECALLs with the same cue could hit a client-side cache.

**Options.**

a) **No cache (status quo).** Every call hits the substrate.

b) **Optional cache with TTL.** Cache hits avoid network.

**Recommendation.** Stay with no cache. The substrate's embedding cache handles most repeated work; client-side caching adds complexity for marginal benefit.

---

## OQ-SDK-5: Stream resumption guarantees

**Issue.** When a SUBSCRIBE stream disconnects and resumes, what's the LSN guarantee?

**Options.**

a) **Best effort (status quo).** Resume from last seen LSN; might miss events in the gap.

b) **Strict no-loss.** The substrate retains events for a window; resumption is lossless.

**Recommendation.** Add (b) as opt-in. Substrate's WAL retention permits lossless resumption within bounds.

---

## OQ-SDK-6: Generated code from schema

**Issue.** SDK code is hand-written per language. Could be generated from a wire-protocol schema.

**Options.**

a) **Hand-written (status quo).** Each SDK is its own codebase.

b) **Generated from schema.** Single source of truth; multiple language outputs.

c) **Hybrid: generate types, hand-write logic.**

**Recommendation.** (c). Types are tedious to maintain by hand; logic benefits from manual care.

---

## OQ-SDK-7: Async runtime selection

**Issue.** Rust SDK uses tokio. Some users want async-std or smol.

**Options.**

a) **Tokio only (status quo).**

b) **Runtime-agnostic.** Abstract over the runtime.

c) **Multiple SDKs (one per runtime).**

**Recommendation.** (a). Tokio is dominant; supporting alternatives adds complexity. Users can wrap if needed.

---

## OQ-SDK-8: gRPC alternative

**Issue.** Brain's wire protocol is custom. Some users prefer gRPC for tooling reasons.

**Options.**

a) **Custom only (status quo).**

b) **Add a gRPC gateway.** A separate component that translates gRPC to Brain's wire protocol.

c) **Native gRPC.** Implement gRPC server in the substrate.

**Recommendation.** (b) as v2 if there's demand. Brain's wire protocol is more efficient; gRPC's value is mostly tooling.

---

## OQ-SDK-9: SDK observability standardization

**Issue.** OpenTelemetry is the standard. The SDK should integrate naturally.

**Options.**

a) **OTel-friendly (current intent).** SDK exposes spans / metrics in OTel format.

b) **OTel-native.** SDK uses OTel APIs directly.

**Recommendation.** (a). OTel-native means hard dependency; OTel-friendly works without OTel installed.

---

## OQ-SDK-10: Test fixture data

**Issue.** Common test fixtures (sample memories, etc.) save boilerplate but may not match real workloads.

**Options.**

a) **Generic fixtures (current intent).** Simple memory text, agent IDs.

b) **Domain-specific fixtures.** "Chatbot fixtures", "knowledge-base fixtures".

**Recommendation.** (a) for general SDK. (b) as separate libraries for specific domains.

---

*Continue to [`11_references.md`](11_references.md) for references.*
