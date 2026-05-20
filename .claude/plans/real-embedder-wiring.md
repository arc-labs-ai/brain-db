# Plan: wire the real BGE-small embedder into brain-server

**Status:** draft — awaiting-confirmation
**Date:** 2026-05-20
**Author:** Claude (autonomous)
**Estimated commits:** 3

---

## 1. Scope

Replace the `NopDispatcher` stub in `crates/brain-server/src/shard/mod.rs:1008` with a real `CpuDispatcher` (optionally wrapped in `CachingDispatcher`) loaded from a BGE-small model at startup. Every shard receives a clone of the shared dispatcher; one `ModelHandle` load amortises across N shards. After this lands, every ENCODE produces a distinct 384-dim L2-normalised vector, RECALL semantic similarity becomes meaningful, AutoEdgeWorker's defensive guard (commit `54784ca`) becomes effectively unreachable in production, and `EncodeResponse.embedding_model_fp` carries a real 16-byte BLAKE3 fingerprint instead of zeros.

**This is the long-deferred "9.10 swaps in a real CpuDispatcher" item.** Phase 9.10 itself (the frame dispatcher) was completed; the embedder swap was tracked only by a stale TODO comment at shard/mod.rs:1007. This plan closes that gap.

**Out of scope:**
- GPU dispatcher (`Device::Cuda`, `Device::Metal`). The brain-embed config already has the slot but the load path rejects non-CPU. Phase 11+ work.
- Multi-model / per-agent model selection. v1 ships one model per deployment.
- Dynamic model swap at runtime. v1 reloads on restart.
- Auto-download of model files. Per spec §04/03 §9: operator downloads BGE-small out-of-band; the substrate refuses to load if the directory is absent. Stays out-of-band.
- Changing the embedding cache size / batch knobs from what `config.embedder` already exposes.

## 2. Spec references

- `spec/04_embedding_layer/01_model_choice.md` — BGE-small-en-v1.5 is the v1 model. 384 dim, L2-normalised, mean-pooled.
- `spec/04_embedding_layer/03_inference.md` §7 — "The substrate doesn't internally batch CPU inference. Each request goes through the model independently." Already honored by `CpuDispatcher`.
- `spec/04_embedding_layer/03_inference.md` §9 — six-step load sequence (config → tokenizer → safetensors weights → fingerprint → build → warm-up). Already implemented in `ModelHandle::load`.
- `spec/04_embedding_layer/07_fingerprinting.md` §3 — fingerprint algorithm. Already implemented in `brain_embed::fingerprint::compute_fingerprint`.
- `crates/brain-server/src/shard/mod.rs:821-836` — the `NopDispatcher` we replace. Comment block explicitly says "Stub dispatcher used until 9.10 wires the config-driven CpuDispatcher."

Binding constraint kept verbatim:
> "Multiple Glommio executors can call inference concurrently. Each call runs on the current core. The model's weights are shared across all callers via `Arc<Model>`."
> — `spec/04_embedding_layer/03_inference.md` §7

`CpuDispatcher` already wraps `Arc<ModelHandle>` so this is structurally fine.

## 3. External validation

Not applicable — internal wiring only. No new dependencies; no new algorithms. Every needed piece (`ModelHandle::load`, `CpuDispatcher::from_arc`, fingerprint computation, BGE-small config knob) already exists in the workspace.

## 4. Architecture

### 4.1 The dispatcher lives once per process, not per shard

Today the comment-stub creates `NopDispatcher` *inside* each shard's `LocalExecutorBuilder::spawn` closure (shard/mod.rs:1008). For the real path that would mean N model loads (~130 MiB BERT weights × N shards). Wrong.

Correct shape: load the `ModelHandle` exactly once at `main.rs` time, wrap it in `Arc<CpuDispatcher>` (or `Arc<CachingDispatcher>`), pass the `Arc<dyn Dispatcher>` into `spawn_shard()` so the closure just clones the Arc into the executor.

