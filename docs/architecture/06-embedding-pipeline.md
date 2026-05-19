# 06 — Embedding pipeline

**Audience:** anyone wondering why Brain owns the embedding step
instead of accepting precomputed vectors, what a "stale vector"
is, or where the latency of an `ENCODE` actually goes.

**Goal:** by the end you should be able to trace a string of text
from the wire all the way to a 384-byte L2-normalised vector,
know which steps run on which thread, name the fingerprint that
ties a vector to its model, and explain when the cache hits and
when it doesn't.

This chapter assumes [03 — Arena and WAL](03-arena-and-wal.md)
(the slot the vector lives in) and
[04 — HNSW index](04-hnsw-index.md) (what it gets indexed into).

---

## Why the substrate owns embedding

A client `ENCODE`s text, not a vector. That is a deliberate
inversion of what most vector databases do, and it carries the
whole chapter's weight.

Four things happen when the substrate owns the embedder:

- **Deduplication is semantic.** Two memories with the same
  embedding get recognised as duplicates — the substrate decides
  whether to coalesce them, not the client.
- **Re-embedding is possible.** When the model upgrades, the
  substrate re-runs every memory's text through the new model and
  stamps the new fingerprint. A client that sent pre-computed
  vectors has no way to be retroactively upgraded.
- **The cache is keyed on text, not on caller-computed hashes.**
  Anyone re-asking the same question (a common pattern with LLM
  agents) skips inference entirely.
- **The operator picks the model.** Agents don't decide it; the
  deployment does. That keeps embedding-space consistency a
  property of the cluster, not of the most lax client.

The cost is making the substrate ML-aware: it has to load weights,
run inference, manage a cache, version fingerprints. We pay it.
The crate is `brain-embed` (`crates/brain-embed/src/lib.rs:1`);
`#![forbid(unsafe_code)]` at the root
(`crates/brain-embed/src/lib.rs:29`).

---

## The mental model

```
   text on the wire
        │
        ▼
   ┌──────────────────────────┐
   │ Caching layer            │   key = BLAKE3(text)[..16]
   │ (CachingDispatcher)      │   value = (vector, fingerprint)
   └────────┬─────┬───────────┘
            │     └─── hit ──► return cached vector
            │ miss
            ▼
   ┌──────────────────────────┐
   │ Tokeniser                │   WordPiece, 512-token cap
   │ tokenize::encode_batch   │   → (input_ids, token_type_ids,
   └────────┬─────────────────┘      attention_mask) tensors
            │
            ▼
   ┌──────────────────────────┐
   │ BertModel forward        │   candle_transformers
   │ model.forward(...)        │   → (batch, seq, 384) hidden state
   └────────┬─────────────────┘
            │
            ▼
   ┌──────────────────────────┐
   │ [CLS] pool               │   take hidden[:, 0, :]
   │ extract_cls              │   → (batch, 384)
   └────────┬─────────────────┘
            │
            ▼
   ┌──────────────────────────┐
   │ L2 normalise             │   reject NaN / Inf / near-zero
   │ l2_normalize_in_place    │   → [f32; 384] unit vector
   └────────┬─────────────────┘
            │
            ▼
   ┌──────────────────────────┐
   │ Stamp + return           │   stored in arena slot ↘
   │                          │   indexed into HNSW    ↘
   └──────────────────────────┘   cached in dispatcher
```

The pipeline is **five stages**: tokenise, forward, pool,
normalise, validate. Each runs synchronously on the same thread
that handled the request — there is no inference queue, no batch
window, no asynchrony inside the embedder itself. The cache wraps
all five and short-circuits when it can.

---

## The model: BGE-small-en-v1.5

The default model is
[BGE-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5):

- 384-dim output
- BERT-style architecture (12 encoder layers, ~33 M parameters)
- WordPiece tokenisation, 512-token max
- ~130 MB weights file (`model.safetensors`)
- ~30 MB tokeniser (`tokenizer.json`)

