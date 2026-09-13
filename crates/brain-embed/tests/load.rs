//! Integration test for `ModelHandle::load`.
//!
//! The full happy path requires a real BGE-small directory on disk
//! (`config.json` + `tokenizer.json` + `model.safetensors`, ~130 MiB).
//! Operators run this opt-in by exporting `BRAIN_EMBED_MODEL_DIR`
//! before `cargo test`. Without the env var, the test is logged and
//! skipped — keeping the default `just test` workflow fast.

use std::path::PathBuf;

use brain_embed::{EmbedderConfig, ModelHandle};

const ENV_VAR: &str = "BRAIN_EMBED_MODEL_DIR";

#[test]
fn load_real_model_or_skip() {
    let Ok(dir) = std::env::var(ENV_VAR) else {
        eprintln!("skipping: set {ENV_VAR} to a BGE-small directory to run this test");
        return;
    };
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "{ENV_VAR}={} is not a directory",
        dir.display()
    );

    let config = EmbedderConfig::new(dir.clone());
    let handle = ModelHandle::load(&config).expect("load should succeed for a real model dir");

    let fp = handle.fingerprint();
    assert_eq!(fp.len(), 16);
    assert!(
        fp.iter().any(|&b| b != 0),
        "fingerprint should not be all-zero"
    );

    let handle2 = ModelHandle::load(&config).expect("second load");
    assert_eq!(
        handle.fingerprint(),
        handle2.fingerprint(),
        "fingerprint must be deterministic for the same model directory"
    );
}
