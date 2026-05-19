# 25 — Determinism, idempotency, and replay

Three related concepts that together let clients retry
operations safely, let extractors produce reproducible
outputs, and let crash recovery reconstruct durable state
exactly:

- **Determinism**: same inputs → same outputs, always.
- **Idempotency**: doing an operation twice has the same
  effect as doing it once.
- **Replay**: re-running a previous operation gets the
  same result.

This chapter is the conceptual glue tying together the
WAL durability story (chapter 18), the extractor tiers
(chapter 14), and the substrate's request-handling
discipline.

---

## Determinism: the property

A function is **deterministic** if the same inputs
always produce the same outputs. Trivially true for pure
math (`add(2, 3) = 5` always). Less trivially true for
ML extractors, where small changes in floating-point
ordering or model weights can produce different outputs.

Brain cares about determinism for one reason: **replay
safety**. If the substrate has to re-run an operation —
for crash recovery, for a backfill, for retry — the
re-run had better produce the same result as the first
run. Otherwise the state diverges silently.

The three extractor tiers (chapter 14) each have
different determinism properties:

| Tier | Deterministic? | How |
|---|---|---|
| Pattern | Yes, by construction | Pure regex; no state. |
| Classifier | Yes, with discipline | Pinned weights, pinned tokenizer, greedy decode (no sampling). |
| LLM | No, but cached → effectively yes | Cache layer makes repeated inputs return the cached output. |

The discipline matters. A classifier extractor isn't
*automatically* deterministic — if the model is loaded
with different weights, or if decoding uses sampling,
or if the tokenizer changes, output differs.
Brain's runtime enforces:

- **Pinned weights file.** Same `.safetensors` every
  time.
- **Pinned tokenizer.** Same `tokenizer.json`.
- **Greedy decoding.** No sampling, no temperature in
  the classifier's inference loop.
- **The model fingerprint** (chapter 08) makes drift
  detectable: if the weights change, the fingerprint
  changes, and the substrate flags affected outputs as
  stale.

For LLM extractors, the underlying model's output is
non-deterministic — the same prompt to Claude or GPT-4o
can produce different completions across calls. Brain
mitigates by:

- Setting `temperature = 0` (the closest the API offers
  to greedy decoding).
- **Caching** the input+model+prompt+schema tuple to a
  per-shard redb file. Subsequent calls hit the cache
  and return the cached bytes, bit-identical.

The cache is what gives the LLM tier its *effective*
determinism. Without the cache, retrying an LLM
extraction could produce different statements; with the
cache, retry is byte-for-byte the same as the first call.

---

## Idempotency: the API guarantee

An operation is **idempotent** if doing it twice has the
same effect as doing it once. The classic example:
setting a value (`x = 5`) is idempotent; appending a
value (`x.push(5)`) is not.

Brain's state-mutating operations — `encode`, `forget`,
`link`, `unlink`, `update_kind`, etc. — are idempotent
**by request_id**. The mechanism:

1. The client generates a unique `request_id` (a 16-byte
   UUID, typically version 7) for each *logical*
   operation. Same retry → same `request_id`.
2. The substrate looks up the `request_id` in the
   idempotency table.
3. **First call:** no entry. Execute the operation;
   store the response in the idempotency table under
   this `request_id`; return the response.
4. **Replay (same request_id, same parameters):** hit
   the cache. Return the stored response. Do *not*
   re-execute. The client gets the original answer.
5. **Conflict (same request_id, different parameters):**
   return `IdempotencyConflict`. This is a client bug.

The 24-hour TTL on idempotency entries (chapter 05 of
the architecture tier) means retries within a day are
safe. Beyond that, the entry expires and a "retry"
becomes a brand-new operation. For most client retry
loops (seconds to minutes), 24 hours is generous.

