//! Typed configuration for `brain-server`.
//!
//! Round-trips `config/dev.toml`. Env overrides follow the
//! `BRAIN__SECTION__FIELD=value` pattern (double underscore separates nesting).
//!
//! — config is restart-only for v1 (no hot reload).

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

impl EmbedderConfig {
    /// Resolve the on-disk directory containing the embedding model
    /// files (`config.json`, `tokenizer.json`, `model.safetensors`).
    ///
    /// Priority cascade:
    ///   1. `BRAIN_EMBED_MODEL_DIR` env var if set (must be absolute).
    ///   2. `self.model` if it starts with `/` (absolute path literal).
    ///   3. `$XDG_DATA_HOME/brain/models/<self.model>` (XDG default).
    ///   4. `~/.local/share/brain/models/<self.model>` (fallback).
    ///
    /// Path resolution only; existence checks belong to the loader. The
    /// env-override slot lets devs point at any local checkout without
    /// editing TOML; the XDG default keeps the no-config first-run
    /// experience clean.
    #[allow(dead_code)] // consumed by main.rs on Linux; unused on non-Linux stub builds.
    pub fn resolve_model_dir(&self) -> Result<PathBuf, ConfigError> {
        self.resolve_model_dir_with(&|k| std::env::var(k).ok())
    }

    /// Same as [`resolve_model_dir`] but reads env via the supplied
    /// closure so tests can drive the cascade without touching the
    /// global process environment.
    pub fn resolve_model_dir_with<F>(&self, env: &F) -> Result<PathBuf, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(env_path) = env("BRAIN_EMBED_MODEL_DIR") {
            let p = PathBuf::from(env_path);
            if !p.is_absolute() {
                return Err(ConfigError::Invariant(format!(
                    "BRAIN_EMBED_MODEL_DIR must be an absolute path; got {}",
                    p.display()
                )));
            }
            return Ok(p);
        }
        let model = &self.model;
        if model.starts_with('/') {
            return Ok(PathBuf::from(model));
        }
        if let Some(xdg) = env("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg).join("brain").join("models").join(model));
        }
        if let Some(home) = env("HOME") {
            return Ok(PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("brain")
                .join("models")
                .join(model));
        }
        Err(ConfigError::Invariant(format!(
            "cannot resolve model directory for '{model}': set BRAIN_EMBED_MODEL_DIR or HOME",
        )))
    }
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
    /// Phase B: substrate auto-derived `SimilarTo` edges. Defaults
    /// kick in when the section is omitted from TOML.
    #[serde(default)]
    pub auto_edge: AutoEdgeWorkerConfig,
    /// Phase E: per-shard extractor pipeline worker. Drains the
    /// writer's post-encode channel and runs the three-tier
    /// extractor framework (pattern + classifier + LLM) before
    /// writing entities / statements / relations / mention edges.
    /// Section may be omitted; every field has a default.
    #[serde(default)]
    pub extractor: ExtractorWorkerConfig,
    /// Phase T: substrate auto-derived `FollowedBy` edges keyed on
    /// per-agent temporal adjacency. Defaults kick in when the
    /// section is omitted from TOML.
    #[serde(default)]
    pub temporal_edge: TemporalEdgeWorkerConfig,
    /// Phase C: substrate auto-derived `Caused` edges, sourced from
    /// extractor-asserted causal statements (`brain:caused_by` etc).
    /// No-schema deployments resolve an empty whitelist and the
    /// worker no-ops; setting `enabled = false` skips registration
    /// entirely.
    #[serde(default)]
    pub causal_edge: CausalEdgeWorkerConfig,
}

