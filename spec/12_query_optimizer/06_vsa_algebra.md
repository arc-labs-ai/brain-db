# 12.06 VSA Algebra

Vector-Symbolic Architecture primitives for binding typed predicates and entities into high-dimensional vectors that compose under simple arithmetic operations. Ships as a callable algebra module; REASON's analogical-inference nudge (§6) is its first executor consumer. PLAN integration is not proposed and remains out of scope.

## 1. What VSA is

VSA encodes structured information — roles, fillers, tuples — into fixed-dimension vectors using algebraic operations that approximately invert. The classical primitives are **bind** (combine role + filler into a single composite vector), **bundle** (combine multiple bound vectors into a set), and **unbind** (recover the filler given the role).

The Brain implementation uses **HRR** — Holographic Reduced Representation — where bind is circular convolution (FFT-multiplied vectors) and unbind is circular correlation.

## 2. Parameters

| Parameter | Value | Why |
|---|---|---|
| `D` | 512 | High enough for hundreds of role/filler pairs without crosstalk; low enough that the FFT path stays in microseconds on CPU |
| Codebook | Deterministic, seeded | Same role/filler vector across runs — required for any cross-call composition |
| Norm | Unitary | All codebook vectors and all derived vectors are L2-normalized; keeps repeated bind chains numerically stable |

512-dim HRR vectors composed via FFT are the field-standard parameterisation; the algebra module exposes them as the only shape Brain supports.

## 3. Operators

```rust
pub fn bind(role: &Vsa, filler: &Vsa) -> Vsa;         // circular conv via FFT
pub fn bundle(items: &[Vsa]) -> Vsa;                  // normalized sum
pub fn unbind(composite: &Vsa, role: &Vsa) -> Vsa;    // circular correlation
pub fn normalize(v: &mut Vsa);                        // in-place L2 normalize
pub fn cosine(a: &Vsa, b: &Vsa) -> f32;               // dot product (assumes unitary)
```

- **bind** combines a role vector with a filler vector. The composite is roughly orthogonal to both, but unbinding with the role approximately recovers the filler.
- **bundle** combines a set of bound vectors into a single composite by summing then normalizing. The result is closer to each constituent than to a random vector — superposition as approximate set membership.
- **unbind** is the inverse of bind: `unbind(bind(r, f), r) ≈ f`, with noise that grows with the number of bundle members.
- **normalize** keeps the algebra numerically stable; chained operations without normalization drift in magnitude.
- **cosine** is the similarity measure. The algebra is engineered so that approximate-equality between two HRR vectors corresponds to cosine ≥ 0.5 — that's the unbinding-noise threshold.

## 4. Codebook

The codebook is the set of named role/filler vectors used as building blocks. Brain's codebook is:

- **Deterministic.** Vectors are generated from a fixed seed; the same role name always maps to the same vector across runs and across deployments.
- **Pre-allocated for system roles.** Every `EdgeKind`, every `StatementKind`, and every `behavior_*` predicate gets a stable codebook slot at module init.
- **Extensible at runtime.** User-defined predicates and entity types get codebook slots derived from their qname hash, also deterministic.

```rust
pub struct VsaCodebook {
    roles: HashMap<RoleId, Vsa>,           // edge_kind, predicate, role-tag
    fillers: HashMap<FillerId, Vsa>,       // entity / value vectors
}
```

The deterministic codebook is what makes cross-call composition possible — two queries from two clients constructing the same logical structure produce the same HRR vector.

## 5. The analogy_query API

The first user-facing surface for the algebra module is `analogy_query`:

```rust
pub fn analogy_query(
    codebook: &mut Codebook,
    triple_a: &VsaVec,       // reserved: unused today, locks the v1.1 signature
    triple_c: &VsaVec,
    role_to_extract: &str,
) -> Result<Option<(String, f32)>, VsaError>;
```

Solves "A is to B as C is to ?" — but not against an arbitrary caller-supplied
corpus. Mechanically (`crates/brain-planner/src/vsa/analogy.rs`):

1. `triple_c` is an HRR vector already composed via `encode_triple`: bind
   each role/filler pair, then bundle — `bundle(role⊛subject,
   role⊛predicate, role⊛object)`.
