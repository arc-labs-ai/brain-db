# Sub-task 7.1 — `Operation` dispatcher + `OpsContext` + `OpError`

Foundation for Phase 7. Builds the skeleton everything else in 7.2–7.10 plugs into. No real handler logic — stub functions return `NotYetImplemented`; subsequent sub-tasks fill them in.

What 7.1 actually ships: the **shape of brain-ops** — its context type, error taxonomy, and the top-level `dispatch()` async function that matches a `RequestBody` to a per-op handler.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §09/01 §1 | Each operation is a request-response interaction (or streaming for SUBSCRIBE). Wire request → wire response |
| §09/01 §10 | Eventual consistency by default; read-after-write on demand |
| §09/01 §12 | Stable error codes: `InvalidRequest`, `NotFound`, `QuotaExceeded`, `Unauthorized`, `Conflict`, `Overloaded`, `InternalError`. Each has a code, message, and retryable flag |
| §09/01 §13 | SUBSCRIBE is the only streaming primitive; other ops are request-response |
| §09/01 §5 | RECALL returns approximate (HNSW); for exact, fallback to brute-force for small shards (not in v1) |
| Orientation plan §4.7 | Dispatcher is the top-level entry replacing planner's `execute()` |
| Orientation plan §5 | All 11 sub-tasks land; 7.1 is the skeleton 7.2–7.11 fill |

## 1. Scope

**In scope for 7.1:**
- `crates/brain-ops/src/error.rs` — `OpError` with the spec §12 categories + planner / executor `#[from]` conversions + a `NotYetImplemented` stub variant.
- `crates/brain-ops/src/context.rs` — `OpsContext` wrapping `brain_planner::ExecutorContext` + reserved slots for later sub-tasks (subscribe broadcast, etc.).
- `crates/brain-ops/src/dispatch.rs` — async `dispatch(req: RequestBody, ctx: &OpsContext) -> Result<ResponseBody, OpError>` with a match arm per wire variant.
- `crates/brain-ops/src/{encode,recall,plan,reason,forget,link,txn,subscribe}.rs` — stub handler modules. Each exposes one `async fn handle_*` returning `OpError::NotYetImplemented`. 7.3–7.10 replace the stubs with real implementations.
- `OpError::error_code()` + `OpError::retryable()` methods that map to spec §12's stable code table.
- Cargo.toml: declare every workspace member brain-ops needs (brain-protocol, brain-planner, brain-embed, brain-index, brain-metadata, brain-core, thiserror, tracing, parking_lot; dev: tempfile, tokio, uuid).
- Unit tests:
  - `dispatch` for every implemented variant returns `NotYetImplemented` (compile-time exhaustive match).
  - `OpError` round-trips through Display.
  - `OpsContext` is `Send + Sync` (compile-time check).
  - `error_code()` returns expected wire codes for each variant.

**NOT in scope for 7.1:**
- Real handler bodies — they land in 7.3–7.10.
- LINK / UNLINK / UNFORGET wire variants — these aren't in `RequestBody` yet (Phase 1 didn't ship them). 7.8 / 7.7 will extend `brain-protocol` first, then wire through here. The dispatcher's match arms for these don't exist in 7.1.
- The real `WriterHandle` impl — that's a sibling sub-task (7.2's idempotency layer naturally extends into it, but ships as part of 7.2 or as its own piece).
- Admin ops (`AdminStats`, `AdminSnapshot`, etc.) — Phase 8 (workers) and Phase 9 (server) own those. 7.1's dispatcher returns `NotYetImplemented` for every admin variant.
- Wire-frame parsing / response framing — that's brain-server's job. 7.1's dispatcher works on already-parsed `RequestBody`.

## 2. Module surface

```rust
// crates/brain-ops/src/lib.rs
pub mod context;
pub mod dispatch;
pub mod error;

// Stub handler modules — bodies land in later sub-tasks.
pub mod encode;
pub mod recall;
pub mod plan;
pub mod reason;
pub mod forget;
pub mod link;     // wire variants come in 7.8
pub mod txn;
pub mod subscribe;

pub use context::OpsContext;
pub use dispatch::dispatch;
pub use error::{OpError, ErrorCode};
```

```rust
// context.rs
use std::sync::Arc;
use brain_planner::ExecutorContext;

#[derive(Clone)]
pub struct OpsContext {
    pub executor: ExecutorContext,
    // 7.10 adds: pub subscribe_tx: Arc<broadcast::Sender<SubscribeEvent>>,
    // 7.9  adds: pub txn_store:    Arc<Mutex<TxnStore>>,
}

impl OpsContext {
    pub fn new(executor: ExecutorContext) -> Self { Self { executor } }
}

const _: fn() = || { fn req<T: Send + Sync>() {} req::<OpsContext>(); };
```