/// `[workers.auto_edge]` TOML section. Controls the substrate
/// SimilarTo derivation worker (Phase B). Every field defaults so an
/// existing `dev.toml` keeps working without edits.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AutoEdgeWorkerConfig {
    /// Master switch. `false` skips registration entirely — encodes
    /// see a None sender, the worker isn't spawned, the channel is
    /// never created. Latency-sensitive deployments that don't want
    /// auto-derived edges should flip this to `false`.
    #[serde(default = "default_auto_edge_enabled")]
    pub enabled: bool,
    /// Scheduler tick in milliseconds. Smaller = faster encode → edge
    /// visibility; larger = less worker overhead.
    #[serde(default = "default_auto_edge_interval_ms")]
    pub interval_ms: u64,
    /// Max memories drained per cycle. Caps HNSW search bursts.
    #[serde(default = "default_auto_edge_batch_size")]
    pub batch_size: usize,
    /// Cosine similarity floor. Neighbours below this don't get an
    /// edge even if HNSW returned them.
    #[serde(default = "default_auto_edge_similarity_threshold")]
    pub similarity_threshold: f32,
    /// Per-memory neighbour count. The worker fetches `top_k + 1`
    /// from HNSW so the self-hit doesn't eat into the requested k.
    #[serde(default = "default_auto_edge_top_k")]
    pub top_k: usize,
    /// HNSW `ef` override for the per-encode search.
    #[serde(default = "default_auto_edge_ef_search")]
    pub ef_search: usize,
    /// Writer→worker queue depth. On overflow the writer drops the
    /// enqueue with a warn; the encode itself never fails.
    #[serde(default = "default_auto_edge_channel_capacity")]
    pub channel_capacity: usize,
}

impl Default for AutoEdgeWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: default_auto_edge_enabled(),
            interval_ms: default_auto_edge_interval_ms(),
            batch_size: default_auto_edge_batch_size(),
            similarity_threshold: default_auto_edge_similarity_threshold(),
            top_k: default_auto_edge_top_k(),
            ef_search: default_auto_edge_ef_search(),
            channel_capacity: default_auto_edge_channel_capacity(),
        }
    }
}

fn default_auto_edge_enabled() -> bool {
    true
}
fn default_auto_edge_interval_ms() -> u64 {
    100
}
fn default_auto_edge_batch_size() -> usize {
    256
}
fn default_auto_edge_similarity_threshold() -> f32 {
    // Reads `BRAIN_AUTO_EDGE_THRESHOLD` at startup so operators can
    // tune the cosine-similarity floor without re-rolling the config.
    // The crate default is 0.75 (topical-cluster floor), tunable up
    // to 0.85+ for strict deduping.
    brain_workers::resolved_auto_edge_threshold(
        brain_workers::DEFAULT_AUTO_EDGE_SIMILARITY_THRESHOLD,
    )
}
fn default_auto_edge_top_k() -> usize {
    5
}
fn default_auto_edge_ef_search() -> usize {
    64
}
fn default_auto_edge_channel_capacity() -> usize {
    1024
}

/// `[workers.temporal_edge]` TOML section. Controls the substrate
/// `FollowedBy` derivation worker (sub-task T). Every field defaults
/// so an existing `dev.toml` keeps working without edits.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TemporalEdgeWorkerConfig {
    /// Master switch. `false` skips registration entirely.
    #[serde(default = "default_temporal_edge_enabled")]
    pub enabled: bool,
    /// Scheduler tick in milliseconds. Smaller → faster encode →
    /// edge visibility; larger → less worker CPU.
    #[serde(default = "default_temporal_edge_interval_ms")]
    pub interval_ms: u64,
    /// Max memories drained per cycle.
    #[serde(default = "default_temporal_edge_batch_size")]
    pub batch_size: usize,
    /// Temporal window in seconds. Memories older than this are not
    /// candidates for predecessor lookup.
    #[serde(default = "default_temporal_edge_window_seconds")]
    pub window_seconds: u64,
    /// Hard floor on the decay-weight curve. Below this, no edge is
    /// written even if the gap is within the window.
    #[serde(default = "default_temporal_edge_weight_min")]
    pub weight_min: f32,
    /// Writer → worker queue depth.
    #[serde(default = "default_temporal_edge_channel_capacity")]
    pub channel_capacity: usize,
    /// Allow `FollowedBy` edges across context boundaries.
    #[serde(default = "default_temporal_edge_cross_context")]
    pub cross_context: bool,
    /// Cosine similarity floor for the topical gate. Below this, the
    /// candidate predecessor is dropped — preserves narrative threads
    /// without writing spurious "followed by" edges between
    /// topically-unrelated memories ("I had lunch" → "deployed to
    /// prod"). The default reads `BRAIN_TEMPORAL_EDGE_TOPICAL_THRESHOLD`
    /// at startup so operators can tune without re-rolling configs.
    #[serde(default = "default_temporal_edge_topical_threshold")]
    pub topical_threshold: f32,
}

