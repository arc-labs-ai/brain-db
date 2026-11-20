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
use brain_server::config;

use config::{Config, ConfigError, LoggingConfig};

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

# No provider credential: a deployment holds none, and each namespace
# registers its own at runtime. These tests exercise the parser.
[llm]
"#;

// ---------------------------------------------------------------------------
// 1. Round-trip dev.toml
// ---------------------------------------------------------------------------

#[test]
fn dev_toml_round_trips_cleanly() {
    let path = dev_toml_path();
    assert!(path.exists(), "expected dev.toml at {}", path.display());
    // dev.toml carries no provider credential, because there is none to
    // carry: a namespace registers its own at run time.
    let cfg = Config::load_with_env(&path, &HashMap::new()).expect("dev.toml must load");

    assert_eq!(cfg.server.listen_addr.to_string(), "127.0.0.1:9090");
    assert_eq!(cfg.storage.shard_count, Some(1));
    assert_eq!(cfg.shard.arena_capacity_bytes, 1u64 << 30);
    assert_eq!(cfg.shard.wal_segment_size_bytes, 256u64 << 20);
    assert_eq!(cfg.shard.wal_retention_segments, 4);
    assert_eq!(cfg.hnsw.m, 16);
    assert_eq!(cfg.hnsw.ef_construction, 200);
    assert_eq!(cfg.hnsw.ef_search, 64);
    assert_eq!(cfg.embedder.model, "bge-small-en-v1.5");
    assert_eq!(cfg.admin.token.as_deref(), Some("dev-admin-token"));
    assert!(!cfg.server.tls.enabled);
    assert_eq!(cfg.monitoring.logging.format, "json");
}

#[test]
fn dev_toml_boots_without_a_provider_key() {
    // A correct boot carries no provider credential: it belongs to the
    // namespace and is not known until a tenant registers one.
    let path = dev_toml_path();
    let cfg = Config::load_with_env(&path, &HashMap::new())
        .expect("a config must boot without a provider key");
    // The model id is the only LLM setting a deployment supplies.
    let _ = cfg.llm.model;
}

// ---------------------------------------------------------------------------
// 1b. BYOK: per-namespace credential mode relaxes the keyless-boot invariant
// ---------------------------------------------------------------------------

#[test]
fn boots_without_a_provider_key() {
    // The serving credential belongs to the tenant and is not known until a
    // write names its namespace, so a keyless boot is correct.
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    Config::load_with_env(&path, &HashMap::new()).expect("must boot without a provider key");
}

#[test]
fn a_config_that_names_a_provider_key_is_rejected() {
    // There is no deployment-wide credential to set. An operator who tries
    // gets a parse failure naming the field rather than a key that is read
    // by nothing.
    let dir = tempfile::tempdir().unwrap();
    let cfg_toml = MINIMAL_CONFIG.replace("[llm]\n", "[llm]\napi_key = \"sk-nope\"\n");
    let path = write_tmp(&dir, &cfg_toml);
    let err = Config::load_with_env(&path, &HashMap::new())
        .expect_err("`[llm] api_key` must not be accepted");
    assert!(
        err.to_string().contains("api_key"),
        "expected the rejection to name the field, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// 2-5. Parser (covered as unit tests in config.rs). Add one belt-and-suspenders
//      integration check that ShardConfig wires through the deserializer.
// ---------------------------------------------------------------------------

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
    assert_eq!(cfg.storage.shard_count, Some(8));
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

#[test]
fn env_numeric_looking_secret_stays_string() {
    // A pure-digit admin token / API key must be accepted verbatim as a
    // string, not re-typed to an integer (which would fail deserialization
    // and make the server refuse to boot with a confusing "config
    // validation error").
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__ADMIN__TOKEN".into(), "48291057".into());
    env.insert("BRAIN__LLM__CREDENTIALS__KEK".into(), "99999".into());
    let cfg = Config::load_with_env(&path, &env).expect("digit secrets must load as strings");
    assert_eq!(cfg.admin.token.as_deref(), Some("48291057"));
    assert_eq!(cfg.llm.credentials.kek.as_deref(), Some("99999"));
}

#[test]
fn env_bool_looking_secret_stays_string() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__ADMIN__TOKEN".into(), "true".into());
    let cfg = Config::load_with_env(&path, &env).expect("bool-looking secret must load as string");
    assert_eq!(cfg.admin.token.as_deref(), Some("true"));
}

#[test]
fn env_numeric_field_still_accepts_numeric_override() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__STORAGE__SHARD_COUNT".into(), "8".into());
    let cfg = Config::load_with_env(&path, &env).unwrap();
    assert_eq!(cfg.storage.shard_count, Some(8));
}