> **Why request_id, not memory_id?**
>
> Because the client doesn't know the `memory_id` *until
> the operation succeeds*. The `request_id` is what the
> client controls; it's how the client says "this is
> the same operation as before, please don't duplicate
> it." The substrate uses the `request_id` as the
> dedup key.

### What's safe to retry

Every state-mutating wire op:

- `encode(text)` — retry is safe; you get the same
  `memory_id` back.
- `forget(memory_id)` — retry is safe; the memory stays
  forgotten.
- `link(src, kind, tgt)` — retry is safe; the edge
  exists exactly once.
- `unlink(...)` — retry is safe.
- Schema operations — `schema_upload` is idempotent on
  the schema content hash; uploading the same schema
  twice doesn't double-version it.

What's *not* automatically idempotent:

- Reads (`recall`, `query`) — they're trivially safe to
  retry because they don't change state. They don't
  carry `request_id` semantics.

### Conflict detection

The substrate detects misuse:

```
encode(text = "A", request_id = R1)        # → mem_001
encode(text = "B", request_id = R1)        # → IdempotencyConflict
```

The second call carries the same `request_id` but
different text. The substrate's content hash on the
idempotency table catches this and returns a clear
error. Without this check, the client could accidentally
write `B` and think it wrote `A`.

This is one of the seven invariants (chapter 24): same
`request_id` → cached response; same `request_id` and
different params → error. Never silent corruption.

---

## Replay: when it happens

Brain "replays" operations in a few scenarios:

### 1. Crash recovery

After a crash, the substrate replays its WAL records.
Every replay step is *idempotent at the substrate level*:

- An `Encode` record applied twice produces the same
  memory state.
- A `Forget` record applied twice keeps the memory
  forgotten.
- A `Link` record applied twice produces the same edge.

The implementation: each WAL record is processed and
the resulting state is compared/updated against
metadata. Already-applied records (by LSN, the
log-sequence number) are skipped.

The WAL is the source of truth; replay reconstructs
state deterministically. Two crashes happening at the
same LSN produce the same post-recovery state.

### 2. Extractor replay

When a new extractor is added or an existing one bumps
its version (chapter 14), Brain has the option to
*replay* it over historical memories. The `backfill`
worker does this:

1. Iterate over memories whose extraction is stale
   (under the old extractor version).
2. Re-run the extractor on each.
3. Produce new statements / entities / relations under
   the new extractor version.
4. Old statements are flagged stale; new ones supersede
   or augment them.

This is replay in the "re-run with current rules"
sense. The extractor's idempotency-key (chapter 14)
ensures each memory gets one new extraction per version,
not multiple.

### 3. LLM cache replay

The LLM cache turns non-deterministic LLM calls into
deterministic replays. A second call with the same key
doesn't hit the network — it returns the cached bytes.

This means: extracting the same memory ten times in a
row costs one LLM call. The remaining nine are cache
hits. Cost-wise, this is a huge win; correctness-wise,
it means LLM-derived statements are *stable*.

A cache miss happens when:

- The key changes (different memory, different extractor
  version, different model).
- The entry expired (TTL elapsed; default 7 days).
- The cache file was rebuilt.

Each is a controlled invalidation; the substrate doesn't
randomly miss.

### 4. Client retry

The whole point of idempotency on the wire is to make
client retries safe. A typical client retry loop:

```
attempt 0:
    encode("hello", request_id = R1) → mem_001
attempt 1 (after network blip, didn't see response):
    encode("hello", request_id = R1) → mem_001
        ↑ replay; cached response from attempt 0
```

The client doesn't know whether attempt 0 succeeded.
Doesn't matter — it retries with the same `request_id`
and gets a clean answer. If attempt 0 *had* succeeded,
the second call is a replay; if it *hadn't*, the
second call is the first one to land.

Either way, the result is identical and the client
moves on.

---

## How the three concepts compose

The three properties stack:

