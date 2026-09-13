//! Determinism property test for the forward path.
//!
//! For a given input, the model is deterministic — repeated
//! inference produces the same vector. We rely on this for the cue
//! cache to be useful.
//!
//! Caveats — out of scope for this test:
//! - CPU vs GPU may differ.
//! - AVX-512 vs AVX2 may differ.
//!
//! What we *do* assert: within a single machine + ISA, repeated
//! inference returns **bit-identical** output. Comparator is
//! `f32::to_bits()` so we don't depend on floating-point `==`.
//!
//! Gated on `BRAIN_EMBED_MODEL_DIR`. Without the env var, each test
//! prints "skipping" and returns; CI sets the var to exercise the
//! property.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread;

use brain_embed::{embed_batch, embed_text, EmbedderConfig, ModelHandle, VECTOR_DIM};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";
const RUNS: usize = 100;

fn try_load() -> Option<ModelHandle> {
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
    Some(ModelHandle::load(&EmbedderConfig::new(dir)).expect("real model loads"))
}

/// Amortise the ~1.5 s model load across the (read-only) tests in
/// this file. The reload test deliberately bypasses this.
fn shared_handle() -> Option<&'static ModelHandle> {
    static HANDLE: OnceLock<Option<ModelHandle>> = OnceLock::new();
    HANDLE.get_or_init(try_load).as_ref()
}

fn assert_bytewise_eq(baseline: &[f32; VECTOR_DIM], got: &[f32; VECTOR_DIM], run_idx: usize) {
    for (i, (b, g)) in baseline.iter().zip(got.iter()).enumerate() {
        assert_eq!(
            b.to_bits(),
            g.to_bits(),
            "run {run_idx} dim {i}: baseline {b} = {:#010x}, got {g} = {:#010x}",
            b.to_bits(),
            g.to_bits(),
        );
    }
}

#[test]
fn embed_text_is_bit_identical_across_100_runs() {
    let Some(handle) = shared_handle() else {
        return;
    };
    let text = "the quick brown fox jumps over the lazy dog";
    let baseline = embed_text(handle, text).expect("baseline");
    for run in 1..RUNS {
        let v = embed_text(handle, text).expect("run");
        assert_bytewise_eq(&baseline, &v, run);
    }
}

#[test]
fn embed_text_long_input_is_bit_identical_across_100_runs() {
    let Some(handle) = shared_handle() else {
        return;
    };
    // ~500 tokens: 100 repetitions of a 5-word phrase. Just under the
    // 512 cap; exercises the full forward pass without truncation
    // (which would still be deterministic but adds a different code
    // path we test separately if needed).
    let text = "the cat sat on mat ".repeat(100);
    let baseline = embed_text(handle, &text).expect("baseline long");
    for run in 1..RUNS {
        let v = embed_text(handle, &text).expect("run long");
        assert_bytewise_eq(&baseline, &v, run);
    }
}

#[test]
fn embed_batch_is_bit_identical_across_100_runs() {
    let Some(handle) = shared_handle() else {
        return;
    };
    let texts = [
        "hi",
        "the cat sat on the mat",
        "embedding determinism property",
        "a longer sentence with several words and punctuation, for variety",
    ];
    let baseline = embed_batch(handle, &texts).expect("baseline batch");
    assert_eq!(baseline.len(), 4);
    for run in 1..RUNS {
        let v = embed_batch(handle, &texts).expect("run batch");
        assert_eq!(v.len(), 4);
        for (row, (b, g)) in baseline.iter().zip(v.iter()).enumerate() {
            for (dim, (bv, gv)) in b.iter().zip(g.iter()).enumerate() {
                assert_eq!(
                    bv.to_bits(),
                    gv.to_bits(),
                    "run {run} row {row} dim {dim}: baseline {bv} vs got {gv}"
                );
            }
        }
    }
}

#[test]
fn embed_is_bit_identical_across_model_reloads() {
    // Two independent loads of the same model directory; same text
    // through each. Validates that ModelHandle::load itself doesn't
    // introduce non-determinism (e.g. via a random init somewhere).
    let Some(_h0) = shared_handle() else { return };
    let h1 = try_load().expect("load 1");
    let h2 = try_load().expect("load 2");

    assert_eq!(
        h1.fingerprint(),
        h2.fingerprint(),
        "fingerprint must be reproducible across loads"
    );

    let text = "model reload determinism";
    let v1 = embed_text(&h1, text).expect("v1");
    let v2 = embed_text(&h2, text).expect("v2");
    assert_bytewise_eq(&v1, &v2, 0);
}

#[test]
fn embed_concurrent_threads_bit_identical_to_serial() {
    let Some(_h) = shared_handle() else { return };
    // Build an Arc<ModelHandle> for sharing. We can't share the
    // OnceLock's static reference into a thread that outlives the
    // test if we use `Arc::new(&'static handle.clone())`, so reload
    // fresh and share via Arc.
    let handle = Arc::new(try_load().expect("load"));
    let text = "the quick brown fox jumps over the lazy dog";

    // Serial baseline.
    let baseline = embed_text(&handle, text).expect("baseline");

    // 8 threads. Stricter than 5.4's cosine ≥ 1 - 1e-6 check — this
    // is the bitwise assertion. If candle parallelises internally and
    // produces different rounding, this test surfaces it.
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let h = Arc::clone(&handle);
        handles.push(thread::spawn(move || embed_text(&h, text).expect("thread")));
    }
    for (idx, h) in handles.into_iter().enumerate() {
        let v = h.join().expect("thread did not panic");
        assert_bytewise_eq(&baseline, &v, idx);
    }
}
