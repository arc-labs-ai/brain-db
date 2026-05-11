# Sub-task 5.4 — Dispatcher trait + CPU passthrough

The phase doc imagined a channel-fed window-and-batch machine (5 ms window, batch up to 32). The spec is more restrictive: **§04/03 §7** and **§04/03 §14** are both explicit — *"There is no batching on the CPU path; each request goes through the model independently."* So in v1 (CPU-only) we ship the **dispatch surface**, not the dispatch machinery.

The trait+CPU pass-through gives Phase 7 (cognitive ops) and Phase 9 (server) a single object to call, and gives a future GPU sub-task a slot to plug into without re-spelling the API.

User direction confirmed via AskUserQuestion before this plan.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/03 §7 | "The substrate doesn't internally batch CPU inference. Each request goes through the model independently. CPU batching has marginal benefits at small batch sizes and adds complexity; we don't bother." |
| §04/03 §14 | (the GPU §) "There is no batching on the CPU path; each request goes through the model independently. CPU inference scales by adding cores." |
| §04/06 §1–§5 | Window + batch + per-shard batchers — explicitly the GPU path. Defers to a future sub-task. |
| §04/06 §11 | Backpressure (`EmbeddingOverloaded`) — also GPU-path machinery; not built here. |
| §04/03 §7 (cont.) | "Multiple Glommio executors can call inference concurrently. Each call runs on the current core. The model's weights are shared across all callers via `Arc<Model>`." → the dispatcher must be `Send + Sync` so it can sit behind an `Arc` and serve many callers concurrently. |

Net: the CPU dispatcher is a `Send + Sync` object that forwards `embed`/`embed_batch` to 5.3's free functions and lets callers use one type whether the eventual deployment is CPU or GPU.

## 1. Scope

**In scope for 5.4:**
- `Dispatcher` trait (sync, in-line with §04/03 §7's "each request runs on the current core" + the orientation plan's "sync API in v1"):
  ```rust
  pub trait Dispatcher: Send + Sync {
      fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>;
      fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;
      fn fingerprint(&self) -> [u8; 16];
  }
  ```
- `CpuDispatcher` impl that owns the `ModelHandle` behind an `Arc` and forwards to 5.3's free functions. No queue, no window, no per-shard logic; pure pass-through.
- Concurrency property test (`std::thread` based, not `tokio` — CPU dispatcher is sync): N threads call `dispatcher.embed(t)` simultaneously, all results equal the serial result. Validates `Send + Sync` and that the model can be shared via `Arc`.
- One new `EmbedError` variant if and only if it's actually triggered (likely none — the dispatcher only propagates from inner functions).

**NOT in scope (later sub-tasks / phases):**
- The LRU cache — 5.5 (and the dispatcher is where the cache plugs in).
- The window-and-batch GPU dispatcher — future Phase 5.x or 11+.
- Backpressure / `EmbeddingOverloaded` — comes with GPU work.
- `EmbedderConfig::max_batch_size`, `batch_window_ms` — would be GPU-only; do not add yet.
- async / Glommio integration — Phase 9 wraps the sync dispatcher.

## 2. Why a trait at all?

If we only ever shipped CPU, free functions would suffice. The trait justifies itself because:

1. **Forward-compat slot for GPU.** The future `GpuDispatcher` lives behind the same trait; callers don't change. Spec §04/03 §7 itself uses the language of "the substrate" — implying a single dispatch surface.
2. **Cache plug point.** 5.5's `CachingDispatcher<D>` wraps any `D: Dispatcher`. Without a trait, the cache would have to be hard-coded against `ModelHandle`.
3. **Test seam.** Phase 7 (ops) can test against a `MockDispatcher` instead of loading 130 MiB BGE-small in every test. The trait makes that natural.

The trait is small (3 methods) and `Dispatcher` carries no associated types — easy to pin without locking us in.

## 3. Module surface