```
client retries                  ← idempotency (request_id)
    │
    ▼
substrate handles request       ← determinism (extractors, indexes)
    │
    ▼
substrate writes to disk        ← replay (WAL + recovery)
```

A retried encode reuses the same `request_id`, gets the
same response (idempotent at the wire). The extractor
that runs on the memory produces the same output
(deterministic). If the substrate crashes mid-write, the
WAL replay produces the same final state (replay).

All three properties have to hold for the substrate to
be retry-safe end-to-end. Drop any one and you lose the
guarantee:

- No idempotency: retries create duplicates.
- No determinism: replays produce different results.
- No replay: crashes lose committed writes.

This is why all three are first-class concerns. The
substrate doesn't get to pick one.

---

## What's *not* idempotent

A few non-state-changing operations have their own
semantics that don't quite fit the idempotency model:

- **`recall`** — returns different results as the index
  grows. Two recalls of the same cue at different
  times legitimately return different rankings.
- **`subscribe`** — opens a stream. Each subscribe is a
  fresh stream; retrying doesn't deduplicate.
- **`txn_begin`** — starts a transaction. A second
  `txn_begin` with the same `request_id` returns the
  existing transaction (so retries are safe) but it's
  conceptually different from "I want a new
  transaction."

For each, the protocol's wire reference covers the
exact semantics.

The substrate's *state-mutating* operations all
participate in idempotency. The read-only verbs don't
need to.

---

## The 24-hour TTL

Idempotency entries live for 24 hours by default. After
that, the substrate's idempotency-cleanup worker prunes
them.

Why 24 hours and not "forever"?

- Storage. Idempotency entries are the largest growing
  table on a write-heavy shard. Without pruning, the
  table grows linearly with request count.
- Practicality. Most retries happen seconds to minutes
  after the first attempt. 24 hours is many orders of
  magnitude more than that.
- A bounded window simplifies reasoning. The substrate
  doesn't have to think about "what if a client retries
  a 6-month-old operation?"

Operators can tune the TTL; 24 hours is the default.
Production deployments rarely need more.

If a client somehow retries an operation 25 hours after
the original, it's a *new* operation from the substrate's
perspective. For `encode`, that means two memories get
created. The client should design retry windows to fit
inside the TTL.

---

## Operational implications

A few patterns that fall out:

- **Generate `request_id`s with UUIDv7.** They're
  time-ordered and ~16 bytes; perfect for this. Avoid
  random UUIDs that don't carry a timestamp.
- **Use the same `request_id` for legitimate retries.**
  Don't randomise on each attempt; that defeats the
  point.
- **Cache the response in the client where possible.**
  If you know the operation succeeded (you have the
  response), you don't need to retry; if you didn't see
  the response, retry with the same `request_id`.
- **Monitor `IdempotencyConflict`.** It's a client bug
  signal — same `request_id` reused for different
  operations. Worth alerting on.
- **Snapshot before bumping extractor versions.** The
  replay (backfill) might produce different outputs;
  snapshots let you compare or roll back.

---

## Recap

- **Determinism**: pattern + classifier extractors are
  deterministic by construction; LLM extractors are
  effectively deterministic through caching.
- **Idempotency**: state-mutating ops carry a
  `request_id`; retries with the same `request_id`
  return the cached response. Conflict detection
  catches client bugs.
- **Replay**: WAL recovery, extractor backfill, LLM
  cache hits, and client retries all rely on
  reproducible results.
- All three properties have to hold together for the
  substrate to be retry-safe.
- 24-hour idempotency window; tunable; designed for
  practical retry timescales.

---

## Where to go next

- **The invariants list:** [chapter 24](24-invariants-and-trust.md).
- **The WAL and recovery story:** [chapter 18](18-storage-and-durability.md).
- **Extractors and the cache:** [chapter 14](14-extractors.md).
- **The architecture-tier idempotency table:**
  [`../architecture/05-redb-metadata.md`](../architecture/05-redb-metadata.md).
