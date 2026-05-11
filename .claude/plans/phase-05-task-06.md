# Sub-task 5.6 — Determinism test

A standalone integration test that asserts the spec's determinism contract: *for a given input on the same machine, the model produces bit-identical output across repeated runs.* The cue cache (5.5) depends on this — if a re-embed of the same text returns a different vector, the fingerprint check still passes but the user sees inconsistency.

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/03 §12 | "For a given input, the model is deterministic — repeated inference produces the same vector. We rely on this for the cue cache to be useful." |
| §04/03 §12 (caveats) | CPU vs GPU may differ; AVX-512 vs AVX2 may differ. Within one machine + one ISA, bit-identical. |
| §04/07 §4 | Fingerprint of the model directory is stable across runs. |
| Phase doc 5.6 | 100 runs; bit-identical; document if pinning candle's matmul is needed. |

The test is *evidence* that 5.1–5.3's implementation honours the contract. If it fails, the cache (5.5) is unsafe and the fingerprint table (Phase 3.8) is meaningless.

## 1. Scope

**In scope for 5.6:**
- `tests/determinism.rs` — gated on `BRAIN_EMBED_MODEL_DIR`. Loads the real model, runs `embed_text` and `embed_batch` N times, asserts bit-identical output.
- Tests across multiple text shapes:
  - Single short text (3 words).
  - Single long text near `MAX_TOKEN_LENGTH` (forces full 512-token forward).
  - Batch of mixed-length texts.
  - Multiple loads of the model (re-init + re-embed must still match).
- A doctest or top-of-file comment that captures the spec's "bit-identical *within* an ISA" caveat so a future reader on a different machine doesn't get confused by a failure.
- Pin number of runs at **100** per phase doc; the test should run in < 30 s on the dev machine.

**NOT in scope:**
- Cross-machine determinism: out of scope per spec §12.
- Cross-ISA: out of scope per spec §12.
- Pinning candle's matmul: only document if the test actually surfaces non-determinism. Most CPU inference is naturally deterministic at the bit level when run sequentially on the same machine.
- Mocking — the determinism property is about the *real* model, not a placeholder.
- Public APIs: 5.6 ships only the test file.

## 2. Test design

```rust
// crates/brain-embed/tests/determinism.rs

const RUNS: usize = 100;
const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";

#[test]
fn embed_text_is_bit_identical_across_100_runs() {
    let Some(handle) = load_or_skip() else { return };
    let text = "the quick brown fox jumps over the lazy dog";
    let baseline = embed_text(&handle, text).expect("baseline");
    for run in 1..RUNS {
        let v = embed_text(&handle, text).expect("run");
        assert_bytewise_eq(&baseline, &v, run);
    }
}
```

Each test takes a fresh `ModelHandle::load`'d model or shares one — we share to keep the test under ~30 s, and add **one** test that re-loads the model and verifies the post-reload embedding still matches.

### 2.1 Bitwise-equal comparator

```rust
fn assert_bytewise_eq(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM], run_idx: usize) {
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(
            x.to_bits(), y.to_bits(),
            "run {run_idx}, dim {i}: baseline {x} = {:#010x}, got {y} = {:#010x}",
            x.to_bits(), y.to_bits(),
        );
    }
}
```

We compare `f32::to_bits()`, not `==`. NaN ≠ NaN under `==`, but NaN's bit pattern equals itself — and we want a NaN-vs-NaN equality check to *succeed* (the spec contract is "same bits", not "same value"). Practical impact: we never expect NaN here (5.3's `NumericFailure` would have rejected the embed), but the comparator is robust to it.

### 2.2 Test list

1. `embed_text_is_bit_identical_across_100_runs` — short input, hot model.
2. `embed_text_long_input_is_bit_identical_across_100_runs` — text padded to ~500 tokens; exercises the full forward pass.
3. `embed_batch_is_bit_identical_across_100_runs` — 4-row batch with varied lengths; asserts each row stable.
4. `embed_is_bit_identical_across_model_reloads` — load model twice, embed same text, vectors match bit-for-bit. (Validates `ModelHandle::load` itself is reproducible.)
5. `embed_concurrent_threads_bit_identical` — 8 std::threads, all return the same bits as the serial baseline. This overlaps with 5.4's concurrency test but uses the *bitwise* (not cosine) comparator; it's the stricter assertion.

5 tests, all gated on the env var, each one logged + skipped when the var is missing.

### 2.3 Why not run determinism on a tiny mock model

Determinism is a *real-model* property. Mocks return deterministic-by-construction fixed vectors — they can't fail this test, so they wouldn't catch a real-model regression. Spec §04/03 §12 says "the model is deterministic" — we're testing that the model + our load path + candle on CPU honours it.

### 2.4 Behaviour when the contract fails

If a run returns different bits, the panic message identifies the run index and the dimension where they differ — useful for debugging.

If this surfaces (e.g. candle 0.9 adds non-deterministic matmul), we have two paths:
- **(A) Document and tighten:** if a fix exists (env var like `CANDLE_DETERMINISTIC=1`), apply it in `ModelHandle::load` and re-test.
- **(B) Spec deviation entry:** if no fix, log `SD-5.6-1` capturing what diverges and the cache implication. Treat it as a *known issue* that may surface as user-visible noise (per spec §12 itself, "inconsistency between cached and freshly-computed vectors is accepted as noise").