2. Unbind `triple_c` with the role named by `role_to_extract` (e.g.
   `ROLE_OBJECT`) to recover a noisy filler vector.
3. Argmax-cosine that noisy vector against the shared `Codebook`'s own
   registered filler vocabulary (`Codebook::cleanup`) — not a
   caller-supplied corpus — returning the single best-matching filler name
   and its cosine score, or `None` if the codebook has no fillers yet.

`triple_a` is accepted but unused in the implementation today (bound as
`_triple_a`) — the parameter locks the public signature for richer analogy
forms ("A : B :: C : ?" using both triples) in a future version; the answer
is currently determined purely by `triple_c` and `role_to_extract`. Filler
names are plain display strings (an entity's canonical name, or a rendered
statement value) resolved by the caller, not `EntityId`s — the algebra
module itself has no entity-table dependency.

This is the structural similarity primitive REASON's analogical-inference
nudge is built on (§6). The initial release also exposes it directly for
tools and experimentation.

## 6. Integration status: REASON live, PLAN out of scope

The algebra module was small and standalone enough to ship early — worth doing because (a) the codebook discipline benefits from baking in before user predicate ids proliferate, and (b) it gives downstream consumers (tools, custom planners) a stable surface to build on.

**REASON integration has landed** (`crates/brain-planner/src/executor/analogical.rs`, wired into `execute_reason` in `crates/brain-planner/src/executor/reason.rs`). The two blockers originally named here are resolved, for REASON specifically:

1. **Cost-model integration** — turned out not to be a blocker. REASON's cost estimator (`cost::cost_reason`, `crates/brain-planner/src/cost.rs`) is a standalone heuristic with no RRF dependency; RRF only fuses RECALL's semantic/lexical/graph lanes, and REASON never used RRF, so there was no "where does VSA-similarity sit relative to RRF" question to answer. `cost_reason` simply gained a small additive term for the new bind/unbind + cosine-rank work — `max_inferences * (bind_unbind_ms + cosine_rank_ms)`, using the ~10 µs bind/unbind and ~5 ms cosine-rank figures from §7. No cost-model redesign was needed.
2. **Wire-level exposure** — resolved as: no new opcode, no new request field. The nudge is an automatic internal scoring signal inside `execute_reason` — REASON re-ranks its already-graph-qualified evidence by structural fit and reports the term in `ReasonTraceScoreBreakdown.analogical_fit` (trace mode only). The `InferenceKind::AnalogicalInference` wire value, defined on the wire since before this integration but never emitted, is now emitted when the nudge materially reshapes a step's result. See [01. Architecture](../01_architecture/03_primitives.md) §4.2 for the primitive-level description.

**PLAN integration was never proposed** and remains out of scope — this spec has never documented a VSA tie-in for PLAN's A*/MCTS search, and this integration doesn't add one. If a PLAN integration is pursued in a future version, it needs its own cost-model and wire-exposure analysis; nothing here resolves that for PLAN.

Until a PLAN integration is proposed and lands, the algebra module remains directly callable from in-process consumers and from tests, with REASON as its first and, for now, only executor consumer.

## 7. Performance

The FFT-based bind/unbind path runs at D=512 in ~10 µs per op on commodity CPU. A bundle of 100 bound vectors at D=512 sits at ~1 ms total. Cosine-rank against a 10k-entity corpus is ~5 ms.

These targets held up as the executor-consumption baseline: REASON's analogical-inference nudge (§6) uses exactly these two operations — one bind/unbind per evidence item's triple, one cosine-rank against the codebook — and its `cost_reason` term (`crates/brain-planner/src/cost.rs`) was set directly from these figures rather than fresh benchmarking. Tightening the budget further, if REASON's real-world latency profile calls for it, is now a REASON-side tuning question, not a blocked integration question.

## 8. Tests

- Bind/unbind round-trip: `cosine(unbind(bind(r, f), r), f) ≥ 0.8` for unitary r and f.
- Bundle membership: each constituent has `cosine(bundle, member) > cosine(bundle, random)` consistently across bundle sizes 1–100.
- Deterministic codebook: two module inits produce byte-identical role/filler vectors.
- Analogy query golden: a 50-pair classical analogy set ("man : king :: woman : ?") resolves correctly.

Test file: `crates/brain-planner/src/vsa/mod.rs::tests`.
