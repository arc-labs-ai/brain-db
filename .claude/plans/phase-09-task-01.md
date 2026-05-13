# Sub-task 9.1 — Config loading

**Reads:** `config/dev.toml`, spec §01/04 §15
**Phase doc:** `docs/phases/phase-09-server.md` §9.1
**Done when:** Config struct deserializes from TOML; env var overrides supported; missing required fields produce clear errors.

---

## 1. Scope

Smallest sub-task in Phase 9, intentional first move because everything else needs typed config. Ship:

- Typed `Config` struct (+ nested sub-structs) that round-trips `config/dev.toml`.
- `Config::load(path)` with a small env-override layer (`BRAIN__SERVER__LISTEN_ADDR` style).
- Helpful errors for missing / malformed fields.
- Wire it through `main.rs`: `--config <PATH>` (default `config/dev.toml`), print loaded summary at startup, exit clean on validation failure.

Out of scope:
- Hot-reload (spec §01/04 §15: restart-only for v1).
- The TLS sub-fields' actual use (lives in 9.9). 9.1 just deserializes them so the schema is complete.
- `figment` integration — spec mentions it, but `serde + toml + a tiny env walker` is enough for v1 and avoids the dep.

---

## 2. The `Config` shape (matches `config/dev.toml`)

```rust
// crates/brain-server/src/config.rs

#[derive(Clone, Debug, Deserialize, PartialEq)]
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
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    #[serde(default)]
    pub tls: TlsConfig,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub shard_count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShardConfig {
    #[serde(deserialize_with = "human_bytes")]
    pub arena_capacity_bytes: u64,
    #[serde(deserialize_with = "human_bytes")]
    pub wal_segment_size_bytes: u64,
    pub wal_retention_segments: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HnswConfig {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmbedderConfig {
    pub model: String,
    pub cache_size: usize,
    pub batch_size: usize,
    pub batch_window_ms: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
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

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub output: String,   // "stdout" | "stderr" | "file:<path>"
    pub format: String,   // "json" | "compact" | "pretty"
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TracingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sampler: String,        // "ratio" | "always_on" | "always_off"
    #[serde(default)]
    pub sample_ratio: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub mode: AuthMode,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { mode: AuthMode::None }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    None,
    ApiKey,    // 9.x sub-task wires the actual handshake
}
```

`#[serde(deny_unknown_fields)]` everywhere so typos in TOML fail loudly instead of silently dropping fields.

---

## 3. The `human_bytes` helper

`config/dev.toml` uses `"1GiB"` / `"256MiB"` syntax. Need a tiny custom deserializer:

```rust
fn human_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    parse_human_bytes(&s).map_err(serde::de::Error::custom)
}

fn parse_human_bytes(s: &str) -> Result<u64, ConfigError> {
    let s = s.trim();
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits.parse().map_err(...)?;
    let mult: u64 = match suffix.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "KB"  => 1_000,
        "MB"  => 1_000_000,
        "GB"  => 1_000_000_000,
        other => return Err(ConfigError::BadByteSuffix(other.into())),
    };
    n.checked_mul(mult).ok_or(ConfigError::Overflow)
}
```

Pure binary multipliers (`KiB`, `MiB`, `GiB`) match the dev.toml convention.

---

## 4. Env-override layer

Pattern: `BRAIN__SECTION__FIELD=value`. Double underscore separates nesting.

```rust
fn apply_env_overrides(value: &mut toml::Value) {
    for (key, val) in std::env::vars() {
        if let Some(suffix) = key.strip_prefix("BRAIN__") {
            let path: Vec<&str> = suffix.split("__").collect();
            // Lowercase each segment to match TOML field names.
            apply_one(value, &path, val);
        }
    }
}
```

Walk the TOML AST, descend by lowercased path components, replace the leaf as a string. The subsequent `serde::Deserialize` handles type coercion.

Examples:
- `BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:8080` overrides `[server] listen_addr`
- `BRAIN__STORAGE__SHARD_COUNT=8` overrides `[storage] shard_count`
- `BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB` overrides `[shard] arena_capacity_bytes`

Edge cases:
- Path doesn't exist → silently inserted at the right spot (so env can add fields).
- Type mismatch → fails at deserialize time with a clear error.

---

## 5. `Config::load(path)`

```rust
pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
        path: path.to_owned(),
        source: e,
    })?;
    let mut value: toml::Value = toml::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path.to_owned(),
        source: e,
    })?;
    apply_env_overrides(&mut value);
    let cfg: Config = value.try_into().map_err(|e| ConfigError::Validate {
        path: path.to_owned(),
        source: e,
    })?;
    cfg.validate_post()?;
    Ok(cfg)
}
```

`validate_post()` checks invariants serde can't:
- `storage.shard_count >= 1`.
- `embedder.batch_size >= 1`.
- `hnsw.m >= 2`, `ef_construction >= m`, `ef_search >= 1`.
- If `server.tls.enabled` then `cert` and `key` must both be `Some`.

---

