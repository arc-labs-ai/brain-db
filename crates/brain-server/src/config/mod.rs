//! Typed configuration for `brain-server`.
//!
//! Round-trips `config/dev.toml`. Env overrides follow the
//! `BRAIN__SECTION__FIELD=value` pattern (double underscore separates nesting).
//!
//! See spec §01/04 §15 — config is restart-only for v1 (no hot reload).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

// ----------------------------------------------------------------------------
// Top-level Config
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub shard: ShardConfig,
    pub hnsw: HnswConfig,
    pub embedder: EmbedderConfig,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub tracing: TracingConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    /// Sub-task 9.15. Defaulted so existing `dev.toml` files keep
    /// working (consolidation remains disabled until an LLM backend
    /// is wired).
    #[serde(default)]
    pub summarizer: SummarizerConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    #[serde(default)]
    pub tls: TlsConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cert: Option<PathBuf>,
    #[serde(default)]
    pub key: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub shard_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShardConfig {
    #[serde(deserialize_with = "deserialize_human_bytes")]
    pub arena_capacity_bytes: u64,
    #[serde(deserialize_with = "deserialize_human_bytes")]
    pub wal_segment_size_bytes: u64,
    pub wal_retention_segments: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HnswConfig {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmbedderConfig {
    pub model: String,
    pub cache_size: usize,
    pub batch_size: usize,
    pub batch_window_ms: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    pub decay_interval_sec: Option<u64>,
    pub consolidation_interval_sec: Option<u64>,
    pub hnsw_maintenance_interval_sec: Option<u64>,
    pub idempotency_cleanup_interval_sec: Option<u64>,
    pub slot_reclamation_interval_sec: Option<u64>,
    pub wal_retention_interval_sec: Option<u64>,
    pub edge_scrub_interval_sec: Option<u64>,
    pub counter_reconciliation_interval_sec: Option<u64>,
    pub statistics_update_interval_sec: Option<u64>,
    pub embedder_cache_eviction_interval_sec: Option<u64>,
    pub snapshot_interval_sec: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub output: String,
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            output: "stdout".into(),
            format: "compact".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TracingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sampler: String,
    #[serde(default)]
    pub sample_ratio: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub mode: AuthMode,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    None,
    ApiKey,
}

// ----------------------------------------------------------------------------
// Summarizer (sub-task 9.15)
// ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SummarizerBackend {
    /// Spec §11/03 §6 default: consolidation worker is a no-op.
    Disabled,
    /// Chat Completions over HTTPS. Requires `summarizer-openai` feature.
    Openai,
    /// Ollama's `/api/generate` over plain HTTP. Requires
    /// `summarizer-ollama` feature.
    Ollama,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SummarizerConfig {
    /// Which backend to use. Defaults to `disabled`.
    #[serde(default = "default_backend")]
    pub backend: SummarizerBackend,
    /// HTTP request timeout (seconds). Applied to every LLM round-trip.
    #[serde(default = "default_request_timeout_sec")]
    pub request_timeout_sec: u32,
    /// Soft cap on summary length. Translates to roughly
    /// `max_summary_chars / 4` tokens on the OpenAI side. Ollama
    /// ignores it (no spec-level token cap).
    #[serde(default = "default_max_summary_chars")]
    pub max_summary_chars: u32,

    // OpenAI-specific knobs. Read only when `backend == Openai`.
    #[serde(default = "default_openai_api_base")]
    pub openai_api_base: String,
    /// Name of the env var holding the API key. We never store the
    /// key itself in TOML — operators set the env var.
    #[serde(default)]
    pub openai_api_key_env: Option<String>,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_temperature")]
    pub openai_temperature: f32,

    // Ollama-specific knobs.
    #[serde(default = "default_ollama_base")]
    pub ollama_base: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            request_timeout_sec: default_request_timeout_sec(),
            max_summary_chars: default_max_summary_chars(),
            openai_api_base: default_openai_api_base(),
            openai_api_key_env: None,
            openai_model: default_openai_model(),
            openai_temperature: default_temperature(),
            ollama_base: default_ollama_base(),
            ollama_model: default_ollama_model(),
        }
    }
}

