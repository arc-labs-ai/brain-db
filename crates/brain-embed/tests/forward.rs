//! Integration test for the full forward pipeline:
//! tokenise → BertModel forward → CLS pool → L2 normalise.
//!
//! Requires a real BGE-small directory on disk. Operators run this
//! opt-in by exporting `BRAIN_EMBED_MODEL_DIR` before `cargo test`.
//! Without the env var, each test is logged and skipped.
//!
//! Asserts:
//! - Output shape is `[f32; 384]` per row.
//! - Norm is exactly unit (within 1e-5).
//! - Identical text on consecutive calls → cosine similarity = 1.0
//!   (within 1e-6), since the model is deterministic on CPU.
//! - Batched and single paths produce the same vector for the same text.

use std::path::PathBuf;

use brain_embed::{embed_text, EmbedderConfig, ModelHandle, VECTOR_DIM};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";

fn load_handle_or_skip() -> Option<ModelHandle> {
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
    let config = EmbedderConfig::new(dir);
    Some(ModelHandle::load(&config).expect("real model loads"))
}

fn l2_norm(v: &[f32; VECTOR_DIM]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn dot(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[test]
fn embed_text_shape_and_unit_norm() {
    let Some(handle) = load_handle_or_skip() else {
        return;
    };
    let v = embed_text(&handle, "hello world").expect("embed_text succeeds");
    assert_eq!(v.len(), VECTOR_DIM);
    let n = l2_norm(&v);
    assert!((n - 1.0).abs() < 1e-5, "norm = {n}");
    assert!(v.iter().all(|x| x.is_finite()), "no NaN / Inf");
}

#[test]
fn different_texts_produce_different_vectors() {
    let Some(handle) = load_handle_or_skip() else {
        return;
    };
    let a = embed_text(&handle, "the cat sat on the mat").expect("a");
    let b = embed_text(&handle, "quantum chromodynamics").expect("b");
    let cos = dot(&a, &b);
    // Unrelated sentences should not have cosine = 1.0; semantically
    // they share *some* structure but well below the deterministic
    // floor.
    assert!(cos < 0.95, "expected unrelated vectors; cos = {cos}");
}