```
main.rs
  ├── Config::load
  ├── ModelHandle::load(embedder_config)        ← happens once
  ├── let dispatcher: Arc<dyn Dispatcher> =
  │       Arc::new(CachingDispatcher::new(
  │           CpuDispatcher::new(handle),
  │           cache_size))
  └── for shard_id in 0..N:
        spawn_shard(shard_id, ..., dispatcher.clone())
                                    ^^^^^^^^^^^^^^^^^^ cheap Arc clone
```

`Dispatcher: Send + Sync` is already an explicit trait bound (brain-embed/src/dispatcher.rs:38). `CpuDispatcher` already proves Send+Sync at compile time (dispatcher.rs:105-109). So the clone-across-threads pattern is sound.

### 4.2 Model directory resolution

`config.embedder.model` is a string like `"bge-small-en-v1.5"`. It needs to map to a filesystem directory containing `config.json`, `tokenizer.json`, `model.safetensors`. Three resolution sources, in priority order:

1. **`BRAIN_EMBED_MODEL_DIR` env var** — explicit override. Test-friendly; the recall_end_to_end test already uses this exact env name.
2. **Operator-provided absolute path** in the config — if `embedder.model` starts with `/` treat it as a literal path. Lets ops pin to `/var/lib/brain/models/bge-small-en-v1.5/`.
3. **`<XDG_DATA_HOME>/brain/models/<model_name>`** — default, follows XDG basedir. Falls back to `~/.local/share/brain/models/<model_name>` if XDG_DATA_HOME unset.