We use it through HuggingFace's
[`candle_transformers::models::bert`](https://github.com/huggingface/candle).
candle is a Rust-native ML framework, no PyTorch C dep. v1 runs
**FP32 on CPU**; FP16 and GPU paths are deferred
(`crates/brain-embed/src/config.rs:18`).

Why BGE-small specifically? Three properties that fit Brain:

- Dimensions are 384 — small enough that the arena slot is
  cheap, big enough that recall@10 is strong on standard
  benchmarks.
- The model outputs the `[CLS]` token's hidden state directly,
  with no pooler MLP. Brain reads `last_hidden_state[:, 0]` and
  is done.
- The tokeniser ships as a single `tokenizer.json` file (no
  Python machinery to bootstrap), which a Rust crate
  ([`tokenizers`](https://github.com/huggingface/tokenizers))
  can load and call directly.

An operator can swap to a different model by changing
`model_path`. Anything that produces a 384-dim L2-normalised
vector via `BertModel::forward` will work; anything else (a
non-BERT model, a different output dim) needs code changes — the
forward path is type-pinned to `[f32; VECTOR_DIM]` where
`VECTOR_DIM = 384`
(`crates/brain-embed/src/forward.rs:30`).

---

## Loading the model

`ModelHandle::load`
(`crates/brain-embed/src/model.rs:60`) runs at shard startup. The
sequence:

1. **Validate device + dtype** before doing I/O. Anything other
   than `Device::Cpu` + `DType::F32` fails with
   `UnsupportedDevice`
   (`crates/brain-embed/src/model.rs:62`).
2. **Verify `model_path` is a directory.** Failure is
   `ModelPathInvalid`.
3. **Read `config.json`** into raw bytes (kept for fingerprinting)
   and parse it into a `BertConfig`.
4. **Read `tokenizer.json`** raw bytes (also kept) and load it
   into a `tokenizers::Tokenizer`.
5. **Refuse pickle outright.** If `model.safetensors` is missing
   but `pytorch_model.bin` is present, log a warning and fail
   (`crates/brain-embed/src/model.rs:96`). Pickle is unsafe — it
   can execute arbitrary code on deserialise. We don't allow
   that surface to exist.
6. **Stream-hash the weights file** for the fingerprint
   (`blake3_hash_file`, 64 KiB chunks). Avoids loading 130 MB
   into memory just to hash it.
7. **Compute the fingerprint** (next section).
8. **Build the BERT model** via candle's `VarBuilder` over the
   safetensors.
9. **Warm-up inferences.** Default 3
   (`crates/brain-embed/src/config.rs:49`). The first inference
   after load triggers candle's lazy ops; warming up cuts cold
   startup latency for the first real request.

Total cold-load on commodity hardware: ~1–2 seconds for the
weights file plus ~50 ms for the warm-up loop. The shard logs
the fingerprint on success
(`crates/brain-embed/src/model.rs:138`).

The `ModelHandle` is `Send + Sync`
(`crates/brain-embed/src/dispatcher.rs:105`) — multiple shards
share one loaded model via `Arc<ModelHandle>`, with the same
in-memory weights tensor. Loading the 130 MB once and sharing it
across N shards saves N × 130 MB.

---

## The fingerprint

Every memory stores the fingerprint of the model that produced
its vector. Vectors that don't match the current model are
"stale" — flagged for re-embedding by the migration worker.

`compute_fingerprint`
(`crates/brain-embed/src/fingerprint.rs:44`) is exact:

```
BLAKE3(
    b"config.json:"  + config_bytes
  + b"tokenizer.json:" + tokenizer_bytes
  + b"weights:"     + BLAKE3(weights_file)        // 32 raw bytes
  + b"vector_dim:"  + vector_dim.to_le_bytes()    // u32 LE
  + b"normalize:"   + [normalize as u8]
)[..16]
```

Truncated to 16 bytes. Why 16? Two reasons. First, the storage
cost: every memory carries this fingerprint, and at 32 bytes
versus 16 the doubling is meaningful at 10 M memories per shard.
Second, the collision space: BLAKE3-truncated-16 still has 2^64
distinct values, and *we control both sides* of the equality
(the substrate writes and reads it). A literal collision would
need an attacker producing a *different model* whose
fingerprint collides — which means controlling the config,
tokeniser, *and* weights files. Out of threat model.

The fingerprint is the *only* thing that survives if any of
those four inputs change. A retraining of the same architecture
gets a new fingerprint. A change in the normalisation toggle
(future GPU path, perhaps) gets a new fingerprint. The
substrate's invariant: same fingerprint ⇒ same embedding space.

The text-cache key is a parallel construction:
`BLAKE3(text)[..16]`
(`crates/brain-embed/src/fingerprint.rs:73`). Same collision
math — 2^64 distinct values, attacker-controlled collisions are
infeasible — but the impact is smaller (a wrong cache lookup
returns the wrong vector, not silent corruption).

---

## Stage 1: tokenisation

`tokenize::encode_batch`
(`crates/brain-embed/src/tokenize.rs:71`) takes one or more
texts and returns a `Tokenized`
(`crates/brain-embed/src/tokenize.rs:39`):

```rust
pub struct Tokenized {
    pub input_ids: Tensor,         // (batch, seq) u32
    pub token_type_ids: Tensor,    // (batch, seq) u32 — all zeros for single-seq
    pub attention_mask: Tensor,    // (batch, seq) u32 — 1 real, 0 pad
    pub actual_lengths: Vec<usize>,
    pub truncated_flags: Vec<bool>,
}
```

The pipeline is:

- WordPiece encode without truncation or padding (the shared
  tokeniser is **immutable** — never mutated at encode time).
- Manual right-truncate at `MAX_TOKEN_LENGTH = 512`
  (`crates/brain-embed/src/tokenize.rs:27`).
- Manual left-pad with `[PAD]` to the longest non-truncated row
  in the batch.
- Build the attention mask: `1` at real-token positions, `0` at
  pad positions.
- Tensorise as `DType::U32` on the configured device.

Three things worth knowing:

- **Truncation is detected.** A text that overflowed 512 tokens
  sets `truncated_flags[i] = true`. The handler can warn the
  operator that content was lost. This is the only "content
  loss" path in the pipeline.
- **The tokeniser is immutable at runtime.** We deliberately
  don't call `with_truncation` / `with_padding` to mutate it —
  truncation and padding happen by hand, here, so concurrent
  callers can share the same `Tokenizer` without locks
  (`crates/brain-embed/src/tokenize.rs:13`).
- **The attention mask matters.** Without it, the BertModel
  forward attends to `[PAD]` positions, contaminating `[CLS]`
  outputs for short rows in a mixed-length batch
  (`crates/brain-embed/src/forward.rs:64`). A bug we'd rather
  catch at design time than in production.

---

## Stages 2–4: forward, pool, normalise

`forward_pooled`
(`crates/brain-embed/src/forward.rs:60`) is the heart of the
chapter:

```rust
pub fn forward_pooled(
    handle: &ModelHandle,
    tokens: &Tokenized,
) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
    // 1. forward pass
    let hidden = handle.forward(
        &tokens.input_ids,
        &tokens.token_type_ids,
        Some(&tokens.attention_mask),
    )?;

    // 2. shape check
    let dims = hidden.dims();
    // dims == [batch, seq, 384]

    // 3. [CLS] extraction: narrow seq → 0..1, squeeze
    let cls = extract_cls(&hidden)?;

    // 4. pull to host as Vec<Vec<f32>>
    let rows: Vec<Vec<f32>> = cls.to_vec2::<f32>()?;

    // 5. validate + normalise each row
    for (row_idx, row) in rows.into_iter().enumerate() {
        let mut arr: [f32; 384] = ...;
        // NaN/Inf check
        // L2 normalise
        // zero-norm check
    }
    Ok(out)
}
```

### `[CLS]` pooling, not mean pooling

BGE-small uses `last_hidden_state[:, 0]` directly as the
embedding. There is **no pooler MLP**.
`candle_transformers::BertModel` returns hidden states without
running `BertPooler`, so taking seq position 0 is the right move
(`crates/brain-embed/src/forward.rs:18`).

This matters because the comment "mean-pool the hidden states"
appears in a lot of generic BERT tutorials. BGE is different.
Mean-pool would produce a *legal* 384-dim vector but it wouldn't
match the embedding space the model was trained against, which
silently degrades recall by ~10–15 % on standard benchmarks.

### L2 normalise, in place, scalar

`l2_normalize_in_place`
(`crates/brain-embed/src/forward.rs:42`):

```rust
pub fn l2_normalize_in_place(v: &mut [f32; VECTOR_DIM]) -> f32 {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();
    if norm < ZERO_NORM_EPS { return norm; }
    let inv = 1.0 / norm;
    for x in v.iter_mut() { *x *= inv; }
    norm
}
```

Scalar, no SIMD. The forward pass dominates by ~3 orders of
magnitude, so optimising the normalise step is premature. The
function returns the *pre-normalisation* norm so the caller can
reject near-zero vectors.

### Validation: NaN, Inf, zero-norm

Three checks
(`crates/brain-embed/src/forward.rs:118`):

- **NaN or Inf anywhere in the row** → `NumericFailure` with
  the offending position. A NaN here is a model or input
  pathology, not an expected outcome.
- **`norm < ZERO_NORM_EPS (= 1e-8)`** → `NumericFailure`. A
  truly zero vector is undefined as a unit vector and would
  blow up downstream (cosine similarity divides by norm).
- **Output dim mismatch** → `OutputDimMismatch`. Defensive — the
  forward pass should never return the wrong shape, but if the
  operator points `model_path` at the wrong model, this is
  where we catch it.

All three failures abort the embed call. There is no "best
effort" fallback. We'd rather refuse than store a garbage vector.

---

## The dispatcher: how callers see it

Inside the shard, handlers don't call `forward_pooled` directly.
They go through the `Dispatcher` trait
(`crates/brain-embed/src/dispatcher.rs:38`):

```rust
pub trait Dispatcher: Send + Sync {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>;
    fn fingerprint(&self) -> [u8; 16];
}
```

Three properties:

- **`Send + Sync`.** Multiple shards' Glommio executors all call
  the same `Arc<dyn Dispatcher>` concurrently. Inside the
  shard, the dispatcher's methods are sync (no `.await`) —
  candle inference is CPU-bound and synchronous.
- **Object-safe.** Handlers hold `Arc<dyn Dispatcher>`, so a
  mock dispatcher can be slotted in for tests without loading
  130 MB of weights per test
  (`crates/brain-embed/src/dispatcher.rs:118`).
- **Both single and batch.** Single-text `embed` is the
  request-path entry point. `embed_batch` is for callers that
  already have a batch (e.g. the bulk reembedding worker).

The production implementation is `CpuDispatcher`
(`crates/brain-embed/src/dispatcher.rs:54`) — pure pass-through
to `embed_text` / `embed_batch`. **No time-window batching, no
inference queue.** Each call runs on the calling thread; the
shared `Arc<ModelHandle>` means concurrent calls run concurrently
without contention on the weights tensor.

### No time-window batching, why?

You might expect a "wait 5 ms, gather a batch, run once" pattern
— it's standard in inference servers. We deliberately don't.

The reasoning: on CPU, the marginal cost of batching is positive
(matmul does amortise across rows), but the marginal *latency*
cost of waiting is also positive. Inference takes ~5–10 ms; a
5 ms window adds 50 % to that. At low traffic the window adds
latency for no benefit (no batch forms anyway); at high traffic
the queue contends on its own mutex. We chose to accept the
~5–10 ms-per-call serial cost in v1.

GPU is a different conversation — there, batching is the entire
performance story, and v1's design has a slot for `GpuDispatcher`
behind the same trait without re-spelling anything
(`crates/brain-embed/src/dispatcher.rs:14`).

---

## The cache: `CachingDispatcher`

The thing that makes Brain feel fast.

`CachingDispatcher` wraps any inner `Dispatcher` with an LRU
cache keyed on `BLAKE3(text)[..16]`
(`crates/brain-embed/src/cache.rs:74`):

```rust
pub struct CachingDispatcher<D: Dispatcher> {
    inner: D,
    state: Option<Arc<Mutex<LruCache<[u8; 16], CachedEmbedding>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

struct CachedEmbedding {
    vector: [f32; 384],
    fingerprint: [u8; 16],
    inserted_at: Instant,
}
```

The key fields:

- **`inner: D`.** Generic, not `Box<dyn Dispatcher>`. Inlining
  the miss path through the trait is a measurable win.
- **`state: Option<...>`.** A capacity of `0` *disables* the
  cache — `state = None` and every call is a pure passthrough
  (`crates/brain-embed/src/cache.rs:140`). Used in
  inference-test fixtures and substrate-only deployments where
  cache memory is precious.
- **Counters are `AtomicU64`.** Reads don't take the cache
  mutex. Operators can scrape `CacheStats`
  (`crates/brain-embed/src/cache.rs:50`) at any rate without
  contending the embed path.

### The hit/miss path

```
embed(text):
  if cache disabled: return inner.embed(text)
  key = BLAKE3(text)[..16]
  current_fp = inner.fingerprint()

  # hit path (no lock promotion yet)
  if entry = state.peek(key):
    if entry.fingerprint == current_fp:
      state.promote(key)     # bump LRU
      hits++
      return entry.vector
    # fingerprint mismatch — treat as miss; entry will age out

  # miss path
  vector = inner.embed(text)
  state.put(key, CachedEmbedding { vector, fingerprint: current_fp, inserted_at: now })
  misses++
  return vector
```

Three behaviours worth knowing:

- **Fingerprint mismatch = miss.** A model upgrade orphans every
  cached entry. The lookup *peeks* the entry, compares the
  fingerprint, and falls through to the miss path on mismatch.
  Stale entries are *not* auto-removed — they age out via LRU
  (`crates/brain-embed/src/cache.rs:8`). Forcing a sweep on
  every fingerprint change would be O(N); LRU eviction is
  amortised constant.
- **`peek` then promote.** A hit doesn't bump LRU position
  speculatively — we only promote on confirmed-hit (fingerprint
  match). Stale entries don't get artificial life.
- **`embed_batch` is a passthrough.** The cache layer doesn't
  try to be clever about per-row hits inside a batch — the
  batch APIs are for bulk paths (re-embed, recall_many) where
  rows aren't independent client cues. A cache-aware batch
  implementation is a contained follow-up if it ever matters
  (`crates/brain-embed/src/cache.rs:19`).

### Default size

`DEFAULT_CACHE_SIZE = 10_000`
(`crates/brain-embed/src/cache.rs:36`). At 16-byte key + ~1.6 KB
value (the vector plus housekeeping), the cache costs ~16 MB per
shard at full size. Operators tune `embedder.cache_size` in TOML.

### What hit rate looks like in practice

For an agent issuing varied cues (each `RECALL` is a different
question), hit rate is low — single-digit percent. For an agent
that repeats the same cues across sessions (a "what did I do
yesterday" pattern), hit rate climbs into double digits. For a
workload running the same prompt template, hit rate is in the
80s. The cache stats live behind the admin endpoint; operators
should watch them and tune the size accordingly.

---

## Latency math

What an `ENCODE` actually spends time on, with cache:

| Stage | Time (cache miss) | Time (cache hit) |
|---|---|---|
| Frame decode | ~1 µs | ~1 µs |
| Shard dispatch (flume) | ~5 µs | ~5 µs |
| Cache lookup | ~1 µs | ~1 µs |
| Tokenise | ~50 µs | — |
| BertModel forward (CPU) | 5–10 ms | — |
| `[CLS]` extract | ~10 µs | — |
| L2 normalise | ~1 µs | — |
| Validate (NaN/Inf/norm) | ~1 µs | — |
| Cache put | ~1 µs | — |
| WAL append + fdatasync | 0.1–0.3 ms | 0.1–0.3 ms |
| Arena write + metadata commit | ~0.5 ms | ~0.5 ms |
| HNSW insert | 1–3 ms | 1–3 ms |
| **Total ENCODE** | **~7–14 ms** | **~2–4 ms** |

The forward pass dominates on a cache miss. On a hit, the WAL
and HNSW insert dominate. **A 50 % cache-hit rate halves
ENCODE latency.**

For `RECALL` (no WAL, no HNSW insert), the cache hit makes the
operation sub-millisecond; a miss is dominated by the same 5–10 ms
inference cost as ENCODE.

---

## Failure modes

**Model directory not found.** Shard startup fails with
`ModelPathInvalid`. Operator misconfigured `model_path`.

**`config.json` or `tokenizer.json` unreadable.** `ConfigRead`
or `TokenizerRead` errors. Refuse to start the shard.

**`model.safetensors` missing.** If `pytorch_model.bin` is the
only weights file, the shard logs the pickle file's presence and
refuses to start. Operator must convert to safetensors out of
band.

**Weights load fails.** Some incompatibility between candle's
expected layer names and the safetensors layout. `WeightsLoad`
with the candle error message. Refuse to start.

**Warm-up inference fails.** Usually means the model and the
config disagree about a shape. `WarmupFailed`. Refuse to start.

**Encode-time NaN/Inf.** Indicates input pathology (e.g. text
with garbage that triggers a numerical issue) or a corrupted
weights file. The single request fails with `NumericFailure`;
the connection survives. If it recurs, the operator should
investigate.

**Zero-norm output.** Same path as NaN. Empty-after-tokenise
text (only `[CLS]` and `[SEP]`, no body) is the textbook cause;
the dispatcher rejects it before storage.

**Truncated input.** Not a failure. The text was longer than
512 tokens; the handler can warn but the embed succeeds against
the truncated prefix.

**Cache disagreement after model swap.** A live cache survives
the swap (it's RAM) but every entry's fingerprint is now stale.
Subsequent lookups all miss, the cache repopulates with new
fingerprints, the old entries age out. No correctness issue; a
brief throughput dip while the cache warms.

**OOM during weights load.** ~130 MB allocation fails. Refuse
to start. v1 doesn't shard the weights across processes; one
process per `data_dir` carries one full model copy in RAM (the
single `Arc<ModelHandle>` is shared across that process's
shards).

---

## Configuration & tuning

| Field | Default | Notes |
|---|---|---|
| `embedder.model` | `bge-small-en-v1.5` | Human label; not used by the loader. |
| `embedder.model_path` | (deployment-set) | Directory containing `config.json`, `tokenizer.json`, `model.safetensors`. |
| `embedder.cache_size` | 10 000 | `0` disables the cache. |
| `embedder.warmup_iters` | 3 | Set in code (`EmbedderConfig::new`). |
| `embedder.batch_size` | (unused on CPU) | Reserved for the GPU path. |
| `embedder.batch_window_ms` | (unused on CPU) | Reserved for the GPU path. |

Operational rules:

- **One model per deployment.** A cluster with two different
  models in production has two different embedding spaces, and
  cross-shard recall is meaningless. Validate fingerprints
  across shards if you suspect drift.
- **Don't downgrade models.** A downgrade orphans every cached
  vector (fingerprint mismatch) *and* every stored vector
  (slow re-embed via the migration worker). Plan upgrades
  carefully.
- **Cache size scales with working-set diversity, not memory.**
  If the agent's cue space is naturally small (a recommender
  with a few thousand fixed queries), keep the cache small. If
  it's effectively unlimited (open-ended chat), oversize the
  cache or disable it — large but unused caches eat RAM with
  no benefit.
- **Watch hit rate.** Sub-1 % is "you should probably disable
  the cache." 50 %+ is doing real work.
- **Truncation flag is a metric.** A workload with many
  `truncated_flags[i] = true` is silently losing content. Pre-
  segment long inputs at the client, or accept the loss.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Crate root, exports | `crates/brain-embed/src/lib.rs` |
| Config (`EmbedderConfig`) | `crates/brain-embed/src/config.rs` |
| `ModelHandle::load`, BertModel wiring | `crates/brain-embed/src/model.rs` |
| Fingerprint algorithm | `crates/brain-embed/src/fingerprint.rs` |
| Tokenise | `crates/brain-embed/src/tokenize.rs` |
| Forward + `[CLS]` pool + L2 normalise | `crates/brain-embed/src/forward.rs` |
| Dispatcher trait + `CpuDispatcher` | `crates/brain-embed/src/dispatcher.rs` |
| LRU `CachingDispatcher` + stats | `crates/brain-embed/src/cache.rs` |
| Error taxonomy | `crates/brain-embed/src/error.rs` |

---

## Further reading

- [03 — Arena and WAL](03-arena-and-wal.md) for what a stored
  vector looks like in the arena's slot and how the
  fingerprint is carried alongside it.
- [04 — HNSW index](04-hnsw-index.md) for what the vector gets
  inserted into.
- [07 — Background workers](07-background-workers.md) for the
  migration worker that re-embeds stale vectors after a model
  upgrade.
- [10 — Extractors](10-extractors.md) for how the knowledge
  layer's LLM tier (which is *also* an embedding-shaped call,
  but to a separate model) interacts with this pipeline.