impl Default for TemporalEdgeWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: default_temporal_edge_enabled(),
            interval_ms: default_temporal_edge_interval_ms(),
            batch_size: default_temporal_edge_batch_size(),
            window_seconds: default_temporal_edge_window_seconds(),
            weight_min: default_temporal_edge_weight_min(),
            channel_capacity: default_temporal_edge_channel_capacity(),
            cross_context: default_temporal_edge_cross_context(),
            topical_threshold: default_temporal_edge_topical_threshold(),
        }
    }
}

fn default_temporal_edge_enabled() -> bool {
    true
}
fn default_temporal_edge_interval_ms() -> u64 {
    100
}
fn default_temporal_edge_batch_size() -> usize {
    256
}
fn default_temporal_edge_window_seconds() -> u64 {
    300
}
fn default_temporal_edge_weight_min() -> f32 {
    0.1
}
fn default_temporal_edge_channel_capacity() -> usize {
    1024
}
fn default_temporal_edge_cross_context() -> bool {
    false
}
fn default_temporal_edge_topical_threshold() -> f32 {
    brain_workers::resolved_topical_threshold(
        brain_workers::DEFAULT_TEMPORAL_EDGE_TOPICAL_THRESHOLD,
    )
}

/// `[workers.causal_edge]` TOML section. Controls extractor-driven
/// `Caused` derivation. Every field defaults so an existing `dev.toml`
/// keeps working without edits.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CausalEdgeWorkerConfig {
    /// Master switch. `false` skips registration entirely.
    #[serde(default = "default_causal_edge_enabled")]
    pub enabled: bool,
    /// Scheduler tick in milliseconds.
    #[serde(default = "default_causal_edge_interval_ms")]
    pub interval_ms: u64,
    /// Max statements drained per cycle.
    #[serde(default = "default_causal_edge_batch_size")]
    pub batch_size: usize,
    /// Minimum statement confidence. Below this, no causal edge —
    /// inferring causality at low confidence produces more noise than
    /// signal.
    #[serde(default = "default_causal_edge_min_confidence")]
    pub min_confidence: f32,
    /// Predicate qnames whose presence triggers causal derivation.
    /// Each entry is `"namespace:name"`. No-schema deployments
    /// inherit the brain defaults but resolve to an empty set against
    /// their predicate table.
    #[serde(default = "default_causal_edge_whitelist")]
    pub whitelist_qnames: Vec<String>,
    /// Per-statement cap on effect-side memories drawn from the
    /// causal statement's own evidence.
    #[serde(default = "default_causal_edge_max_effect_memories")]
    pub max_effect_memories_per_statement: usize,
    /// Per-related-statement cap on cause-side memories drawn from
    /// the object entity's statement evidence.
    #[serde(default = "default_causal_edge_max_cause_memories")]
    pub max_cause_memories_per_statement: usize,
    /// Cap on related statements walked back from the cause-side
    /// entity. Net per causal statement: max_effect × max_cause ×
    /// max_related edges.
    #[serde(default = "default_causal_edge_max_related_statements")]
    pub max_related_statements_per_entity: usize,
    /// Extractor → worker queue depth.
    #[serde(default = "default_causal_edge_channel_capacity")]
    pub channel_capacity: usize,
}