```rust
// error.rs

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    #[error("idempotency conflict: {0}")]
    Conflict(String),

    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("overloaded: {0}")]
    Overloaded(String),

    /// Spec §08/06 §6 — single forget cap.
    #[error("too many memories targeted by one request")]
    TooManyMemories,

    /// Spec §09/08 §9 — txn duration cap exceeded.
    #[error("transaction expired")]
    TxnExpired,

    /// Phase 7 sub-task placeholder. Replaced as each handler lands.
    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),

    #[error(transparent)]
    PlanError(#[from] brain_planner::PlanError),

    #[error(transparent)]
    ExecError(#[from] brain_planner::ExecError),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Spec §09/01 §12 — stable wire error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    QuotaExceeded,
    Unauthorized,
    Conflict,
    Overloaded,
    InternalError,
}

impl OpError {
    pub fn error_code(&self) -> ErrorCode { /* match-based mapping */ }
    pub fn retryable(&self) -> bool { /* Overloaded + transient internal */ }
}
```

```rust
// dispatch.rs

use brain_protocol::request::RequestBody;
use brain_protocol::response::ResponseBody;
use crate::{context::OpsContext, error::OpError};

pub async fn dispatch(
    req: RequestBody,
    ctx: &OpsContext,
) -> Result<ResponseBody, OpError> {
    match req {
        RequestBody::Encode(r)             => crate::encode::handle_encode(r, ctx).await.map(ResponseBody::Encode),
        RequestBody::EncodeVectorDirect(_) => Err(OpError::NotYetImplemented("EncodeVectorDirect")),
        RequestBody::Recall(r)             => crate::recall::handle_recall(r, ctx).await.map(ResponseBody::Recall),
        RequestBody::Plan(r)               => crate::plan::handle_plan(r, ctx).await.map(ResponseBody::Plan),
        RequestBody::Reason(r)             => crate::reason::handle_reason(r, ctx).await.map(ResponseBody::Reason),
        RequestBody::Forget(r)             => crate::forget::handle_forget(r, ctx).await.map(ResponseBody::Forget),
        RequestBody::Subscribe(r)          => crate::subscribe::handle_subscribe(r, ctx).await.map(ResponseBody::SubscribeEvent),
        RequestBody::Unsubscribe(r)        => crate::subscribe::handle_unsubscribe(r, ctx).await.map(ResponseBody::Unsubscribe),
        RequestBody::TxnBegin(r)           => crate::txn::handle_txn_begin(r, ctx).await.map(ResponseBody::TxnBegin),
        RequestBody::TxnCommit(r)          => crate::txn::handle_txn_commit(r, ctx).await.map(ResponseBody::TxnCommit),
        RequestBody::TxnAbort(r)           => crate::txn::handle_txn_abort(r, ctx).await.map(ResponseBody::TxnAbort),

        // Connection / lifecycle — brain-server owns these.
        RequestBody::Hello(_) | RequestBody::Auth(_) | RequestBody::Bye(_)
        | RequestBody::Ping(_) | RequestBody::ClientPong(_)
        | RequestBody::CancelStream(_) => Err(OpError::NotYetImplemented("connection-lifecycle op")),

        // Admin — Phase 8 / 9.
        RequestBody::AdminStats(_) | RequestBody::AdminSnapshot(_)
        | RequestBody::AdminRestore(_) | RequestBody::AdminIntegrityCheck(_)
        | RequestBody::AdminMigrateEmbeddings(_) | RequestBody::AdminCreateContext(_)
        | RequestBody::AdminRenameContext(_) /* … */ => Err(OpError::NotYetImplemented("admin op")),
    }
}
```

Each stub handler module is ~5 lines:

```rust
// encode.rs (stub for 7.1)
use brain_protocol::request::EncodeRequest;
use brain_protocol::response::EncodeResponse;

use crate::{context::OpsContext, error::OpError};

pub async fn handle_encode(
    _req: EncodeRequest,
    _ctx: &OpsContext,
) -> Result<EncodeResponse, OpError> {
    Err(OpError::NotYetImplemented("ENCODE — sub-task 7.3"))
}
```

Same shape for recall / plan / reason / forget / subscribe / txn.

## 3. Implementation decisions

### 3.1 Response variant naming alignment

The wire `ResponseBody` variants and the request side are mostly aligned (`Encode` ↔ `Encode`, `Recall` ↔ `Recall`, etc.), but `Subscribe` is asymmetric: the request is `Subscribe(SubscribeRequest)` and the **first** response is `SubscribeEvent(SubscriptionEvent)` because subscribe is streaming. For 7.1's dispatcher we use `ResponseBody::SubscribeEvent` as the first response; subsequent events come from the stream wired in 7.10.