fn default_backend() -> SummarizerBackend {
    SummarizerBackend::Disabled
}
fn default_request_timeout_sec() -> u32 {
    30
}
fn default_max_summary_chars() -> u32 {
    4096
}
fn default_openai_api_base() -> String {
    "https://api.openai.com/v1".to_owned()
}
fn default_openai_model() -> String {
    "gpt-4o-mini".to_owned()
}
fn default_temperature() -> f32 {
    0.3
}
fn default_ollama_base() -> String {
    "http://localhost:11434".to_owned()
}
fn default_ollama_model() -> String {
    "llama3.1:8b".to_owned()
}

// ----------------------------------------------------------------------------
// Errors
// ----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found or unreadable at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config TOML parse error at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config validation error at {path}: {source}")]
    Validate {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("bad byte suffix: '{0}' (expected one of: KiB, MiB, GiB, TiB, KB, MB, GB, B, or bare digits)")]
    BadByteSuffix(String),

    #[error("byte value '{0}' is not a valid unsigned integer")]
    BadByteDigits(String),

    #[error("byte value overflows u64")]
    Overflow,

    #[error("invalid config: {0}")]
    Invariant(String),
}

// ----------------------------------------------------------------------------
// human_bytes
// ----------------------------------------------------------------------------

fn deserialize_human_bytes<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    parse_human_bytes(&s).map_err(serde::de::Error::custom)
}

/// Parse a byte size like "1GiB", "256MiB", "1024", "1MB".
pub fn parse_human_bytes(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ConfigError::BadByteDigits(String::new()));
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, suffix) = s.split_at(split);
    if digits.is_empty() {
        return Err(ConfigError::BadByteDigits(s.to_owned()));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| ConfigError::BadByteDigits(digits.to_owned()))?;
    let mult: u64 = match suffix.trim() {
        "" | "B" => 1,
        "KiB" => 1u64 << 10,
        "MiB" => 1u64 << 20,
        "GiB" => 1u64 << 30,
        "TiB" => 1u64 << 40,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        other => return Err(ConfigError::BadByteSuffix(other.to_owned())),
    };
    n.checked_mul(mult).ok_or(ConfigError::Overflow)
}

// ----------------------------------------------------------------------------
// Env overrides
// ----------------------------------------------------------------------------

/// Apply env-style overrides to a parsed TOML value.
///
/// Each `(key, value)` whose key starts with `BRAIN__` is interpreted as a
/// double-underscore-separated path into the TOML tree. Each path component
/// is lowercased to match TOML field names. The leaf string is heuristically
/// re-typed (bool / integer / float / string) so the subsequent `serde`
/// deserialize sees a value of the expected primitive type.
fn apply_env_overrides(value: &mut toml::Value, env: &HashMap<String, String>) {
    for (key, val) in env {
        let Some(suffix) = key.strip_prefix("BRAIN__") else {
            continue;
        };
        let path: Vec<String> = suffix.split("__").map(|s| s.to_ascii_lowercase()).collect();
        if path.is_empty() || path.iter().any(String::is_empty) {
            continue;
        }
        set_path(value, &path, val);
    }
}

