# 12.06 VSA Algebra

Vector-Symbolic Architecture primitives for binding typed predicates and entities into high-dimensional vectors that compose under simple arithmetic operations. Ships as a callable algebra module; PLAN / REASON integration lands in a future version.

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
    given: &[(Vsa /* role */, Vsa /* filler */)],
    missing_role: Vsa,
    corpus: &[(EntityId, Vsa)],
) -> Vec<(EntityId, f32)>;
```

Solves "A is to B as C is to ?". Mechanically:

1. Compose the given pairs via bind + bundle into a composite.
2. Unbind with `missing_role` to surface the candidate filler.
3. Cosine-rank against the corpus to find the nearest entity.

This is the structural similarity primitive that PLAN / REASON will lean on in a future version. The initial release exposes it for tools and experimentation.

## 6. Why the module ships before the integration

The algebra module is small and standalone — it's worth shipping early because (a) the codebook discipline benefits from baking in before user predicate ids proliferate, and (b) it gives downstream consumers (tools, custom planners) a stable surface to build on.

PLAN / REASON integration is deferred because:

1. The cost-model integration (where does VSA-similarity sit relative to RRF in the planner's cost estimator?) needs more bench data.
2. The wire-level exposure (an `analogy` opcode, or a new field on the `RECALL` read / the `QUERY_TRACE` debug op?) is an open design call that's better made after the algebra has been used in anger.

Until that integration lands, the algebra module is callable from in-process consumers and from tests but does not appear on the wire. The integration's wire surface will require a wire-version bump if it warrants one.

## 7. Performance

The FFT-based bind/unbind path runs at D=512 in ~10 µs per op on commodity CPU. A bundle of 100 bound vectors at D=512 sits at ~1 ms total. Cosine-rank against a 10k-entity corpus is ~5 ms.

These targets are loose because no executor consumes the algebra in latency-critical paths yet. The bench harness verifies they hold; tightening the budget happens when PLAN / REASON wire it in.

## 8. Tests

- Bind/unbind round-trip: `cosine(unbind(bind(r, f), r), f) ≥ 0.8` for unitary r and f.
- Bundle membership: each constituent has `cosine(bundle, member) > cosine(bundle, random)` consistently across bundle sizes 1–100.
- Deterministic codebook: two module inits produce byte-identical role/filler vectors.
- Analogy query golden: a 50-pair classical analogy set ("man : king :: woman : ?") resolves correctly.

Test file: `crates/brain-planner/src/vsa/mod.rs::tests`.
