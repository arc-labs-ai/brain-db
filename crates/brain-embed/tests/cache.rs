//! Integration test for `CachingDispatcher<CpuDispatcher>`.
//!
//! Validates that wrapping a real BGE-small dispatcher with the LRU
//! cache produces bit-identical vectors on the second call, and that
//! the stats reflect what happened.
//!
//! Gated on `BRAIN_EMBED_MODEL_DIR`. Default `cargo test` prints
//! "skipping" and returns; CI sets the env var.

use std::path::PathBuf;

use brain_embed::{
    CachingDispatcher, CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle, VECTOR_DIM,
};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";

fn try_load_cached() -> Option<CachingDispatcher<CpuDispatcher>> {
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
    let cpu = CpuDispatcher::new(handle);
    Some(CachingDispatcher::new(cpu, 100))
}

fn bytewise_equal(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.to_bits() == y.to_bits())
}

#[test]
fn second_call_returns_bitwise_identical_vector() {
    let Some(cache) = try_load_cached() else {
        return;
    };
    let text = "the cat sat on the mat";

    let v1 = cache.embed(text).expect("first embed");
    let v2 = cache.embed(text).expect("second embed");
    assert!(
        bytewise_equal(&v1, &v2),
        "cached hit must return the exact bytes we stored"
    );

    let stats = cache.stats();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.size, 1);
    assert!((stats.hit_rate().unwrap() - 0.5).abs() < 1e-9);
}

#[test]
fn distinct_texts_grow_cache() {
    let Some(cache) = try_load_cached() else {
        return;
    };
    cache.embed("alpha").unwrap();
    cache.embed("beta").unwrap();
    cache.embed("gamma").unwrap();
    let stats = cache.stats();
    assert_eq!(stats.misses, 3);
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.size, 3);
}

#[test]
fn fingerprint_matches_inner_dispatcher() {
    let Some(cache) = try_load_cached() else {
        return;
    };
    let fp = cache.fingerprint();
    let inner_fp = cache.inner().fingerprint();
    assert_eq!(fp, inner_fp, "wrapper must surface inner fingerprint");
    assert_ne!(fp, [0u8; 16]);
}

#[test]
fn clear_drops_cache_state() {
    let Some(cache) = try_load_cached() else {
        return;
    };
    cache.embed("hello").unwrap();
    assert_eq!(cache.stats().size, 1);
    cache.clear();
    assert_eq!(cache.stats().size, 0);
    // Counters survive clear.
    assert_eq!(cache.stats().misses, 1);
    // Re-embed same text → another miss (since cache was cleared).
    cache.embed("hello").unwrap();
    assert_eq!(cache.stats().misses, 2);
    assert_eq!(cache.stats().size, 1);
}
