//! End-to-end embedder tests that exercise the loaded BGE-small
//! forward pass. Two tests, both gated on `BRAIN_EMBED_MODEL_DIR`:
//!
//! 1. `encode_fingerprint_is_real_for_loaded_model` — the loaded
//!    model produces a non-zero fingerprint and a unit-norm, non-zero
//!    vector. This is the sanity assertion that the model actually
//!    ran (rather than returning a constant) and that downstream
//!    consumers (HNSW, wire) see a real signal.
//! 2. `distinct_texts_embed_to_distinct_vectors` — two clearly
//!    different sentences must produce clearly different vectors, and
//!    the same text must be deterministic across repeated calls. This
//!    is the cheapest possible "the model is actually running" check.
//!
//! Both tests bypass the brain-server frame harness and call
//! `Dispatcher::embed` directly. The harness coverage (wire fingerprint
//! round-trip, ENCODE→RECALL with a real model) lives in
//! brain-planner's `recall_with_real_embedder_end_to_end`; what's new
//! here is the per-vector property assertions on the live forward
//! pass, which the harness doesn't expose. If the wire path ever
//! diverges from `dispatcher.embed`, that's a separate bug — the
//! property assertions below would still hold against the dispatcher.

// Linux-gated because brain-server's brain-embed dep is itself
// Linux-gated. See embedder_startup.rs for the full reasoning.
#![cfg(target_os = "linux")]

use brain_embed::{CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle, VECTOR_DIM};

#[path = "common/model_dir.rs"]
mod model_dir;

/// L2 norm of a vector. Used to verify the BGE pooling/normalisation
/// step actually ran — un-normalised BERT pooled outputs sit at
/// arbitrary magnitudes, so a deviation from 1.0 is a real signal of
/// a forward-path regression.
fn l2_norm(v: &[f32; VECTOR_DIM]) -> f32 {
    let sq: f32 = v.iter().map(|x| x * x).sum();
    sq.sqrt()
}

/// Cosine similarity between two L2-normalised vectors collapses to
/// the dot product. Used to assert "these vectors are not the same
/// thing" — anything below 0.95 between unrelated sentences is a
/// solid pass; BGE-small typically lands well below 0.5 on unrelated
/// pairs.
fn cosine_similarity(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Element-wise float equality with a per-element tolerance. We need
/// this rather than `==` because BGE-small's forward pass is
/// deterministic on CPU but not bit-exact across runs in every
/// reading of the candle source; a tolerance of `1e-6` is well below
/// the threshold at which any downstream consumer would behave
/// differently. We don't pull in `approx` — a four-line helper keeps
/// the test crate dep-free.
fn assert_relative_eq(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM], tol: f32) {
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        let diff = (x - y).abs();
        assert!(
            diff <= tol,
            "vectors differ at index {i}: |{x} - {y}| = {diff} > {tol}",
        );
    }
}

fn build_dispatcher(dir: std::path::PathBuf) -> CpuDispatcher {
    let cfg = EmbedderConfig::new(dir);
    let handle = ModelHandle::load(&cfg).expect("model loads");
    CpuDispatcher::new(handle)
}

/// The loaded model must report a non-zero fingerprint (the
/// `[0; 16]` value is the NopDispatcher sentinel that downstream
/// renderers branch on) AND it must produce a non-zero L2-normalised
/// vector. Three properties pinned together because they're three
/// facets of the same thing: "the real model is wired and running".
#[test]
fn encode_fingerprint_is_real_for_loaded_model() {
    model_dir::with_model_dir(|dir| {
        let dispatcher = build_dispatcher(dir);

        let fp = dispatcher.fingerprint();
        assert_ne!(
            fp, [0u8; 16],
            "non-zero fingerprint is the wire signal that semantic \
             recall is live; [0;16] means we're still on the stub",
        );

        let v = dispatcher.embed("hello world").expect("embed");

        // The vector must not be all zeros — that's the failure mode
        // we're guarding against in production (the NopDispatcher
        // crash this whole H1+H2+H3 stack was written to eliminate).
        assert!(
            v.iter().any(|x| *x != 0.0),
            "real embedder must not return an all-zero vector",
        );

        // BGE-small's last step is L2 normalisation; if the norm
        // drifts from 1.0, either the pooling stage failed or the
        // post-pool normalise step regressed.
        let norm = l2_norm(&v);
        assert!(
            (norm - 1.0).abs() < 0.001,
            "BGE-small must produce L2-normalised vectors; got norm={norm}",
        );
    });
}

/// The cheapest possible "the model is actually running" check.
/// Distinct concepts must land at distinct points in the embedding
/// space; identical inputs must land at the same point. A constant
/// dispatcher (or one that returns a hash of the input) would fail
/// either property.
#[test]
fn distinct_texts_embed_to_distinct_vectors() {
    model_dir::with_model_dir(|dir| {
        let dispatcher = build_dispatcher(dir);

        let v1 = dispatcher.embed("the cat sat on the mat").expect("embed v1");
        let v2 = dispatcher.embed("quantum chromodynamics").expect("embed v2");

        // 0.95 is generous; BGE-small typically lands unrelated
        // sentences well below 0.5 cosine. The bound exists to catch
        // a constant or near-constant dispatcher without flagging
        // legitimate model drift.
        let sim = cosine_similarity(&v1, &v2);
        assert!(
            sim < 0.95,
            "unrelated sentences must not collapse to the same vector; \
             cosine={sim}",
        );

        // Same text -> same vector. Determinism is the foundation of
        // the cache, the fingerprint, and recall stability across
        // sessions; if this regresses, every other guarantee in the
        // embedding layer cracks.
        let v3 = dispatcher.embed("the cat sat on the mat").expect("embed v3");
        assert_relative_eq(&v1, &v3, 1e-6);
    });
}
