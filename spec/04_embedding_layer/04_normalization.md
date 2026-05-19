# 04.04 L2 Normalization

After the model produces a 384-dim vector, the substrate normalizes it to unit L2 norm. This file specifies the operation and its consequences.

## 1. The operation

For a vector `v = [v_0, v_1, ..., v_383]`:

```
norm = sqrt(sum(v_i² for i in 0..384))
v_normalized = v / norm
```

After normalization:

```
||v_normalized||₂ = 1.0  (within floating-point precision)
```

## 2. Why normalize

### 2.1 Cosine similarity = dot product

For two unit vectors `a` and `b`:

```
cos_sim(a, b) = (a · b) / (||a|| · ||b||)
              = (a · b) / (1 · 1)
              = a · b
```

The dot product of unit vectors is the cosine similarity. Storing only normalized vectors means similarity is a single SIMD-friendly fused-multiply-add chain — no division at query time, no per-vector norm precomputation.

### 2.2 Numerical stability

Normalized vectors stay bounded. Intermediate computations (in HNSW search, in attractor dynamics) are easier to reason about when all vectors live on the unit sphere.

### 2.3 Geometric uniformity

All vectors have the same magnitude. Distance and similarity computations are uniform across the space. Without normalization, vectors with larger magnitudes would dominate similarity scores.

## 3. The model's output

`bge-small-en-v1.5` produces vectors that are *approximately* unit-norm. The model is trained with a normalization-aware loss, so its outputs naturally cluster near the unit sphere.

But "approximately" isn't good enough. The norm typically falls in [0.95, 1.05]. We re-normalize to be exactly unit-norm; the dot-product simplification depends on it.

## 4. Implementation

```rust
fn l2_normalize(v: &mut [f32; 384]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();

    if norm < 1e-8 {
        // Pathological: zero vector. Should never happen post-inference,
        // but defensive: leave as-is and let downstream handle.
        return;
    }

    let inv_norm = 1.0 / norm;
    for x in v.iter_mut() {
        *x *= inv_norm;
    }
}
```

SIMD-accelerated versions exist for AVX2 and NEON; the substrate uses them on supported architectures. The non-SIMD version is the reference.

Cost: ~50 ns on modern CPU. Negligible compared to the 5–10 ms of inference.

## 5. Pre vs post normalization

A subtle point: should the substrate normalize before or after the model's projection layer?

The bge-small-en-v1.5 model outputs are already roughly normalized — meaning the projection layer is trained to produce normalizable vectors. We normalize **after** the model's output, which:

- Doesn't affect retrieval quality.
- Gives us guaranteed unit norm.
- Costs ~50 ns.

Some literature suggests normalizing before downstream operations rather than after the model. For our use case, post-output normalization is the right place — it's the single point where vectors enter the substrate's storage and indexing.

## 6. Norm validation on input

For `ENCODE_VECTOR_DIRECT` (clients submitting their own vectors), the substrate validates the norm:

```rust
const NORM_TOLERANCE: f32 = 1e-3;

fn validate_unit_norm(v: &[f32; 384]) -> Result<(), InvalidVector> {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();

    if (norm - 1.0).abs() > NORM_TOLERANCE {
        return Err(InvalidVector::NotNormalized { norm });
    }
    Ok(())
}
```

The tolerance (1e-3) accommodates floating-point precision while catching gross errors (a vector with norm 0.5 or 1.5 is clearly not normalized).

If a client submits a non-unit vector, the substrate rejects with `InvalidVector::NotNormalized`. The error includes the actual norm so the client can debug.

The substrate doesn't auto-normalize client vectors. The reasoning: if the client supplies a non-unit vector, that's a bug in their pipeline, not something to silently fix. The strict policy catches mistakes early.

## 7. Norm validation on read

When a vector is read from the arena (during HNSW search, attractor dynamics, etc.), the substrate optionally re-validates the norm. By default, this is **off** on the hot path — too expensive for every read.

The integrity-check worker periodically scans vectors and validates norms. Vectors with bad norms are flagged for re-embedding. See [11. Background Workers](../11_background_workers/) §Integrity.

## 8. NaN and Inf handling

Vectors with NaN or Inf elements are invalid. The substrate detects these:

- During normalization: dividing by NaN propagates NaN; the result is rejected.
- On client input (`ENCODE_VECTOR_DIRECT`): explicit check rejects vectors containing NaN/Inf.

Errors return `InvalidVector::ContainsNaN` or `InvalidVector::ContainsInf`.

## 9. Zero vectors

A zero vector (all elements 0.0) has zero norm and is undefined under normalization. The substrate refuses zero vectors:

- **From the model:** if the model emits an exact-zero output, something is wrong. The substrate logs and rejects.
- **From clients:** `ENCODE_VECTOR_DIRECT` with a zero vector is rejected with `InvalidVector::ZeroVector`.

In practice, well-trained models never emit exact zero vectors for reasonable inputs.

## 10. Norm drift over operations

Some downstream operations (attractor dynamics, VSA bind/bundle) may produce intermediate vectors that aren't unit-norm. This is fine — the operations re-normalize their outputs before using them as queries or storing them.

The invariant: *stored* vectors and *query* vectors are unit-norm. *Intermediate* computations may temporarily produce non-unit vectors.

## 11. Cosine vs Euclidean

We use cosine similarity throughout. For unit vectors:

```
euclidean_dist(a, b)² = ||a - b||²
                     = ||a||² - 2(a·b) + ||b||²
                     = 1 - 2(a·b) + 1
                     = 2 - 2(a·b)
```

So Euclidean distance² = 2 - 2 × dot product. Sorting by Euclidean distance ascending is equivalent to sorting by dot product descending. The choice between cosine and Euclidean is purely cosmetic for unit vectors.

The substrate uses cosine similarity (dot product) directly. HNSW's hnsw_rs supports it natively.

---

*Continue to [`05_caching.md`](05_caching.md) for the cue cache.*