```rust
// crates/brain-embed/src/dispatcher.rs

use std::sync::Arc;

use crate::error::EmbedError;
use crate::forward::{embed_batch, embed_text, VECTOR_DIM};
use crate::model::ModelHandle;

/// Sync, thread-safe surface for text→vector. CPU path is a pure
/// passthrough per spec §04/03 §7 + §14 ("no batching on CPU"). Future
/// GPU impl will live behind this same trait with a windowed batcher.
pub trait Dispatcher: Send + Sync {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;
    fn fingerprint(&self) -> [u8; 16];
}

/// CPU dispatcher: every call goes through the model independently
/// per spec §04/03 §7. Concurrent callers share the weights via Arc;
/// activations are per-call (5.3 builds them).
pub struct CpuDispatcher {
    model: Arc<ModelHandle>,
}

impl CpuDispatcher {
    pub fn new(model: ModelHandle) -> Self;
    pub fn from_arc(model: Arc<ModelHandle>) -> Self;
    /// Borrow the inner handle. Useful in tests + as an escape hatch
    /// for callers that need the raw tokenizer or forward primitives.
    pub fn handle(&self) -> &Arc<ModelHandle>;
}

impl Dispatcher for CpuDispatcher { /* forwards to 5.3 */ }
```

Re-exports from `lib.rs`: `Dispatcher`, `CpuDispatcher`.

## 4. Implementation decisions

### 4.1 `ModelHandle: Send + Sync`?

`ModelHandle` owns:
- `BertModel` from `candle_transformers` — internally `Tensor`s, which wrap `Arc<...>`. Candle types are `Send + Sync` (we'll verify with a static-assert in `dispatcher.rs`).
- `Tokenizer` from `tokenizers` 0.21 — documented `Send + Sync` (spec §04/02 §8 also asserts this).
- `[u8; 16]` fingerprint — trivially `Send + Sync`.
- `Device::Cpu` + `DType::F32` — both `Copy`.

We add a `const _: fn() = || { fn assert<T: Send + Sync>() {} assert::<ModelHandle>(); };` compile-time check inside `dispatcher.rs`. If candle ever stops being thread-safe, the build breaks loudly.

### 4.2 `Arc<ModelHandle>` vs `Box<ModelHandle>`

Spec §04/03 §7 specifically calls out `Arc<Model>` shared across callers. `CpuDispatcher` owns `Arc<ModelHandle>`; cheap to clone, multiple dispatchers can share a single loaded model (useful for tests + multi-shard deployments).

### 4.3 Why sync, not async

Three reasons, in order:

1. Spec §04/03 §7: "Each call runs on the current core." That's a sync description.
2. Orientation plan §4.4 settled on sync API in v1 (user confirmed).
3. The forward pass is pure CPU work; `async fn embed` would just be a thin wrapper that immediately blocks the executor. Phase 9's server wraps the sync dispatcher in `spawn_blocking` (or Glommio's equivalent) at the network boundary — that's the right place for async.

### 4.4 No new errors expected

`CpuDispatcher::embed` forwards `forward::embed_text`'s `Result`. Any failure is already classified by 5.1–5.3's `EmbedError` variants. The dispatcher is a pure forwarder; it doesn't add any failure modes.

If a future GPU dispatcher needs `EmbeddingOverloaded` (spec §04/06 §11), that variant gets added at that time, not now.

### 4.5 `Dispatcher::fingerprint()` is in the trait

Callers (cache in 5.5, Phase 7 ops, Phase 9 metrics) need the model fingerprint to:
- Key the cache by `(fingerprint, text_hash)` so cache stays valid across model changes (spec §04/05 §3).
- Stamp stored vectors with their model's fingerprint (Phase 7's ENCODE op consults `brain-metadata`'s `model_fingerprints` table from Phase 3.8).

Putting `fingerprint()` on the trait keeps the cache (5.5) clean — it wraps any `Dispatcher`, not just `CpuDispatcher`.

### 4.6 Concurrency property test

The substantive test for this sub-task. Without a real model we can't run the model; but we can prove the *type surface* is correct:

```rust
#[test]
fn dispatcher_is_send_sync() {
    fn require<T: Send + Sync>() {}
    require::<CpuDispatcher>();
    require::<dyn Dispatcher>(); // object-safety check
}
```

Plus a model-gated integration test (`tests/dispatcher.rs`):

```rust
#[test]
fn cpu_dispatcher_concurrent_calls_match_serial() {
    // gated on BRAIN_EMBED_MODEL_DIR; spawn 8 std::threads each calling
    // dispatcher.embed("the quick brown fox"). All 8 vectors must equal
    // the serial result (cosine ≥ 1 - 1e-6).
}
```

This is the empirical proof that `Arc<ModelHandle>` + candle's `Tensor` are actually thread-safe in production, not just in `Send + Sync` impls.

### 4.7 What about `EmbedderConfig`?