impl Default for CausalEdgeWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: default_causal_edge_enabled(),
            interval_ms: default_causal_edge_interval_ms(),
            batch_size: default_causal_edge_batch_size(),
            min_confidence: default_causal_edge_min_confidence(),
            whitelist_qnames: default_causal_edge_whitelist(),
            max_effect_memories_per_statement: default_causal_edge_max_effect_memories(),
            max_cause_memories_per_statement: default_causal_edge_max_cause_memories(),
            max_related_statements_per_entity: default_causal_edge_max_related_statements(),
            channel_capacity: default_causal_edge_channel_capacity(),
        }
    }
}

fn default_causal_edge_enabled() -> bool {
    true
}
fn default_causal_edge_interval_ms() -> u64 {
    200
}
fn default_causal_edge_batch_size() -> usize {
    64
}
fn default_causal_edge_min_confidence() -> f32 {
    0.6
}
fn default_causal_edge_whitelist() -> Vec<String> {
    brain_workers::DEFAULT_WHITELIST_QNAMES
        .iter()
        .map(|(ns, name)| format!("{ns}:{name}"))
        .collect()
}
fn default_causal_edge_max_effect_memories() -> usize {
    brain_workers::DEFAULT_MAX_EFFECT_MEMORIES
}
fn default_causal_edge_max_cause_memories() -> usize {
    brain_workers::DEFAULT_MAX_CAUSE_MEMORIES
}
fn default_causal_edge_max_related_statements() -> usize {
    brain_workers::DEFAULT_MAX_RELATED_STATEMENTS
}
fn default_causal_edge_channel_capacity() -> usize {
    1024
}

/// `[workers.extractor]` TOML section. Defaults registered every
/// shard. Omit the section to accept defaults; set `enabled = false`
/// to skip worker registration entirely for no-schema deployments.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtractorWorkerConfig {
    /// Master switch. `false` skips registration entirely.
    #[serde(default = "default_extractor_enabled")]
    pub enabled: bool,
    /// Scheduler tick in milliseconds.
    #[serde(default = "default_extractor_interval_ms")]
    pub interval_ms: u64,
    /// Max memories drained per cycle (caps pattern / classifier /
    /// LLM work per scheduler tick).
    #[serde(default = "default_extractor_drain_per_cycle")]
    pub drain_per_cycle: usize,
    /// Per-cycle LLM cost ceiling in dollar-micro-units (1e-6 USD).
    #[serde(default = "default_extractor_llm_budget_micro_usd")]
    pub llm_budget_per_cycle_micro_usd: u64,
    /// Writer → worker queue depth. Overflow drops the enqueue with
    /// a warn (encode itself never fails).
    #[serde(default = "default_extractor_channel_capacity")]
    pub channel_capacity: usize,
    /// Skip memories that already carry a pipeline audit row. Set to
    /// `false` only for re-extraction backfill scenarios.
    #[serde(default = "default_extractor_skip_audited")]
    pub skip_already_extracted: bool,
    /// Memories the extractor worker bundles into one classifier
    /// forward pass per cycle iteration. The GLiNER backbone GEMM
    /// dominates per-encode latency; batching 8 memories pulls
    /// per-memory cost down by ~4-5x on a CPU host. Operators can
    /// override via `BRAIN_EXTRACTOR_BATCH_SIZE` for tail-latency
    /// tuning.
    #[serde(default = "default_extractor_batch_size")]
    pub batch_size: usize,
}

impl Default for ExtractorWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: default_extractor_enabled(),
            interval_ms: default_extractor_interval_ms(),
            drain_per_cycle: default_extractor_drain_per_cycle(),
            llm_budget_per_cycle_micro_usd: default_extractor_llm_budget_micro_usd(),
            channel_capacity: default_extractor_channel_capacity(),
            skip_already_extracted: default_extractor_skip_audited(),
            batch_size: default_extractor_batch_size(),
        }
    }
}

