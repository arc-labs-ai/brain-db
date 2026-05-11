//! Integration test for `CpuDispatcher` under concurrent load.
//!
//! Spec `04/03 §7` says: "multiple Glommio executors can call inference
//! concurrently. Each call runs on the current core. The model's
//! weights are shared across all callers via `Arc<Model>`." This test
//! is the empirical proof that `Arc<ModelHandle>` + candle's `Tensor`
//! are actually thread-safe at runtime, not just at the type level.
//!
//! Gated on `BRAIN_EMBED_MODEL_DIR`. Default `cargo test` reports
//! "skipping" and returns; CI exports the var.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use brain_embed::{CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle, VECTOR_DIM};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";

fn dot(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn try_load() -> Option<CpuDispatcher> {
    let Ok(dir) = std::env::var(ENV_VAR) else {
        eprintln!("skipping: set {ENV_VAR} to a BGE-small directory to run");
        return None;
    };
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "{ENV_VAR}={} is not a directory",
        dir.display()
    );
    let handle = ModelHandle::load(&EmbedderConfig::new(dir)).expect("real model loads");
    Some(CpuDispatcher::new(handle))
}

#[test]
fn cpu_dispatcher_concurrent_calls_match_serial() {
    let Some(dispatcher) = try_load() else {
        return;
    };
    let dispatcher = Arc::new(dispatcher);
    let text = "the quick brown fox jumps over the lazy dog";

    // Baseline: serial result.
    let baseline = dispatcher.embed(text).expect("serial embed");

    // 8 threads each compute the same embedding. All must match the
    // baseline to cosine ≥ 1 - 1e-6 (CPU determinism per spec §03 §12).
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let d = Arc::clone(&dispatcher);
        handles.push(thread::spawn(move || {
            d.embed(text).expect("concurrent embed")
        }));
    }

    for h in handles {
        let v = h.join().expect("thread did not panic");
        let cos = dot(&v, &baseline);
        assert!(
            (cos - 1.0).abs() < 1e-6,
            "concurrent vector diverged from baseline; cos = {cos}"
        );
    }
}

#[test]
fn cpu_dispatcher_batch_matches_single() {
    let Some(dispatcher) = try_load() else {
        return;
    };
    let texts = ["hello", "world", "embedding"];
    let batched = dispatcher.embed_batch(&texts).expect("batch");
    assert_eq!(batched.len(), 3);
    for (i, text) in texts.iter().enumerate() {
        let single = dispatcher.embed(text).expect("single");
        let cos = dot(&batched[i], &single);
        assert!(
            (cos - 1.0).abs() < 1e-4,
            "batched[{i}] != single({text}); cos = {cos}"
        );
    }
}

#[test]
fn cpu_dispatcher_fingerprint_stable() {
    let Some(dispatcher) = try_load() else {
        return;
    };
    let fp1 = dispatcher.fingerprint();
    let fp2 = dispatcher.fingerprint();
    assert_eq!(fp1, fp2, "fingerprint must be stable per dispatcher");
    assert_ne!(fp1, [0u8; 16], "fingerprint should not be all-zero");
}

#[test]
fn cpu_dispatcher_clone_shares_model() {
    let Some(d1) = try_load() else {
        return;
    };
    let d2 = d1.clone();
    // Both clones must share the underlying Arc<ModelHandle>.
    assert_eq!(
        d1.fingerprint(),
        d2.fingerprint(),
        "clones must agree on fingerprint"
    );
    // And produce the same vector.
    let v1 = d1.embed("ping").unwrap();
    let v2 = d2.embed("ping").unwrap();
    let cos = dot(&v1, &v2);
    assert!(
        (cos - 1.0).abs() < 1e-6,
        "clones produced different vectors; cos = {cos}"
    );
}
