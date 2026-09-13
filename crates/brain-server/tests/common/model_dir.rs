//! Shared gating helper for tests that need a real BGE-small model on
//! disk. Each test that depends on the model wraps its body in
//! [`with_model_dir`]. When `BRAIN_EMBED_MODEL_DIR` is unset the
//! closure is skipped with an `eprintln!` note, matching the pattern
//! used by `crates/brain-planner/tests/recall_end_to_end.rs`. The
//! point is to keep `cargo test` fast on developer laptops while still
//! letting CI exercise the real model end-to-end when the env is set
//! by the test runner.

use std::path::PathBuf;

/// Return the model directory path if `BRAIN_EMBED_MODEL_DIR` is set,
/// otherwise `None`. Centralised so individual tests don't re-spell
/// the env var name.
pub fn model_dir() -> Option<PathBuf> {
    std::env::var("BRAIN_EMBED_MODEL_DIR")
        .ok()
        .map(PathBuf::from)
}

/// Run `f` with the resolved model directory when the env var is set;
/// otherwise print a skip note and return. Tests stay quietly green
/// in the default `cargo test` flow but report a clear reason for the
/// skip so "all green" doesn't mask "all skipped".
pub fn with_model_dir<F: FnOnce(PathBuf)>(f: F) {
    match model_dir() {
        Some(dir) => f(dir),
        None => {
            eprintln!(
                "skipping: set BRAIN_EMBED_MODEL_DIR to run \
                 (download with ./.devcontainer/bootstrap-model.sh)"
            );
        }
    }
}