fn default_extractor_enabled() -> bool {
    true
}
fn default_extractor_interval_ms() -> u64 {
    1000
}
fn default_extractor_drain_per_cycle() -> usize {
    32
}
fn default_extractor_llm_budget_micro_usd() -> u64 {
    50_000
}
fn default_extractor_channel_capacity() -> usize {
    1024
}
fn default_extractor_skip_audited() -> bool {
    true
}
fn default_extractor_batch_size() -> usize {
    brain_workers::DEFAULT_EXTRACTOR_BATCH_SIZE
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
    /// Master switch. When false, no spans are exported regardless of
    /// other fields. Default `false` so "no-trace
    /// fallback" is honoured out of the box.
    #[serde(default)]
    pub enabled: bool,
    /// Sampler name: `always_on`, `always_off`, `ratio`, `parent_based`.
    /// Default `always_off`.
    #[serde(default)]
    pub sampler: String,
    /// Used only when `sampler == "ratio"`. Clamped to `[0.0, 1.0]`.
    #[serde(default)]
    pub sample_ratio: f64,
    /// OTLP/HTTP endpoint of the upstream collector. Empty means
    /// "no endpoint configured" — tracing degrades to a stdout
    /// exporter if `enabled = true` and this is empty.
    #[serde(default)]
    pub endpoint: String,
    /// Service-name attribute attached to every span (OTel
    /// `service.name`). Defaults to `brain-server`.
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

fn default_service_name() -> String {
    "brain-server".to_string()
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
    /// default: consolidation worker is a no-op.
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

    fn embedder(model: &str) -> EmbedderConfig {
        EmbedderConfig {
            model: model.to_owned(),
            cache_size: 1,
            batch_size: 1,
            batch_window_ms: 1,
        }
    }

    #[test]
    fn resolve_model_dir_honours_env() {
        let cfg = embedder("bge-small-en-v1.5");
        let env = |k: &str| match k {
            "BRAIN_EMBED_MODEL_DIR" => Some("/var/lib/brain/models/x".to_owned()),
            _ => None,
        };
        let p = cfg.resolve_model_dir_with(&env).unwrap();
        assert_eq!(p, PathBuf::from("/var/lib/brain/models/x"));
    }

    #[test]
    fn resolve_model_dir_honours_absolute_path() {
        let cfg = embedder("/opt/models/bge");
        let env = |_: &str| None;
        let p = cfg.resolve_model_dir_with(&env).unwrap();
        assert_eq!(p, PathBuf::from("/opt/models/bge"));
    }

    #[test]
    fn resolve_model_dir_xdg_default() {
        let cfg = embedder("bge-small-en-v1.5");
        let env = |k: &str| match k {
            "XDG_DATA_HOME" => Some("/home/dev/.local/share".to_owned()),
            _ => None,
        };
        let p = cfg.resolve_model_dir_with(&env).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/dev/.local/share/brain/models/bge-small-en-v1.5"),
        );
    }

    #[test]
    fn resolve_model_dir_home_fallback() {
        let cfg = embedder("bge-small-en-v1.5");
        let env = |k: &str| match k {
            "HOME" => Some("/home/dev".to_owned()),
            _ => None,
        };
        let p = cfg.resolve_model_dir_with(&env).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/dev/.local/share/brain/models/bge-small-en-v1.5"),
        );
    }

    #[test]
    fn resolve_model_dir_rejects_relative_env() {
        let cfg = embedder("bge-small-en-v1.5");
        let env = |k: &str| match k {
            "BRAIN_EMBED_MODEL_DIR" => Some("relative/path".to_owned()),
            _ => None,
        };
        let err = cfg.resolve_model_dir_with(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Invariant(ref m) if m.contains("absolute")));
    }

    #[test]
    fn resolve_model_dir_errors_without_env_or_home() {
        let cfg = embedder("bge-small-en-v1.5");
        let env = |_: &str| None;
        assert!(matches!(
            cfg.resolve_model_dir_with(&env),
            Err(ConfigError::Invariant(_))
        ));
    }

    #[test]
    fn set_path_inserts_into_missing_section() {
        let mut value: toml::Value = toml::from_str("[a]\nb = 1\n").unwrap();
        set_path(&mut value, &["c".into(), "d".into(), "e".into()], "true");
        // "true" coerces to a Boolean.
        assert!(value["c"]["d"]["e"].as_bool().unwrap());
    }
}