Path (A) is preferred; the SD entry is only filled if we can't make (A) work.

## 3. Implementation details

### 3.1 Test runtime budget

On the reference dev machine, BGE-small CPU inference is 5–10 ms per text. 100 × 10 ms = 1 s for the short test; 100 × 20 ms = 2 s for the long test; batch of 4 × 100 = 4 s for the batch test; reload test is one extra load + one embed = ~1.5 s. Total < 30 s, fine.

### 3.2 Shared `ModelHandle` across tests

Cargo runs `#[test]` functions in parallel by default. Loading BGE-small four times in parallel = ~4 × 130 MiB ephemeral memory and ~4 × 1.5 s startup. Two options:

- **(A) `--test-threads=1`** in `Cargo.toml`'s `[lib]` or `[[test]]` profile — but cargo doesn't natively support per-target thread caps.
- **(B) `OnceLock<ModelHandle>`** at module scope. First test triggers the load; subsequent tests reuse it.

**Choice: (B).** Cleaner; doesn't require global config. A `OnceLock<Result<ModelHandle, ...>>` is the right shape because load can fail; if it does, every test logs the same error.

```rust
use std::sync::OnceLock;
static HANDLE: OnceLock<Option<ModelHandle>> = OnceLock::new();
fn shared_handle() -> Option<&'static ModelHandle> {
    HANDLE.get_or_init(load_optional).as_ref()
}
```

The reload test deliberately bypasses the `OnceLock` — it needs *two* independent loads.

### 3.3 `embed_concurrent_threads_bit_identical` — what makes it stricter than 5.4's

5.4's `cpu_dispatcher_concurrent_calls_match_serial` asserts cosine ≥ 1 − 1e-6 (high but not bitwise). 5.6's variant asserts `to_bits()` equality. They're different assertions: cosine ~1 doesn't prove bitwise equal; bitwise equal trivially implies cosine = 1.

For non-bitwise concurrent reads (which could arise from candle using parallel reductions internally), 5.4's test passes but 5.6's would fail. That's the regression we want to catch.

If 5.6's concurrent test surfaces a divergence that 5.4's didn't, we know candle is parallelising internally and producing slightly different rounding. That's an actionable signal.

### 3.4 No new error variants or public APIs

Test file only. `EmbedError` unchanged. `lib.rs` unchanged.

## 4. Files written / changed

```
crates/brain-embed/tests/determinism.rs        [new — gated on BRAIN_EMBED_MODEL_DIR]
```

No source changes. No `Cargo.toml` change. No new deps.

## 5. Verify checklist

- `cargo build -p brain-embed --tests` clean.
- `cargo test -p brain-embed` — 48 existing + 5 new gated tests. Without env var: tests skip (cargo counts them as passes). With env var on the dev machine: all bit-identical.
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-embed` no diff.

## 6. Commit message (draft)

```
test(brain-embed): determinism property test for the forward path (sub-task 5.6)

Spec §04/03 §12 says: "for a given input, the model is deterministic
— repeated inference produces the same vector. We rely on this for
the cue cache to be useful." 5.6 ships the evidence.

tests/determinism.rs (gated on BRAIN_EMBED_MODEL_DIR):
- 100 runs of embed_text("…") must return bit-identical bytes (via
  f32::to_bits()) — the strictest contract spec §12 mandates within
  one ISA.
- Long-input variant (~500 tokens) exercises the full forward pass.
- 4-row batch variant; each row must be bit-stable across 100 runs.
- ModelHandle::load is itself reproducible: load twice, embed the
  same text, bytes match.
- 8 std::threads — bitwise equal to serial baseline. Stricter than
  5.4's cosine ≥ 1 - 1e-6 assertion; would catch candle parallel-
  reduction non-determinism if it surfaces.

Uses OnceLock<Option<ModelHandle>> to amortise the ~1.5 s model
load across tests; reload test deliberately bypasses for an
independent second load.

No source changes. No new deps.

Verify: cargo build/test/clippy -p brain-embed.
```

## 7. Risks

- **Long test runtime under the env var.** Budgeted < 30 s on dev hardware; if CI is slower, drop `RUNS = 100` to `RUNS = 25` (still strict enough to surface non-determinism — once is enough). Decision happens at implementation if runtime is a concern.
- **candle internal parallelism.** If candle's matmul uses parallel reductions (it does on some BLAS backends), `embed_concurrent_threads_bit_identical` may fail. Mitigation: spec §12 already documents this as acceptable; we log SD-5.6-1 and tighten the comparator to cosine-equality if needed.
- **OnceLock load-failure caching.** If the first test loads with a corrupt env var (e.g. path moved), every subsequent test sees the cached failure. Acceptable — tests run in one cargo invocation and the operator fixes the env var.

## 8. Out-of-scope flags

- No cross-ISA testing. Spec §12 explicitly excludes.
- No SIMD-vs-scalar comparator. Spec §12 covers this case (different ISAs may differ).
- No pinning candle features yet. If 5.6 fails, we add it. Otherwise it stays as-is.

---

PLAN READY.
