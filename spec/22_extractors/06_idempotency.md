# 22.06 Idempotency

Every extractor MUST be idempotent on its declared inputs. Replays
of the same `(memory_id, text_hash, extractor_id, extractor_version,
schema_version)` tuple produce byte-identical outputs (or the same
deterministic skip).

Cross-references:
- [`./05_audit.md`](./05_audit.md) §8 — idempotency probe.
- [`./01_pattern_extractor.md`](./01_pattern_extractor.md) §6 —
  per-tier determinism.
- [`./02_classifier_extractor.md`](./02_classifier_extractor.md) §2
  — classifier determinism contract.

## 1. The idempotency tuple

```rust
pub struct IdempotencyKey {
    pub memory_id: MemoryId,
    pub text_hash: [u8; 32],          // BLAKE3 of memory.text.
    pub extractor_id: ExtractorId,
    pub extractor_version: u32,
    pub schema_version: u32,
}
```

`text_hash` is included alongside `memory_id` so that an admin
edit-in-place of memory text invalidates the cache; v1 doesn't
support in-place memory edits but the field is wired for forward-
compat.

## 2. Determinism contract by tier

| Tier | Determinism source |
|---|---|
| Pattern | Pinned `regex` crate version + pattern source order. |
| Classifier | Pinned model weights + tokeniser + seed + math library. |
| LLM (phase 21) | Cache lookup; first uncached call may drift across binary versions; cache hit is bit-identical. |

When the source changes, `extractor_version` bumps. Old audit rows
remain queryable; the next ENCODE produces new outputs and a new
audit row.

## 3. Replay semantics

```rust
pub fn run_extractor(
    ctx: &OpsContext,
    memory: &Memory,
    extractor: &dyn Extractor,
    options: ExtractorRunOptions,
) -> Result<ExtractionResult, ExtractorError>;

pub struct ExtractorRunOptions {
    /// Force re-execution even if the idempotency probe hits.
    pub replay: bool,
    /// Default: false.
    pub include_cached_outputs: bool,
}
```

Normal ENCODE path: `replay = false`. Idempotency probe fires; if
it hits, the cached outputs are returned (with `include_cached_outputs
= true` semantics needed by RECALL) and a `SkippedDuplicate` audit
row is written. If it misses, the extractor runs.

Admin "re-extract" path (phase 22+): `replay = true`. The extractor
always runs; new outputs supersede / diff against the cached ones
per §25/00 §"Re-extraction workflow".

## 4. Output identity

For idempotency to hold, **output IDs must be deterministic** given
the tuple in §1. The current allocation rules:

| Output kind | ID source | Deterministic? |
|---|---|---|
| `EntityMention` | None (transient; not persisted) | n/a |
| Entity (auto-create) | UUIDv7 | NO — re-run creates a new ID. v1 reads the cached audit row's `outputs` to avoid this. |
| Statement (create) | UUIDv7 | NO — same. |
| Relation (create) | UUIDv7 | NO — same. |

So the practical idempotency invariant becomes:
1. First run writes outputs with fresh UUIDv7 IDs.
2. Subsequent runs (probe hits) return the **first run's IDs**
   verbatim from the audit row's `outputs: Vec<OutputRefRow>`.
3. The extractor logic itself is deterministic, but the substrate
   doesn't re-run it on cache hit — so non-determinism in ID
   allocation doesn't leak.

Phase 22+ may introduce content-addressed IDs for some kinds
(tracked in §22/07).

## 5. Cache invalidation

The audit row's `extractor_version` and `schema_version` are part
of the probe key. Two scenarios:

1. **Extractor changed** (config or model bump). Schema upload
   bumps `extractor.version`. Probe misses for the new version;
   the extractor runs; new outputs are written. Old outputs
   remain but `stale_extraction_detection` (§25/00) flags them.

2. **Schema changed but extractor didn't.** Schema upload bumps
   the namespace version. If the extractor's config bytes are
   identical, its `extractor_version` stays — probe hits the
   prior `schema_version` row. The audit row's `schema_version`
   reflects when the row was written, not the current namespace
   version.

The probe key prefers the **lowest matching schema_version** to
avoid unnecessary re-runs on cosmetic schema bumps.

## 6. Concurrency

Two ENCODEs of the same memory text on the same shard race only
through the single-writer-per-shard discipline (CLAUDE.md §5
invariant 2). One wtxn commits first; its outputs become the
cached probe target. The second wtxn's probe hits and skips. No
duplicate outputs.

Across shards: the same memory text on different shards is allowed
to produce distinct outputs — phase 20 doesn't deduplicate cross-
shard. Tracked in §22/07.

## 7. Tests

Phase 20 unit + integration tests:

- Run extractor twice over same memory → second call returns
  cached outputs + `SkippedDuplicate` audit.
- Bump `extractor_version` between runs → second call re-runs and
  writes `Success` audit + supersedes old outputs per §17/01.
- `replay = true` re-runs even on probe hit.
- Different memory text on the same `memory_id` (text edit
  scenario) → probe misses (text_hash differs).