#[test]
fn env_wrong_typed_numeric_field_still_errors() {
    // A non-numeric value for a genuinely numeric field must still fail
    // fast — the string-preserving fix only covers string-typed targets.
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__STORAGE__SHARD_COUNT".into(), "notanumber".into());
    let err = Config::load_with_env(&path, &env)
        .expect_err("a string for a numeric field must fail deserialization");
    assert!(matches!(err, ConfigError::Validate { .. }), "got: {err:?}");
}

#[test]
fn validate_post_rejects_zero_worker_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert("BRAIN__WORKERS__AUTO_EDGE__INTERVAL_MS".into(), "0".into());
    let err = Config::load_with_env(&path, &env).expect_err("interval_ms = 0 must be rejected");
    assert!(
        matches!(err, ConfigError::Invariant(ref m) if m.contains("interval_ms")),
        "got: {err:?}"
    );
}

#[test]
fn validate_post_rejects_zero_channel_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert(
        "BRAIN__WORKERS__EXTRACTOR__CHANNEL_CAPACITY".into(),
        "0".into(),
    );
    let err =
        Config::load_with_env(&path, &env).expect_err("channel_capacity = 0 must be rejected");
    assert!(
        matches!(err, ConfigError::Invariant(ref m) if m.contains("channel_capacity")),
        "got: {err:?}"
    );
}

#[test]
fn validate_post_rejects_zero_optional_cadence() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_tmp(&dir, MINIMAL_CONFIG);
    let mut env = HashMap::new();
    env.insert(
        "BRAIN__WORKERS__CONSOLIDATION_INTERVAL_SEC".into(),
        "0".into(),
    );
    let err =
        Config::load_with_env(&path, &env).expect_err("consolidation_interval_sec = 0 must reject");
    assert!(
        matches!(err, ConfigError::Invariant(ref m) if m.contains("consolidation_interval_sec")),
        "got: {err:?}"
    );
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
    assert_eq!(cfg.monitoring.logging, LoggingConfig::default());
    assert!(!cfg.monitoring.tracing.enabled);
    assert!(cfg.admin.token.is_none());
    assert!(!cfg.server.tls.enabled);
}

#[test]
fn analytics_timeout_not_below_export_interval_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = format!(
        "{MINIMAL_CONFIG}\n[analytics]\nenabled = true\nexport_interval_secs = 30\nquery_timeout_secs = 30\n"
    );
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invariant(ref m) if m.contains("query_timeout_secs")),
        "got: {err:?}"
    );
}

#[test]
fn analytics_and_aggregates_sections_parse_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let ok = format!(
        "{MINIMAL_CONFIG}\n[analytics]\nenabled = true\n\n[aggregates]\nverify_interval_secs = 0\n"
    );
    let path = write_tmp(&dir, &ok);
    let cfg = Config::load_with_env(&path, &HashMap::new()).unwrap();
    assert!(cfg.analytics.enabled);
    assert_eq!(cfg.analytics.export_interval_secs, 3600);
    assert_eq!(cfg.aggregates.verify_interval_secs, 0);
    assert_eq!(cfg.aggregates.build_batch_rows, 2_000);
}

#[test]
fn zero_aggregate_build_batch_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bad = format!("{MINIMAL_CONFIG}\n[aggregates]\nbuild_batch_rows = 0\n");
    let path = write_tmp(&dir, &bad);
    let err = Config::load_with_env(&path, &HashMap::new()).unwrap_err();
    assert!(
        matches!(err, ConfigError::Invariant(ref m) if m.contains("build_batch_rows")),
        "got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Shard sizing
// ---------------------------------------------------------------------------

#[test]
fn an_unpinned_shard_count_sizes_itself_to_the_machine() {
    // `shard_count` is the operator's intent: `None` means "you decide".
    let mut cfg = Config::for_tests();
    cfg.storage.shard_count = None;
    assert_eq!(cfg.storage.shards(), config::shards_for_this_machine());
}

#[test]
fn a_pinned_shard_count_is_returned_verbatim() {
    let mut cfg = Config::for_tests();
    cfg.storage.shard_count = Some(4);
    assert_eq!(cfg.storage.shards(), 4);
}

#[test]
fn machine_sizing_always_yields_at_least_one_shard() {
    // A single-core box, or a container pinned to one CPU, must still run a
    // shard — the subtraction that reserves a core for the network runtime
    // must not reach zero.
    assert!(config::shards_for_this_machine() >= 1);
}