/// Coerce a raw env-var string to the most specific TOML scalar that fits.
/// Fields that look like byte-size strings (e.g. "2GiB") fall through to
/// `String`, which is exactly what `deserialize_human_bytes` expects.
fn coerce_leaf(raw: &str) -> toml::Value {
    match raw {
        "true" => return toml::Value::Boolean(true),
        "false" => return toml::Value::Boolean(false),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    // Only treat as float if it contains a decimal point or scientific 'e'.
    if (raw.contains('.') || raw.contains('e') || raw.contains('E')) && raw.parse::<f64>().is_ok() {
        return toml::Value::Float(raw.parse::<f64>().expect("just checked"));
    }
    toml::Value::String(raw.to_owned())
}

fn set_path(value: &mut toml::Value, path: &[String], leaf: &str) {
    if path.is_empty() {
        return;
    }
    let mut cursor = value;
    for segment in &path[..path.len() - 1] {
        if !matches!(cursor, toml::Value::Table(_)) {
            *cursor = toml::Value::Table(toml::value::Table::new());
        }
        let toml::Value::Table(table) = cursor else {
            unreachable!("ensured above");
        };
        cursor = table
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    }
    if !matches!(cursor, toml::Value::Table(_)) {
        *cursor = toml::Value::Table(toml::value::Table::new());
    }
    let toml::Value::Table(table) = cursor else {
        unreachable!("ensured above");
    };
    let leaf_key = path.last().expect("path non-empty").clone();
    table.insert(leaf_key, coerce_leaf(leaf));
}

// ----------------------------------------------------------------------------
// load
// ----------------------------------------------------------------------------

impl Config {
    /// Read config from disk and apply env overrides from the live process env.
    #[allow(dead_code)] // used by main.rs; tests use load_with_env to avoid global env mutation.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::load_with_env(path, &env)
    }

    /// Read config from disk, applying overrides from `env` instead of the
    /// global process environment. Used by tests to avoid global env mutation.
    pub fn load_with_env(
        path: impl AsRef<Path>,
        env: &HashMap<String, String>,
    ) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let mut value: toml::Value = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        apply_env_overrides(&mut value, env);
        let cfg: Config = value.try_into().map_err(|source| ConfigError::Validate {
            path: path.to_owned(),
            source,
        })?;
        cfg.validate_post()?;
        Ok(cfg)
    }

    /// Minimal valid `Config` used by integration tests that need to
    /// construct an [`AdminState`](crate::admin::AdminState) without
    /// reading a TOML file. Numeric defaults mirror `config/dev.toml`.
    #[doc(hidden)]
    #[allow(dead_code)] // referenced by integration tests via #[path] mounts.
    pub fn for_tests() -> Self {
        Self {
            server: ServerConfig {
                listen_addr: "127.0.0.1:0".parse().expect("addr"),
                metrics_addr: "127.0.0.1:0".parse().expect("addr"),
                admin_addr: "127.0.0.1:0".parse().expect("addr"),
                tls: TlsConfig::default(),
            },
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/brain-test"),
                shard_count: 1,
            },
            shard: ShardConfig {
                arena_capacity_bytes: 1u64 << 20,
                wal_segment_size_bytes: 1u64 << 20,
                wal_retention_segments: 4,
            },
            hnsw: HnswConfig {
                m: 16,
                ef_construction: 64,
                ef_search: 64,
            },
            embedder: EmbedderConfig {
                model: "test".into(),
                cache_size: 1,
                batch_size: 1,
                batch_window_ms: 1,
            },
            workers: WorkersConfig::default(),
            logging: LoggingConfig::default(),
            tracing: TracingConfig::default(),
            auth: AuthConfig::default(),
            summarizer: SummarizerConfig::default(),
        }
    }

    fn validate_post(&self) -> Result<(), ConfigError> {
        if self.storage.shard_count == 0 {
            return Err(ConfigError::Invariant(
                "storage.shard_count must be >= 1".into(),
            ));
        }
        if self.embedder.batch_size == 0 {
            return Err(ConfigError::Invariant(
                "embedder.batch_size must be >= 1".into(),
            ));
        }
        if self.embedder.cache_size == 0 {
            return Err(ConfigError::Invariant(
                "embedder.cache_size must be >= 1".into(),
            ));
        }
        if self.hnsw.m < 2 {
            return Err(ConfigError::Invariant("hnsw.m must be >= 2".into()));
        }
        if self.hnsw.ef_construction < self.hnsw.m {
            return Err(ConfigError::Invariant(
                "hnsw.ef_construction must be >= hnsw.m".into(),
            ));
        }
        if self.hnsw.ef_search == 0 {
            return Err(ConfigError::Invariant("hnsw.ef_search must be >= 1".into()));
        }
        if self.shard.arena_capacity_bytes == 0 {
            return Err(ConfigError::Invariant(
                "shard.arena_capacity_bytes must be > 0".into(),
            ));
        }
        if self.shard.wal_segment_size_bytes == 0 {
            return Err(ConfigError::Invariant(
                "shard.wal_segment_size_bytes must be > 0".into(),
            ));
        }
        if self.shard.wal_retention_segments == 0 {
            return Err(ConfigError::Invariant(
                "shard.wal_retention_segments must be >= 1".into(),
            ));
        }
        if self.server.tls.enabled
            && (self.server.tls.cert.is_none() || self.server.tls.key.is_none())
        {
            return Err(ConfigError::Invariant(
                "server.tls.enabled = true requires both server.tls.cert and server.tls.key".into(),
            ));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Unit tests (env-override + parser primitives). Integration round-trip lives
// in `tests/config.rs`.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_binary_units() {
        assert_eq!(parse_human_bytes("1GiB").unwrap(), 1u64 << 30);
        assert_eq!(parse_human_bytes("256MiB").unwrap(), 256 * (1u64 << 20));
        assert_eq!(parse_human_bytes("4KiB").unwrap(), 4096);
        assert_eq!(parse_human_bytes("1TiB").unwrap(), 1u64 << 40);
    }

    #[test]
    fn human_bytes_decimal_units() {
        assert_eq!(parse_human_bytes("1KB").unwrap(), 1_000);
        assert_eq!(parse_human_bytes("1MB").unwrap(), 1_000_000);
        assert_eq!(parse_human_bytes("2GB").unwrap(), 2_000_000_000);
    }

    #[test]
    fn human_bytes_bare_number() {
        assert_eq!(parse_human_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_human_bytes("1024B").unwrap(), 1024);
        assert_eq!(parse_human_bytes("0").unwrap(), 0);
    }

    #[test]
    fn human_bytes_rejects_bad_suffix() {
        assert!(matches!(
            parse_human_bytes("1XYZ"),
            Err(ConfigError::BadByteSuffix(_))
        ));
        assert!(matches!(
            parse_human_bytes("100gigs"),
            Err(ConfigError::BadByteSuffix(_))
        ));
    }

    #[test]
    fn human_bytes_rejects_missing_digits() {
        assert!(matches!(
            parse_human_bytes("GiB"),
            Err(ConfigError::BadByteDigits(_))
        ));
        assert!(matches!(
            parse_human_bytes(""),
            Err(ConfigError::BadByteDigits(_))
        ));
    }

    #[test]
    fn coerce_leaf_handles_primitive_types() {
        assert_eq!(coerce_leaf("true"), toml::Value::Boolean(true));
        assert_eq!(coerce_leaf("false"), toml::Value::Boolean(false));
        assert_eq!(coerce_leaf("42"), toml::Value::Integer(42));
        assert_eq!(coerce_leaf("-7"), toml::Value::Integer(-7));
        assert_eq!(coerce_leaf("0.5"), toml::Value::Float(0.5));
        assert_eq!(coerce_leaf("1e3"), toml::Value::Float(1000.0));
        // Strings that aren't numeric or bool keep type String — including
        // byte-size syntax that human_bytes will parse downstream.
        assert_eq!(coerce_leaf("2GiB"), toml::Value::String("2GiB".into()));
        assert_eq!(
            coerce_leaf("127.0.0.1:9090"),
            toml::Value::String("127.0.0.1:9090".into())
        );
    }

    #[test]
    fn set_path_replaces_existing_scalar() {
        let mut value: toml::Value = toml::from_str("[a]\nb = 1\n").unwrap();
        set_path(&mut value, &["a".into(), "b".into()], "0.0.0.0:8080");
        let s = value["a"]["b"].as_str().unwrap();
        assert_eq!(s, "0.0.0.0:8080");
    }

    #[test]
    fn set_path_inserts_into_missing_section() {
        let mut value: toml::Value = toml::from_str("[a]\nb = 1\n").unwrap();
        set_path(&mut value, &["c".into(), "d".into(), "e".into()], "true");
        // "true" coerces to a Boolean.
        assert!(value["c"]["d"]["e"].as_bool().unwrap());
    }
}