## 6. `ConfigError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found / unreadable at {path}: {source}")]
    Read { path: PathBuf, #[source] source: std::io::Error },
    #[error("config TOML parse error at {path}: {source}")]
    Parse { path: PathBuf, #[source] source: toml::de::Error },
    #[error("config validation error at {path}: {source}")]
    Validate { path: PathBuf, #[source] source: toml::de::Error },
    #[error("bad byte suffix: '{0}' (expected KiB|MiB|GiB|TiB|KB|MB|GB|B)")]
    BadByteSuffix(String),
    #[error("byte value overflows u64")]
    Overflow,
    #[error("invalid config: {0}")]
    Invariant(String),
}
```

Errors are clear and actionable — operator sees the path, the section, the offending field.

---

## 7. `main.rs` integration

```rust
fn main() -> ExitCode {
    init_tracing_pre_config();   // info-level, compact, until config loads

    let args: Args = parse_args();
    if args.show_version { ... }
    if args.show_help { ... }

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    // Re-init tracing with config-driven level / format.
    init_tracing_from_config(&cfg.logging);

    tracing::info!(
        version = %VERSION,
        listen = %cfg.server.listen_addr,
        shards = cfg.storage.shard_count,
        "brain-server starting (Phase 9 stub: config loaded, runtime not yet wired)"
    );
    tracing::info!("exiting cleanly (runtime lands in sub-tasks 9.4+)");
    ExitCode::SUCCESS
}

struct Args { config: PathBuf, show_version: bool, show_help: bool }

fn parse_args() -> Args { /* simple argv walk, no clap dep */ }
```

Default config path: `config/dev.toml`.

---

## 8. File-by-file plan

| File | Action | Notes |
| ---- | ------ | ----- |
| `crates/brain-server/src/config.rs`     | NEW    | Types + load + validate + env overrides + ConfigError |
| `crates/brain-server/src/main.rs`       | Edit   | Wire `--config`, `Config::load`, config-driven tracing init |
| `crates/brain-server/Cargo.toml`        | None   | `anyhow + serde + toml + tracing + tracing-subscriber` already present |
| `crates/brain-server/tests/config.rs`   | NEW    | ~12 tests |

No spec / wire / other-crate changes.

---

## 9. Tests (`tests/config.rs`)

### Round-trip dev.toml (1)
1. `dev_toml_round_trips_cleanly` — load `config/dev.toml` from workspace root, assert top-level fields match the file's literal values.

### Parser (4)
2. `human_bytes_parses_binary_units` — "1GiB" → 2^30, "256MiB" → 256 * 2^20.
3. `human_bytes_parses_decimal_units` — "1MB" → 1_000_000.
4. `human_bytes_bare_number_is_bytes` — "1024" → 1024, "1024B" → 1024.
5. `human_bytes_bad_suffix_errors_cleanly` — "1XYZ" → `ConfigError::BadByteSuffix`.

### Validation (3)
6. `missing_required_section_errors` — TOML missing `[server]` → clear error mentioning `server`.
7. `unknown_field_errors` — extra `[server] foo = 1` → clear error mentioning `foo`.
8. `tls_enabled_without_cert_or_key_errors` — `tls.enabled = true` with no cert → `ConfigError::Invariant`.

### Env overrides (3)
9. `env_override_replaces_string_field` — set `BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:8080`; assert applied.
10. `env_override_replaces_integer_field` — set `BRAIN__STORAGE__SHARD_COUNT=8`; assert applied.
11. `env_override_with_no_matching_path_inserts` — set `BRAIN__SERVER__TLS__ENABLED=true`; assert applied.

### Defaults (1)
12. `omitted_optional_sections_use_defaults` — TOML with only required sections; assert `[workers] = empty`, `[logging] = compact stdout info`, `[auth] = none`.

Env-mutation tests need careful handling — set env, run, unset. Use a small RAII guard or `serial_test` if needed. Simpler: write the env-override logic to **accept a `HashMap<String, String>` of overrides** instead of reading `std::env::vars()` directly, so tests pass the map and `load_with_env()` wraps it. The public `load()` just reads `std::env::vars()` into the map. Trivial; sidesteps the global-mutation flake.

---

## 10. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `dev.toml` schema evolves during Phase 9 | This sub-task is the schema authority. Subsequent sub-tasks add fields via `#[serde(default)]` so existing configs stay valid |
| TOML AST walking for env overrides has edge cases (arrays, nested arrays) | dev.toml has no arrays of tables. Document the limitation: env overrides target scalar fields, not arrays. Surface clearly |
| Required-field-missing error from serde is verbose / ugly | The `Validate` arm of `ConfigError` carries the original `toml::de::Error` Display, which gives line+column. Acceptable v1 |
| The `--config <PATH>` arg without clap means hand-rolled argv parse | argv is short (`--config X`, `--version`, `--help`). 20 lines, no dep |

---

## 11. Done criteria

- [ ] `Config` + sub-structs + `ConfigError` in `config.rs`.
- [ ] `Config::load(path)` works end-to-end with `dev.toml`.
- [ ] `main.rs` wires `--config`, prints summary, exits clean.
- [ ] 12 tests pass first run.
- [ ] `cargo test --workspace` green; clippy + fmt clean.
- [ ] Commit subject: `feat(brain-server): config loading (sub-task 9.1)`.

~500 LOC impl + ~400 LOC tests. Single commit.