5.1's `EmbedderConfig` has `warmup_iters`, no batching fields. 5.4 does **not** extend it — there's nothing to configure on the CPU path. When GPU lands, that's when `batch_window_ms` and `max_batch_size` join the config.

### 4.8 Why expose `embed_batch` if there's no batching

CPU has no *automatic* batching, but callers that already have a batch of texts (e.g., a future `ADMIN_REINDEX` worker, or Phase 7's `BATCH_ENCODE`) should still be able to hand the batch in one call. 5.3 already supports this efficiently — `forward_pooled` runs all rows through one BERT forward pass on the CPU, which IS faster per-text than N serial single-text calls (matmul is amortised across the batch even on CPU, just less dramatically than on GPU).

This is consistent with spec §04/03 §7: *"the substrate doesn't internally batch"* — i.e. doesn't *queue + window* to assemble batches. But it accepts caller-provided batches.

### 4.9 Doc comment per spec deviation

The `CpuDispatcher::embed_batch` doc explicitly notes:
> Per spec §04/03 §7, the substrate does not assemble batches itself.
> If you call this with multiple texts, they are run as one BertModel
> forward pass (cheaper than N serial single-text calls). The "no
> batching" rule means no time-window queueing — not that batches are
> forbidden.

This is the docstring the user will see; capturing it here so the wording survives implementation.

## 5. Files written / changed

```
crates/brain-embed/src/dispatcher.rs           [new]
crates/brain-embed/src/lib.rs                  [edit: mod + re-exports]
crates/brain-embed/tests/dispatcher.rs         [new — gated on BRAIN_EMBED_MODEL_DIR]
```

No `Cargo.toml` change. No new workspace deps. No `EmbedError` change.

## 6. Verify checklist

- `cargo build -p brain-embed` clean.
- `cargo test -p brain-embed` — existing 28 + ~3 new (Send+Sync compile-time check, an object-safety check, plus the gated concurrency integration test).
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-embed` no diff.

## 7. Commit message (draft)

```
feat(brain-embed): Dispatcher trait + CpuDispatcher passthrough (sub-task 5.4)

Spec §04/03 §7 and §14 are explicit: no batching on the CPU path.
v1 ships the dispatch *surface* — a Send + Sync Dispatcher trait —
without the GPU-only window-and-batch machinery the phase doc
imagined. CpuDispatcher is a pure passthrough wrapping
Arc<ModelHandle>; future GpuDispatcher will plug in behind the same
trait.

- Dispatcher trait: embed, embed_batch, fingerprint. Send + Sync;
  object-safe.
- CpuDispatcher::new / from_arc / handle — forwards to 5.3's
  embed_text / embed_batch.
- Compile-time Send + Sync assertion on ModelHandle + CpuDispatcher
  guards against candle losing thread-safety upstream.
- tests/dispatcher.rs (gated on BRAIN_EMBED_MODEL_DIR): 8 std::threads
  call embed concurrently; all vectors match the serial result to
  cosine ≥ 1 - 1e-6.

Verify: cargo build/test/clippy -p brain-embed.
```

## 8. Risks

- **candle thread-safety surprise**: if `BertModel` or its underlying tensors aren't actually `Send + Sync` in 0.8.x, the compile-time assertion fails and we need a `Mutex<ModelHandle>` workaround. Mitigated by: candle's `Tensor` uses `Arc<Storage>` internally and is documented `Send + Sync`; verified at build time.
- **`Tokenizer::encode` thread-safety**: spec §04/02 §8 says safe; tokenizers 0.21 confirms. The `encode_batch_char_offsets` we used in 5.2 takes `&Tokenizer` and is thread-safe per crate docs.
- **`EmbedError` not `Sync`**: it might contain `std::io::Error` (which is `Send + !Sync` in some compositions). Doesn't affect the dispatcher because `EmbedError` doesn't need to be `Sync` — it's the *return* type, not stored shared state. The trait is `Send + Sync`, not its return values.

## 9. Out-of-scope flags (re-confirm)

- **No `EmbedderConfig` changes.** Window/batch fields are GPU-only.
- **No `lru` workspace dep yet.** Lands with 5.5.
- **No fallback / overload logic.** Spec §04/06 §11 is GPU territory; CPU path can't overload itself in a way that needs the queue threshold from §11.
- **Doc comment on CpuDispatcher::embed_batch explains the §04/03 §7 nuance** (see §4.9 above).

---

PLAN READY.
