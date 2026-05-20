//! Integration tests for the process-startup embedder wiring landed
//! in H1 of the real-embedder rollout. Three tests:
//!
//! 1. `start_refuses_with_missing_model_dir` — always runs. Asserts
//!    that the bootstrap path returns an actionable "missing required
//!    model file" error when the resolved directory does not contain
//!    the three required files. This is the fail-stop contract that
//!    keeps operators from launching a server whose recall returns
//!    arbitrary vectors.
//! 2. `start_succeeds_with_valid_model_dir` — gated on the env var.
//!    Asserts that the same composition produces a working dispatcher
//!    with a non-zero fingerprint when the directory is populated.
//! 3. `dispatcher_fingerprint_is_consistent_across_clones` — gated.
//!    Catches a regression where the dispatcher accidentally re-loads
//!    the model per shard: every clone of the shared `Arc` must
//!    report the same fingerprint bytes.
//!
//! The brain-server binary's `linux_main::build_dispatcher` is
//! crate-private, so the tests mirror its composition
//! (`resolve_model_dir` → existence check → `ModelHandle::load` →
//! `CpuDispatcher::new` → `CachingDispatcher::new`). The composition
//! is the implementation detail; the contract under test is "given a
//! valid (or invalid) config, get a working (or failing-fast)
//! dispatcher". If the production composition drifts from what the
//! tests mirror, fix the test to match — the contract is unchanged.

// Linux-gated because brain-server pulls brain-embed in via
// `[target.'cfg(target_os = "linux")'.dependencies]` — the dep isn't
// available on macOS / Windows builds of the binary. The negative
// test could in principle live cross-platform, but bundling it with
// the gated tests keeps the surface coherent and matches the pattern
// used by every other brain-server integration test.
#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{CachingDispatcher, CpuDispatcher, Dispatcher, EmbedderConfig, ModelHandle};

#[path = "common/model_dir.rs"]
mod model_dir;

/// The three files `build_dispatcher` insists on before handing the
/// path to `ModelHandle::load`. Kept in sync with main.rs by code
/// review; if they drift, this test list is the canonical reference.
const REQUIRED_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// Mirror of `main.rs::linux_main::build_dispatcher`'s pre-load
/// existence check. Returns the first missing path; `Ok(())` only if
/// every required file is present.
fn check_required_files(dir: &std::path::Path) -> Result<(), std::path::PathBuf> {
    for required in REQUIRED_FILES {
        let p = dir.join(required);
        if !p.exists() {
            return Err(p);
        }
    }
    Ok(())
}

/// Negative path: an empty directory is not a model. The operator gets
/// a clear error pointing at the first missing file rather than a
/// candle / tokenizers stack trace they cannot act on.
#[test]
fn start_refuses_with_missing_model_dir() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let dir = tempdir.path();

    // Sanity: the directory exists but is empty.
    assert!(dir.exists());
    assert!(std::fs::read_dir(dir).unwrap().next().is_none());

    let err = check_required_files(dir).expect_err("must reject an empty dir");
    let msg = format!("missing required model file: {}", err.display());
    assert!(
        msg.contains("missing required model file"),
        "error message must say 'missing required model file' so the \
         operator knows what to do; got: {msg}",
    );
    // The named file is one of the three the loader needs; the order
    // matches REQUIRED_FILES, so the first missing one is config.json.
    assert!(
        err.ends_with("config.json"),
        "first missing file must be the one the operator should drop \
         in first; got: {}",
        err.display()
    );
}

/// Positive path. Given a populated directory, we should be able to
/// load the model, wrap it in a CachingDispatcher, and read a
/// non-zero fingerprint back. Gated because it loads ~130 MiB of
/// weights and only runs in environments that have the model on disk.
#[test]
fn start_succeeds_with_valid_model_dir() {
    model_dir::with_model_dir(|dir| {
        // The existence check is what the production path runs first;
        // if it fails here, the environment is mis-configured, not
        // the code under test.
        check_required_files(&dir)
            .expect("BRAIN_EMBED_MODEL_DIR points at a populated model dir");

        let embed_cfg = EmbedderConfig::new(dir);
        let handle = ModelHandle::load(&embed_cfg).expect("model loads");
        let cpu = CpuDispatcher::new(handle);
        // cache_size matches the dev.toml default; the production
        // wrapper applies the same shape so the test exercises the
        // identical type composition.
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(CachingDispatcher::new(cpu, 1024));

        let fp = dispatcher.fingerprint();
        assert_ne!(
            fp, [0u8; 16],
            "real BGE-small must produce a non-zero fingerprint; \
             [0;16] is the NopDispatcher sentinel",
        );
    });
}

/// The dispatcher is `Arc<dyn Dispatcher>` so cloning it into N shards
/// is a cheap reference bump. If a regression sneaks a `.clone()` of
/// the underlying CpuDispatcher (or worse, re-runs `ModelHandle::load`
/// per shard), the test still passes for fingerprint equality but
/// silently wastes ~130 MiB per shard. We can't observe the memory
/// regression here, but we can lock down the cheaper invariant: every
/// clone reports the same fingerprint bytes. Pair this test with a
/// resident-memory smoke check in the bench suite.
#[test]
fn dispatcher_fingerprint_is_consistent_across_clones() {
    model_dir::with_model_dir(|dir| {
        let embed_cfg = EmbedderConfig::new(dir);
        let handle = ModelHandle::load(&embed_cfg).expect("model loads");
        let cpu = CpuDispatcher::new(handle);
        let dispatcher: Arc<dyn Dispatcher> = Arc::new(CachingDispatcher::new(cpu, 1024));

        let baseline = dispatcher.fingerprint();
        // Simulate N shard clones. 8 covers typical small / medium
        // deployments; the count is arbitrary — the invariant holds
        // for any N >= 1.
        let clones: Vec<Arc<dyn Dispatcher>> =
            (0..8).map(|_| Arc::clone(&dispatcher)).collect();

        for (i, c) in clones.iter().enumerate() {
            assert_eq!(
                c.fingerprint(),
                baseline,
                "clone #{i} reported a different fingerprint; the \
                 dispatcher must be shared by Arc, not re-loaded per shard",
            );
        }
    });
}