This is a small mismatch between the request/response variant pairing. Document.

### 3.2 `OpsContext` minimalism

7.1 keeps `OpsContext` as a thin wrapper around `brain_planner::ExecutorContext`. Each later sub-task that needs new shared state (txn store, subscribe broadcast) adds a field. Non-breaking — additive.

Phase 9's server will likely add `tracing::Span`-derived request metadata, rate limiters, etc. Those live on `OpsContext` too.

### 3.3 `OpError::error_code()` mapping

Per spec §09/01 §12 the wire-level error code is a small set of stable categories. The mapping:

| `OpError` variant | `ErrorCode` |
|---|---|
| `InvalidRequest` | `InvalidRequest` |
| `NotFound { … }` | `NotFound` |
| `Conflict(_)` | `Conflict` |
| `QuotaExceeded(_)` | `QuotaExceeded` |
| `Unauthorized(_)` | `Unauthorized` |
| `Overloaded(_)` | `Overloaded` |
| `TooManyMemories` | `InvalidRequest` (it's a user-error: too big a request) |
| `TxnExpired` | `Conflict` (the txn was invalidated) |
| `NotYetImplemented(_)` | `InternalError` |
| `PlanError::QueryTooExpensive { … }` | `InvalidRequest` |
| `PlanError::InvalidParameters { … }` | `InvalidRequest` |
| `PlanError::Unsupported(_)` | `InternalError` |
| `ExecError::EmbedFailed(_)` | `InternalError` |
| `ExecError::IndexSearchFailed(_)` | `InternalError` |
| `ExecError::MetadataReadFailed(_)` | `InternalError` |
| `ExecError::MemoryNotFound { … }` | `NotFound` |
| `ExecError::WriterFailed(WriterError::Overloaded)` | `Overloaded` |
| `ExecError::Unsupported(_)` | `InternalError` |
| `ExecError::Internal(_)` | `InternalError` |
| `Internal(_)` | `InternalError` |

### 3.4 `retryable()` rule

Per spec §12 each error has a retryable flag. The rule:

- `Overloaded(_)` → `true`
- `ExecError::WriterFailed(WriterError::Overloaded)` → `true`
- Everything else → `false` (including `Internal` — operators investigate before retry)

### 3.5 LINK / UNLINK / UNFORGET

Wire `RequestBody` doesn't have these variants in Phase 1. Phase 7 must add them in `brain-protocol` before 7.7's UNFORGET and 7.8's LINK / UNLINK can wire through. **7.1 doesn't add the wire variants** — those changes belong to the sub-tasks that need them.

When 7.8 lands, the dispatcher gains `RequestBody::Link(_)` / `RequestBody::Unlink(_)` arms and the `RequestBody` enum gets the new variants. Same for `RequestBody::Unforget(_)` in 7.7.

This is a deliberate scope cut: 7.1 ships the dispatcher with what's currently wireable. The wire extensions land alongside the handler that consumes them.

### 3.6 `#[from]` on `PlanError` + `ExecError`

Both are convertible into `OpError` via `#[from]`. This means each handler's `?` operator propagates upstream errors without manual mapping. The trade-off: an `OpError::PlanError(PlanError::InvalidParameters)` payload is slightly less direct than a flat `OpError::InvalidParameters`, but the spec-§12 mapping via `error_code()` collapses them to the same wire code.

Acceptable; alternative (flattening every variant) doubles the surface area.

### 3.7 `EncodeResponse` shape

Wire `EncodeResponse` is one variant for both `Encode` and `EncodeVectorDirect`. We map both successfully-handled cases to the same response type, so dispatcher's match collapses cleanly.

### 3.8 `Subscribe` streaming

7.1's stub returns `Err(NotYetImplemented)`. 7.10 ships the real `handle_subscribe` returning `Result<SubscriptionEvent, OpError>` for the first event; the stream itself goes through a side channel (broadcast). Wire framing for the stream is Phase 9. Documented in 7.10's plan.

### 3.9 Compile-time exhaustive match

The `match req { … }` is exhaustive over `RequestBody` variants. If Phase 1 adds a new variant (Phase 7's LINK / UNLINK / UNFORGET extensions), the match fails to compile until the new arm is added — which is the bug-prevention guarantee we want. No `_ => Err(NotYetImplemented)` catch-all.

## 4. Test plan

### 4.1 Pure unit tests

- `dispatch_compiles_with_all_variants` — empty test that proves the `match` is exhaustive over `RequestBody`. Cargo's compile failure on a missing arm is the actual assertion; the test exists so the failure is loud.
- `dispatch_returns_not_yet_implemented_for_each_handler` — constructs a minimal `RequestBody::Encode(_)` etc. and asserts `Err(OpError::NotYetImplemented(_))` for each currently-supported variant.
- `op_error_display` — each variant displays with a useful message.
- `op_error_error_code_mapping` — table-driven; for each variant, the right `ErrorCode` is returned.
- `op_error_retryable_flag` — `Overloaded` → true, everything else → false.
- `ops_context_is_send_sync` — compile-time `Send + Sync` assertion.

### 4.2 Skip integration tests for now

7.1's handlers all return `NotYetImplemented`. No real storage interaction yet. Integration tests start landing in 7.3 (the first real handler).

## 5. Files written / changed

```
crates/brain-ops/Cargo.toml             [edit: add brain-* deps + workspace dev-deps]
crates/brain-ops/src/lib.rs             [edit: mod decls + re-exports]
crates/brain-ops/src/context.rs         [new]
crates/brain-ops/src/error.rs           [new]
crates/brain-ops/src/dispatch.rs        [new]
crates/brain-ops/src/encode.rs          [new — stub]
crates/brain-ops/src/recall.rs          [new — stub]
crates/brain-ops/src/plan.rs            [new — stub]
crates/brain-ops/src/reason.rs          [new — stub]
crates/brain-ops/src/forget.rs          [new — stub]
crates/brain-ops/src/link.rs            [new — empty, scaffold for 7.8]
crates/brain-ops/src/txn.rs             [new — stubs for begin/commit/abort]
crates/brain-ops/src/subscribe.rs       [new — stubs for subscribe/unsubscribe]
```

No new external deps. All workspace members already declared.

## 6. Verify checklist

- `cargo build -p brain-ops` clean (dev container).
- `cargo test -p brain-ops` — ~6 new unit tests.
- `cargo clippy -p brain-ops --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-ops` no diff.

## 7. Commit message (draft)

```
feat(brain-ops): Operation dispatcher + OpsContext + OpError (sub-task 7.1)

Foundation for Phase 7. Ships the skeleton each later handler plugs
into:

- OpsContext: thin wrapper over brain_planner::ExecutorContext;
  later sub-tasks extend with txn store + subscribe broadcast.
- OpError: spec §09/01 §12 error taxonomy with #[from] conversions
  from PlanError + ExecError. error_code() maps every variant to a
  stable wire ErrorCode; retryable() pins Overloaded only.
- dispatch(req, &ctx) -> Result<ResponseBody, OpError>: top-level
  async entry. Exhaustive match over RequestBody so Phase 1 wire
  changes force compile errors here.
- Stub handler modules (encode/recall/plan/reason/forget/txn/
  subscribe) each export one async fn returning NotYetImplemented.
  Sub-tasks 7.3-7.10 replace the stubs.

Excluded from 7.1's match arms:
- Connection lifecycle (Hello/Auth/Bye/Ping/CancelStream) — brain-
  server territory.
- Admin ops — Phase 8 / 9.
- LINK / UNLINK / UNFORGET — wire variants don't exist yet; 7.7 /
  7.8 will extend brain-protocol when they need them.

Tests: 6 unit tests pinning the match exhaustiveness, stub returns,
error-code mapping, retryable flag, OpsContext Send+Sync.

Built / tested inside the Linux dev container (brain-metadata pulls
in brain-storage transitively).
```

## 8. Risks

- **`ResponseBody::Subscribe` mismatch**. The wire response variant for SUBSCRIBE is `SubscribeEvent(SubscriptionEvent)`, not `Subscribe(_)`. The dispatcher returns the first event; subsequent stream items go through the broadcast channel wired in 7.10. Document.
- **`RequestBody` has more variants than we handle**. The exhaustive match must cover every variant. Connection lifecycle + admin variants all map to `NotYetImplemented`. If the variant count is high, this list is long but mechanical.
- **`#[from]` ambiguity**. If `PlanError` and `ExecError` ever gain a common variant name (`Internal`?), `#[from]` could pick either. The current shape has no overlap.

## 9. Out-of-scope flags (re-confirm)

- No real handler logic. Every handler is a stub.
- No `WriterHandle` implementation. 7.2 / sibling ships the no-WAL real writer.
- No wire-level changes to `brain-protocol`. 7.7 / 7.8 land those.
- No tests against real storage.
- No `Admin*` handlers (Phase 8 / 9).
- No `tokio::sync::broadcast` channel (added in 7.10).

---

PLAN READY.