If the resolved directory doesn't exist OR doesn't contain the three required files, **`brain-server` refuses to start** with a clear error pointing at where to put the model. No fallback to NopDispatcher in production; the stub stays only as a test-only construct in `crates/brain-workers/tests/auto_edge.rs` (where it's local to that test fixture).

### 4.3 CachingDispatcher integration

The LRU cache in `brain-embed/src/cache.rs` wraps any `Dispatcher` and avoids re-embedding the same text. `config.embedder.cache_size` already controls capacity.

For v1 deployments the cache lives at the *process* level (one cache shared across all shards). Two shards encoding the same text both hit the cache. The cache is `Send + Sync` already so the same `Arc` works.

Alternative considered: per-shard caches. Rejected — defeats the purpose of caching agent-scoped repeat queries that land on the same shard.

### 4.4 Fingerprint flow into wire response

`EncodeResponse.embedding_model_fp: [u8; 16]` already exists and is populated from `ctx.executor.embedder.fingerprint()` (brain-ops/src/ops/encode.rs:55). With the real dispatcher, this becomes a non-zero 16-byte BLAKE3 of `config.json + tokenizer.json + model.safetensors`. No further wire work needed.

The renderer (`brain-explore/src/render/encode.rs`) was updated in commit `ffbdc4e` to honestly say "(stub — NopDispatcher; semantic search inactive)" when the fingerprint is `[0; 16]`. After this plan lands, real fingerprints flow through, the stub-warning branch becomes dead in production, and the renderer naturally shows `fp <short hex>` in wide mode.

### 4.5 Shard-spawn surface change

`Shard::spawn(...)` (or whatever function owns shard construction) gains an `Arc<dyn Dispatcher>` parameter. Today the dispatcher is built inside the closure; we pull it out and pass it in.

```rust
// before
pub fn spawn(shard_id: u16, …) -> JoinHandle<…> {
    let join_handle = LocalExecutorBuilder::new(placement)
        .spawn(move || async move {
            let dispatcher: Arc<dyn Dispatcher> = Arc::new(NopDispatcher);
            // ...
        });
}

// after
pub fn spawn(
    shard_id: u16,
    …,
    dispatcher: Arc<dyn Dispatcher>,
) -> JoinHandle<…> {
    let join_handle = LocalExecutorBuilder::new(placement)
        .spawn(move || async move {
            // dispatcher already constructed at process level; cheap clone
            let dispatcher = dispatcher.clone();
            // ...
        });
}
```

`NopDispatcher` is deleted from `shard/mod.rs`. If any internal test in brain-server used it, those tests either get a real `ModelHandle::load` (gated on env) or get a tiny local mock dispatcher.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| **A. Process-wide CpuDispatcher + CachingDispatcher, loaded in main.rs** | Single model load amortises across N shards; one cache shared across shards; matches spec §04/03 §7 "weights shared via Arc<Model>"; clean dep flow | One slot in `Shard::spawn` signature changes | ✓ chosen |
| B. Per-shard model load | No spawn-signature change | N copies of 130 MiB weights in memory; N caches; defeats sharing | rejected |
| C. Lazy load on first encode | Faster startup | First-encode latency spike; complicates error reporting (load failure surfaces as a request error, not a startup error) | rejected — spec §04/03 §9 implies eager load with explicit warm-up |
| D. Keep NopDispatcher behind a feature flag for tests | Tests stay fast | Wire signal "model unavailable" is now ambiguous (could be stub, could be load failure); operator gets less honest fail-stop | rejected — tests can mock locally; the production path refuses to start without a real model |
| E. Auto-download model on first start | Fewer steps for new operators | Adds a network dep at startup, license/auth surface for HF Hub, ~130 MiB transient burden during pull, doesn't match the spec's "out-of-band" rule | rejected per spec |

## 6. Risks / open questions

1. **Risk: model files aren't in the dev container.** The dev container build today doesn't ship BGE-small. We need to either: (a) document a manual `bootstrap-model.sh` for devs, (b) extend `scripts/full-acceptance.sh` to fetch it, or (c) add a `make models` target. Recommendation: (a) for v1 — keep the bootstrap explicit; mention in `docs/notes/embedding-model-install.md` (referenced in phase-05-embedding.md but doesn't seem to exist yet).

2. **Risk: model load time at startup.** BGE-small is ~130 MiB; cold-load is several seconds on a laptop, including the 3 warm-up forward passes. Healthcheck endpoints must wait until the dispatcher is built before reporting ready. brain-server's bootstrap order needs to be: parse config → load model → spawn shards → bind listener. The user gets one good wait at startup instead of latency surprises on the first encode.

3. **Risk: model not present, server refuses to start, dev gets surprised.** Mitigation: the startup error message is *specific* — "BGE-small model files not found at <resolved path>; download from <url> and place at <expected path> OR set BRAIN_EMBED_MODEL_DIR=<path>." Single sentence, copy-pasteable, points at the exact files needed.

4. **Open question: should the in-process cache be optional via config?** Today `embedder.cache_size = 1` (in the test config default) is allowed and produces a useless cache. v1 always wraps in CachingDispatcher; setting cache_size = 1 just makes it nearly degenerate. Recommendation: keep as-is — operators who want zero cache can wrap-but-set-to-1; the perf difference is negligible vs. introducing a no-cache path.

5. **Open question: does brain-extractors' Classifier need the same wiring?** No — classifier loads its own model (per `crates/brain-extractors/src/classifier.rs:38`). Embedding is for memory similarity; classifier is for entity-type classification. Separate models, separate wiring. This plan touches only the memory embedder.

6. **Open question: after this lands, can we delete the AutoEdgeWorker zero-vector guard from `54784ca`?** Recommendation: **keep it.** The guard becomes effectively unreachable in production (real BGE vectors are normalised and non-zero) but stays cheap and provides defense-in-depth against any future code path that might hand a zero vector (test fixtures that bypass the embedder, future GPU init bug, etc.).

7. **Risk: `cargo test -p brain-server` becomes slow because model load runs per test.** Mitigation: test-only `Arc::new(NopDispatcher)` lives in test modules only. Production `Shard::spawn` requires a real dispatcher; tests construct one locally per file. Existing test fixtures (e.g., `crates/brain-workers/tests/auto_edge.rs:31-42`) already define a local NopDispatcher; same pattern extends.

## 7. Test plan

Map each "done when" to one or more tests:

- [ ] Server refuses to start when `embedder.model` points at a missing directory; error message names the missing path.
  → New integration test in `crates/brain-server/tests/startup.rs`: `start_refuses_with_missing_model_dir`.
- [ ] Server starts cleanly when the directory exists with the three required files.
  → Same file: `start_succeeds_with_valid_model_dir` (gated on `BRAIN_EMBED_MODEL_DIR`).
- [ ] One `ModelHandle` is loaded; N shards share the dispatcher (verify by checking the fingerprint is identical across shards in a multi-shard config).
  → New test: `dispatcher_fingerprint_is_consistent_across_shards`.
- [ ] `EncodeResponse.embedding_model_fp` is non-zero for the real path.
  → New test: `encode_fingerprint_is_real_for_loaded_model`.
- [ ] Two distinct texts produce distinct vectors (sanity-check the model is actually running, not returning a constant).
  → New test: `distinct_texts_embed_to_distinct_vectors` (gated on env).
- [ ] AutoEdgeWorker's defensive guard no longer fires in the real path (real vectors are non-zero).
  → Existing tests stay; add `auto_edge_writes_real_edges_with_loaded_model` (gated on env) that asserts edges are produced when vectors are real.
- [ ] The renderer shows real `fp <short hex>` in `-o wide` instead of the stub warning.
  → New brain-explore test: `render_wide_shows_short_fp_with_real_fingerprint` (already exists in `54784ca`; the assertion needs no change, but document that the stub-warning branch becomes unreachable in prod).

The model-loading tests are all gated on `BRAIN_EMBED_MODEL_DIR` being set, mirroring the existing pattern at `brain-planner/tests/recall_end_to_end.rs:660`. CI gets a separate job that sets the env and runs the gated suite; default `cargo test` stays fast.

## 8. Commit shape

Three commits:

1. **`feat(server): load BGE-small at startup; pass dispatcher into shards`**
   - `main.rs` resolves the model directory via the three sources in §4.2, calls `ModelHandle::load`, wraps in `CachingDispatcher`, hands the Arc into shard spawn.
   - `shard/mod.rs::spawn(...)` gains a `dispatcher: Arc<dyn Dispatcher>` parameter; the closure clones it into the executor; `NopDispatcher` is deleted from production paths.
   - Configuration: a small `EmbedderConfig::resolve_model_dir(&self) -> Result<PathBuf, ConfigError>` method handles the env-override / absolute-path / XDG-default cascade.
   - Startup error path: if model load fails, exit non-zero with the actionable message described in §6.3.

2. **`docs: bootstrap-model script + install instructions`**
   - New `scripts/bootstrap-model.sh` that downloads BGE-small from HuggingFace and places it at the XDG-default path.
   - New `docs/notes/embedding-model-install.md` (referenced from phase-05-embedding.md but missing today) covering: where the model lives, how to populate it manually, env override, the file checksums, the spec section that pins model choice.
   - Updates to `docs/development/getting-started.md` (or wherever new-dev onboarding lives) to add the one-line "run scripts/bootstrap-model.sh" step.

3. **`test(server,embed): integration tests for the real embedder path`**
   - The five tests listed in §7. All gated on `BRAIN_EMBED_MODEL_DIR` so default `cargo test` stays fast.
   - A small helper `tests/common/model_dir.rs` in brain-server that wraps the env lookup with a clear `skip` message when unset, so flaky-test errors don't mask "the model wasn't downloaded."

## 9. Confirmation

Three judgment calls to sign off on:

1. **Process-wide CachingDispatcher (not per-shard).** §4.1 + §5 row A. Confirms we share one ~130 MiB BERT model across N shards instead of N copies.

2. **Model directory resolution order: env > absolute path > XDG default.** §4.2. Lets ops pin a known path, lets devs use a local override, defaults clean for first-time users.

3. **Server refuses to start without a model.** §4.2 last paragraph + §6 risk 3. The alternative — keep NopDispatcher as a "demo mode" — was explicitly rejected as making the production behavior ambiguous. Confirm the strict-startup posture is right.

After sign-off, three commits as drafted in §8. Estimated wall time: ~2 hours focused work for the wiring + tests; an extra ~30 min if the bootstrap-model.sh needs HuggingFace auth handling.
