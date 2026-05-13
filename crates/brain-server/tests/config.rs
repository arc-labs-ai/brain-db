//! Integration tests for `brain-server::config`.
//!
//! Tests round-trip the real `config/dev.toml`, exercise validation rules,
//! and exercise env overrides. Env overrides are tested with the explicit
//! `load_with_env(map)` form so we don't mutate the global process env.

use std::collections::HashMap;
use std::path::PathBuf;

// Re-export the binary's internal modules under a regular crate path.
// brain-server is a [[bin]] target without a library, so we pull the
// config module in as source via a small inline crate stub.
#[path = "../src/config/mod.rs"]
mod config;

use config::{AuthMode, Config, ConfigError, LoggingConfig};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Path to the workspace's checked-in `config/dev.toml`. Resolved from the
/// crate manifest dir so the test is location-stable.
fn dev_toml_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("config")
        .join("dev.toml")
}

/// Write a TOML snippet to a temp file and return the path. The caller keeps
/// the `tempfile::TempDir` alive for the lifetime of the test.
fn write_tmp(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
    let p = dir.path().join("test.toml");
    std::fs::write(&p, contents).expect("write temp config");
    p
}

const MINIMAL_CONFIG: &str = r#"
[server]
listen_addr = "127.0.0.1:9090"
metrics_addr = "127.0.0.1:9091"
admin_addr   = "127.0.0.1:9092"

[storage]
data_dir = "./data"
shard_count = 4

[shard]
arena_capacity_bytes  = "1GiB"
wal_segment_size_bytes = "256MiB"
wal_retention_segments = 4

[hnsw]
m = 16
ef_construction = 200
ef_search = 64

[embedder]
model = "bge-small-en-v1.5"
cache_size = 10000
batch_size = 32
batch_window_ms = 5
"#;

// ---------------------------------------------------------------------------
// 1. Round-trip dev.toml
// ---------------------------------------------------------------------------

#[test]
fn dev_toml_round_trips_cleanly() {
    let path = dev_toml_path();
    assert!(path.exists(), "expected dev.toml at {}", path.display());
    let env: HashMap<String, String> = HashMap::new();
    let cfg = Config::load_with_env(&path, &env).expect("dev.toml must load");

    assert_eq!(cfg.server.listen_addr.to_string(), "127.0.0.1:9090");
    assert_eq!(cfg.storage.shard_count, 4);
    assert_eq!(cfg.shard.arena_capacity_bytes, 1u64 << 30);
    assert_eq!(cfg.shard.wal_segment_size_bytes, 256u64 << 20);
    assert_eq!(cfg.shard.wal_retention_segments, 4);
    assert_eq!(cfg.hnsw.m, 16);
    assert_eq!(cfg.hnsw.ef_construction, 200);
    assert_eq!(cfg.hnsw.ef_search, 64);
    assert_eq!(cfg.embedder.model, "bge-small-en-v1.5");
    assert_eq!(cfg.auth.mode, AuthMode::None);
    assert!(!cfg.server.tls.enabled);
    assert_eq!(cfg.logging.format, "json");
}

// ---------------------------------------------------------------------------
// 2-5. Parser (covered as unit tests in config.rs). Add one belt-and-suspenders
//      integration check that ShardConfig wires through the deserializer.
// ---------------------------------------------------------------------------

#[test]
fn shard_config_parses_human_byte_strings() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let cfg = Config::load_with_env(&path, &HashMap::new()).unwrap();
    assert_eq!(cfg.shard.arena_capacity_bytes, 1u64 << 30);
    assert_eq!(cfg.shard.wal_segment_size_bytes, 256u64 << 20);
}

#[test]
fn shard_config_rejects_bad_byte_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let bad = MINIMAL_CONFIG.replace("\"1GiB\"", "\"1XYZ\"");
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("XYZ") || msg.contains("byte suffix"),
        "expected byte-suffix complaint, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 6-8. Validation
// ---------------------------------------------------------------------------

#[test]
fn missing_required_section_errors() {
    let dir = tempfile::tempdir().unwrap();
    // Drop [server] entirely.
    let bad = MINIMAL_CONFIG.replace(
        "[server]\nlisten_addr = \"127.0.0.1:9090\"\nmetrics_addr = \"127.0.0.1:9091\"\nadmin_addr   = \"127.0.0.1:9092\"\n",
        "",
    );
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(matches!(err, ConfigError::Validate { .. }));
    assert!(
        err.to_string().contains("server"),
        "expected error to mention `server`, got: {err}"
    );
}

#[test]
fn unknown_field_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = MINIMAL_CONFIG.replace(
        "admin_addr   = \"127.0.0.1:9092\"\n",
        "admin_addr   = \"127.0.0.1:9092\"\nmystery_field = 42\n",
    );
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(matches!(err, ConfigError::Validate { .. }));
    assert!(
        err.to_string().contains("mystery_field"),
        "expected error to mention `mystery_field`, got: {err}"
    );
}

#[test]
fn tls_enabled_without_cert_or_key_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = MINIMAL_CONFIG.replace(
        "admin_addr   = \"127.0.0.1:9092\"\n",
        "admin_addr   = \"127.0.0.1:9092\"\n\n[server.tls]\nenabled = true\n",
    );
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(matches!(err, ConfigError::Invariant(_)), "got: {err:?}");
    assert!(err.to_string().contains("tls"));
}

#[test]
fn zero_shard_count_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = MINIMAL_CONFIG.replace("shard_count = 4", "shard_count = 0");
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(matches!(err, ConfigError::Invariant(_)), "got: {err:?}");
}

#[test]
fn hnsw_ef_construction_below_m_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = MINIMAL_CONFIG.replace("ef_construction = 200", "ef_construction = 4");
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(matches!(err, ConfigError::Invariant(_)), "got: {err:?}");
}

// ---------------------------------------------------------------------------
// 9-11. Env overrides
// ---------------------------------------------------------------------------

#[test]
fn env_override_replaces_string_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__SERVER__LISTEN_ADDR".into(), "0.0.0.0:8080".into());
    let cfg = Config::load_with_env(&path, &env).unwrap();
    assert_eq!(cfg.server.listen_addr.to_string(), "0.0.0.0:8080");
}

#[test]
fn env_override_replaces_integer_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__STORAGE__SHARD_COUNT".into(), "8".into());
    let cfg = Config::load_with_env(&path, &env).unwrap();
    assert_eq!(cfg.storage.shard_count, 8);
}

#[test]
fn env_override_inserts_nested_path() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("server.crt");
    let key = dir.path().join("server.key");
    std::fs::write(&cert, "stub").unwrap();
    std::fs::write(&key, "stub").unwrap();

    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__SERVER__TLS__ENABLED".into(), "true".into());
    env.insert(
        "BRAIN__SERVER__TLS__CERT".into(),
        cert.to_string_lossy().into_owned(),
    );
    env.insert(
        "BRAIN__SERVER__TLS__KEY".into(),
        key.to_string_lossy().into_owned(),
    );
    let cfg = Config::load_with_env(&path, &env).unwrap();
    assert!(cfg.server.tls.enabled);
    assert_eq!(cfg.server.tls.cert.as_deref(), Some(cert.as_path()));
    assert_eq!(cfg.server.tls.key.as_deref(), Some(key.as_path()));
}

#[test]
fn env_override_byte_size_string_parses() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__SHARD__ARENA_CAPACITY_BYTES".into(), "2GiB".into());
    let cfg = Config::load_with_env(&path, &env).unwrap();
    assert_eq!(cfg.shard.arena_capacity_bytes, 2u64 << 30);
}

// ---------------------------------------------------------------------------
// 12. Defaults
// ---------------------------------------------------------------------------

#[test]
fn omitted_optional_sections_use_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let cfg = Config::load_with_env(&path, &HashMap::new()).unwrap();

    assert_eq!(cfg.workers, Default::default());
    assert_eq!(cfg.logging, LoggingConfig::default());
    assert!(!cfg.tracing.enabled);
    assert_eq!(cfg.auth.mode, AuthMode::None);
    assert!(!cfg.server.tls.enabled);
}
