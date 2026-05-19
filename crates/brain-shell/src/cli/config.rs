//! Brain shell on-disk configuration.
//!
//! One TOML file at `${XDG_CONFIG_HOME:-~/.config}/brain/config.toml`
//! holding two concerns:
//!
//! - `[settings]` — persistent shell preferences (output format,
//!   timing, sticky context, default server).
//! - `[agents.<name>]` — named agent identities (an AWS-profile-like
//!   bag the user can opt into via `--agent <name>` /
//!   `BRAIN_AGENT=<name>`).
//!
//! Backwards-incompatibly replaces the earlier single-field
//! `agent_id = "<uuid>"` shape; legacy files migrate on first load.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

/// Default filename inside the config dir.
pub const FILE_NAME: &str = "config.toml";

/// Header comment written above an entirely-new file. Edits via
/// `set` / agent CRUD preserve the rest of the file but rewrite
/// this header on every save so the most current usage notes are
/// always at the top.
const HEADER: &str = "\
# Brain shell configuration.
#
# Created on first run. Edit by hand or via `brain config set`
# and `brain agent create`. Delete the file to reset everything.
#
# Settings are persistent shell preferences. Named agents under
# [agents.<name>] are opt-in identities — bare `brain` mints a
# fresh ephemeral agent. Pass `--agent <name>` or set
# `BRAIN_AGENT=<name>` to use a stored one.

";

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentEntry>,
}

/// Closed set of persistent shell preferences. Adding a key takes
/// one line; unknown keys in the file are rejected so typos
/// surface as errors rather than ghost-persisting forever.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputPref>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// `output` preference. Mirror of the existing `OutputFormatArg` so
/// the wire schema is independent of clap's value-enum names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputPref {
    Table,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEntry {
    /// UUIDv7 string. Resolver validates on lookup.
    pub id: String,
    /// RFC3339 UTC timestamp the entry was created at. Stored as
    /// a string so the wire schema doesn't depend on time-crate
    /// features (and the field round-trips through `cat` cleanly).
    pub created_at: String,
    #[serde(default, skip_serializing_if = "String::is_none_or_empty")]
    pub note: String,
}

// Workaround: `String::is_empty` isn't a method-on-Option-of-String,
// so we provide a small helper for the `skip_serializing_if` above.
trait IsNoneOrEmpty {
    fn is_none_or_empty(&self) -> bool;
}
impl IsNoneOrEmpty for String {
    fn is_none_or_empty(&self) -> bool {
        self.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config dir unavailable (XDG_CONFIG_HOME and HOME both unset)")]
    NoConfigDir,

    #[error("config file at {path} is malformed: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("unknown setting key: {key}{hint}", hint = match suggestion { Some(s) => format!(". did you mean '{s}'?"), None => String::new() })]
    UnknownKey {
        key: String,
        suggestion: Option<String>,
    },

    #[error("invalid value for {key}: {detail}")]
    InvalidValue { key: String, detail: String },

    #[error("agent '{name}' already exists")]
    AgentExists { name: String },

    #[error("unknown agent '{name}'{hint}", hint = match suggestion { Some(s) => format!(". did you mean '{s}'?"), None => String::new() })]
    AgentUnknown {
        name: String,
        suggestion: Option<String>,
    },

    #[error("agent name '{name}' is invalid: {reason}")]
    AgentBadName { name: String, reason: &'static str },

    #[error("agent id is not a valid uuid: {0}")]
    AgentBadId(#[source] uuid::Error),
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve `<config_dir>/brain/config.toml` via `dirs::config_dir()`.
/// Returns `None` if no config dir is available.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("brain").join(FILE_NAME))
}

/// Same as [`default_path`] but takes the config-dir root explicitly
/// so tests can drive without touching process env.
#[must_use]
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("brain").join(FILE_NAME)
}

// ---------------------------------------------------------------------------
// Top-level handle
// ---------------------------------------------------------------------------

/// Loaded view of `config.toml`. Mutate via the typed setters, then
/// `save` to persist atomically.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub file: ConfigFile,
    pub path: PathBuf,
}

impl Config {
    /// Load from `dirs::config_dir()/brain/config.toml`. Returns a
    /// default Config (empty file) when the path doesn't exist yet.
    /// Legacy single-`agent_id` files migrate in place.
    pub fn load_or_default() -> Result<(Self, Option<MigrationNote>), ConfigError> {
        let path = default_path().ok_or(ConfigError::NoConfigDir)?;
        Self::load_or_default_at(&path)
    }

    /// As above but takes the path explicitly — testing entry point.
    pub fn load_or_default_at(path: &Path) -> Result<(Self, Option<MigrationNote>), ConfigError> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let (file, note) = parse_or_migrate(&contents, path)?;
                Ok((
                    Self {
                        file,
                        path: path.to_path_buf(),
                    },
                    note,
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((
                Self {
                    file: ConfigFile::default(),
                    path: path.to_path_buf(),
                },
                None,
            )),
            Err(e) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Atomically render and rewrite the file. Creates the parent
    /// directory if missing, chmod 600 on Unix.
    pub fn save(&self) -> Result<(), ConfigError> {
        save_atomic(&self.path, &self.file)
    }

    pub fn settings(&self) -> &Settings {
        &self.file.settings
    }

    pub fn agents(&self) -> &BTreeMap<String, AgentEntry> {
        &self.file.agents
    }

    // ----- typed setting CRUD ------------------------------------

    /// Validate and apply a `<key> <value>` pair. Caller persists via
    /// [`Self::save`] afterwards.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        match key {
            "output" => {
                let v = match value {
                    "table" => OutputPref::Table,
                    "json" => OutputPref::Json,
                    other => {
                        return Err(ConfigError::InvalidValue {
                            key: key.into(),
                            detail: format!("'{other}' is not one of: table, json"),
                        });
                    }
                };
                self.file.settings.output = Some(v);
            }
            "timing" => {
                let v = match value {
                    "true" | "on" | "1" => true,
                    "false" | "off" | "0" => false,
                    other => {
                        return Err(ConfigError::InvalidValue {
                            key: key.into(),
                            detail: format!("'{other}' is not one of: true|on|1, false|off|0"),
                        });
                    }
                };
                self.file.settings.timing = Some(v);
            }
            "sticky_context" => {
                let v: u64 = value.parse().map_err(|e| ConfigError::InvalidValue {
                    key: key.into(),
                    detail: format!("'{value}' is not a non-negative integer: {e}"),
                })?;
                self.file.settings.sticky_context = Some(v);
            }
            "server" => {
                // Light validation: must contain ':' so it looks like
                // host:port. The full `parse_server` validator lives
                // higher up and depends on the connect path; we keep
                // this loose to avoid duplicating that logic.
                if !value.contains(':') {
                    return Err(ConfigError::InvalidValue {
                        key: key.into(),
                        detail: format!("'{value}' must be host:port"),
                    });
                }
                self.file.settings.server = Some(value.to_owned());
            }
            other => {
                return Err(ConfigError::UnknownKey {
                    key: other.to_owned(),
                    suggestion: closest_key(other, &KNOWN_KEYS),
                });
            }
        }
        Ok(())
    }

    /// Read back a single value as the string we'd accept in `set`.
    /// Returns the literal `"(unset)"` when missing — chosen over
    /// `Option` so the CLI rendering is uniform without a wrapper.
    pub fn get(&self, key: &str) -> Result<String, ConfigError> {
        match key {
            "output" => Ok(self
                .file
                .settings
                .output
                .map(|o| match o {
                    OutputPref::Table => "table",
                    OutputPref::Json => "json",
                })
                .unwrap_or("(unset)")
                .to_owned()),
            "timing" => Ok(self
                .file
                .settings
                .timing
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("(unset)")
                .to_owned()),
            "sticky_context" => Ok(self
                .file
                .settings
                .sticky_context
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(unset)".to_owned())),
            "server" => Ok(self
                .file
                .settings
                .server
                .clone()
                .unwrap_or_else(|| "(unset)".to_owned())),
            other => Err(ConfigError::UnknownKey {
                key: other.to_owned(),
                suggestion: closest_key(other, &KNOWN_KEYS),
            }),
        }
    }

    /// All known keys with their current values — for `config list`.
    /// Stable ordering matches the module-private `KNOWN_KEYS` array.
    pub fn list(&self) -> Vec<(&'static str, String)> {
        KNOWN_KEYS
            .iter()
            .map(|k| (*k, self.get(k).unwrap_or_else(|_| "(unset)".to_owned())))
            .collect()
    }

    // ----- agent CRUD --------------------------------------------

    /// Insert a fresh agent. Mints a UUIDv7 and stamps `created_at`.
    /// Errors if the name already exists or is invalid.
    pub fn create_agent(&mut self, name: &str, note: &str) -> Result<&AgentEntry, ConfigError> {
        validate_agent_name(name)?;
        if self.file.agents.contains_key(name) {
            return Err(ConfigError::AgentExists {
                name: name.to_owned(),
            });
        }
        let entry = AgentEntry {
            id: Uuid::now_v7().to_string(),
            created_at: now_rfc3339(),
            note: note.to_owned(),
        };
        self.file.agents.insert(name.to_owned(), entry);
        Ok(self.file.agents.get(name).expect("just inserted"))
    }

    /// Insert an externally-supplied id under a local name — for
    /// sharing across machines / teammates.
    pub fn import_agent(
        &mut self,
        name: &str,
        id: &str,
        note: &str,
    ) -> Result<&AgentEntry, ConfigError> {
        validate_agent_name(name)?;
        Uuid::parse_str(id).map_err(ConfigError::AgentBadId)?;
        if self.file.agents.contains_key(name) {
            return Err(ConfigError::AgentExists {
                name: name.to_owned(),
            });
        }
        let entry = AgentEntry {
            id: id.to_owned(),
            created_at: now_rfc3339(),
            note: note.to_owned(),
        };
        self.file.agents.insert(name.to_owned(), entry);
        Ok(self.file.agents.get(name).expect("just inserted"))
    }

    pub fn rename_agent(&mut self, old: &str, new: &str) -> Result<(), ConfigError> {
        validate_agent_name(new)?;
        let entry = self
            .file
            .agents
            .remove(old)
            .ok_or_else(|| ConfigError::AgentUnknown {
                name: old.to_owned(),
                suggestion: closest_agent(old, &self.file.agents),
            })?;
        if self.file.agents.contains_key(new) {
            // Restore the original entry before bailing so the
            // in-memory view stays consistent if the caller doesn't
            // re-load.
            self.file.agents.insert(old.to_owned(), entry);
            return Err(ConfigError::AgentExists {
                name: new.to_owned(),
            });
        }
        self.file.agents.insert(new.to_owned(), entry);
        Ok(())
    }

    pub fn delete_agent(&mut self, name: &str) -> Result<AgentEntry, ConfigError> {
        self.file
            .agents
            .remove(name)
            .ok_or_else(|| ConfigError::AgentUnknown {
                name: name.to_owned(),
                suggestion: closest_agent(name, &self.file.agents),
            })
    }

    pub fn get_agent(&self, name: &str) -> Result<&AgentEntry, ConfigError> {
        self.file
            .agents
            .get(name)
            .ok_or_else(|| ConfigError::AgentUnknown {
                name: name.to_owned(),
                suggestion: closest_agent(name, &self.file.agents),
            })
    }
}

/// Result of [`Config::load_or_default`] when a legacy single-uuid
/// file was rewritten in place. The CLI surfaces this as a one-line
/// `note:` on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationNote {
    pub backup_path: PathBuf,
    pub migrated_name: String,
}

// ---------------------------------------------------------------------------
// Parse / migrate
// ---------------------------------------------------------------------------

/// Catch the legacy `agent_id = "<uuid>"` shape on parse failure and
/// migrate it in place. The legacy file is moved to
/// `config.toml.bak-YYYYMMDDHHMMSS` first.
fn parse_or_migrate(
    contents: &str,
    path: &Path,
) -> Result<(ConfigFile, Option<MigrationNote>), ConfigError> {
    // Fast path: parse as the current schema.
    match toml::from_str::<ConfigFile>(contents) {
        Ok(file) => Ok((file, None)),
        Err(strict_err) => {
            // Try the legacy shape: `agent_id = "<uuid>"` at top level.
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Legacy {
                agent_id: String,
            }
            match toml::from_str::<Legacy>(contents) {
                Ok(legacy) => {
                    Uuid::parse_str(&legacy.agent_id).map_err(ConfigError::AgentBadId)?;
                    let backup = backup_path_for(path);
                    fs::rename(path, &backup).map_err(|e| ConfigError::Write {
                        path: backup.clone(),
                        source: e,
                    })?;
                    let mut agents = BTreeMap::new();
                    agents.insert(
                        "default".to_owned(),
                        AgentEntry {
                            id: legacy.agent_id,
                            created_at: now_rfc3339(),
                            note: "migrated from legacy singleton".to_owned(),
                        },
                    );
                    let migrated = ConfigFile {
                        settings: Settings::default(),
                        agents,
                    };
                    save_atomic(path, &migrated)?;
                    Ok((
                        migrated,
                        Some(MigrationNote {
                            backup_path: backup,
                            migrated_name: "default".to_owned(),
                        }),
                    ))
                }
                Err(_) => Err(ConfigError::Parse {
                    path: path.to_path_buf(),
                    source: strict_err,
                }),
            }
        }
    }
}

fn backup_path_for(path: &Path) -> PathBuf {
    let ts = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".into());
    // Compress to YYYYMMDDHHMMSS form for the suffix.
    let compact: String = ts.chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    let stem = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    path.with_file_name(format!("{stem}.bak-{compact}"))
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

fn save_atomic(path: &Path, file: &ConfigFile) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .expect("config path always has a parent (config_dir/brain/...)");
    fs::create_dir_all(parent).map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;

    let body = toml::to_string_pretty(file).map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: io::Error::other(e.to_string()),
    })?;
    let mut full = String::with_capacity(HEADER.len() + body.len());
    full.push_str(HEADER);
    full.push_str(&body);

    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;
    tmp.write_all(full.as_bytes())
        .map_err(|e| ConfigError::Write {
            path: path.to_path_buf(),
            source: e,
        })?;
    tmp.as_file().sync_all().map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600)).map_err(|e| {
            ConfigError::Write {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
    }
    tmp.persist(path).map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const KNOWN_KEYS: [&str; 4] = ["output", "timing", "sticky_context", "server"];

fn validate_agent_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::AgentBadName {
            name: name.to_owned(),
            reason: "name must not be empty",
        });
    }
    if name.len() > 64 {
        return Err(ConfigError::AgentBadName {
            name: name.to_owned(),
            reason: "name must be <= 64 chars",
        });
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(ConfigError::AgentBadName {
            name: name.to_owned(),
            reason: "name must match [A-Za-z0-9_-]",
        });
    }
    Ok(())
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Cheap Levenshtein-1 plus prefix heuristic for "did you mean".
fn closest_key(needle: &str, haystack: &[&str]) -> Option<String> {
    haystack
        .iter()
        .map(|h| (*h, levenshtein(needle, h)))
        .min_by_key(|(_, d)| *d)
        .and_then(|(h, d)| if d <= 2 { Some(h.to_owned()) } else { None })
}

fn closest_agent(needle: &str, agents: &BTreeMap<String, AgentEntry>) -> Option<String> {
    agents
        .keys()
        .map(|h| (h.clone(), levenshtein(needle, h)))
        .min_by_key(|(_, d)| *d)
        .and_then(|(h, d)| if d <= 2 { Some(h) } else { None })
}

/// Standard textbook Levenshtein. ~10 LOC; not worth a crate.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg_path(t: &TempDir) -> PathBuf {
        path_in(t.path())
    }

    // ----- file IO ------------------------------------------------

    #[test]
    fn load_missing_file_returns_default_config() {
        let t = TempDir::new().unwrap();
        let (c, note) = Config::load_or_default_at(&cfg_path(&t)).unwrap();
        assert!(note.is_none());
        assert!(c.file.agents.is_empty());
        assert_eq!(c.file.settings, Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let t = TempDir::new().unwrap();
        let path = cfg_path(&t);
        let mut c = Config::load_or_default_at(&path).unwrap().0;
        c.set("output", "json").unwrap();
        c.set("timing", "true").unwrap();
        c.set("sticky_context", "7").unwrap();
        c.create_agent("work", "prod notebook").unwrap();
        c.save().unwrap();

        let (re, note) = Config::load_or_default_at(&path).unwrap();
        assert!(note.is_none());
        assert_eq!(re.file.settings.output, Some(OutputPref::Json));
        assert_eq!(re.file.settings.timing, Some(true));
        assert_eq!(re.file.settings.sticky_context, Some(7));
        assert_eq!(re.file.agents.len(), 1);
        assert!(re.file.agents.contains_key("work"));
    }

    #[test]
    fn save_sets_chmod_600_on_unix() {
        let t = TempDir::new().unwrap();
        let path = cfg_path(&t);
        let mut c = Config::load_or_default_at(&path).unwrap().0;
        c.create_agent("a", "").unwrap();
        c.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn malformed_toml_errors_without_overwriting() {
        let t = TempDir::new().unwrap();
        let path = cfg_path(&t);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "garbage = ===").unwrap();
        let err = Config::load_or_default_at(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "garbage = ===");
    }

    // ----- settings -----------------------------------------------

    #[test]
    fn set_then_get_returns_serialised_value() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.set("output", "json").unwrap();
        c.set("timing", "off").unwrap();
        c.set("sticky_context", "42").unwrap();
        c.set("server", "10.0.0.1:9090").unwrap();
        assert_eq!(c.get("output").unwrap(), "json");
        assert_eq!(c.get("timing").unwrap(), "false");
        assert_eq!(c.get("sticky_context").unwrap(), "42");
        assert_eq!(c.get("server").unwrap(), "10.0.0.1:9090");
    }

    #[test]
    fn set_unknown_key_errors_with_hint() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let err = c.set("ouput", "json").unwrap_err();
        match err {
            ConfigError::UnknownKey { key, suggestion } => {
                assert_eq!(key, "ouput");
                assert_eq!(suggestion.as_deref(), Some("output"));
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn set_invalid_value_errors() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        assert!(matches!(
            c.set("output", "yaml").unwrap_err(),
            ConfigError::InvalidValue { .. }
        ));
        assert!(matches!(
            c.set("sticky_context", "not-a-number").unwrap_err(),
            ConfigError::InvalidValue { .. }
        ));
        assert!(matches!(
            c.set("server", "no-colon-here").unwrap_err(),
            ConfigError::InvalidValue { .. }
        ));
    }

    #[test]
    fn list_includes_unset_marker_for_missing_keys() {
        let t = TempDir::new().unwrap();
        let c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let rows: Vec<_> = c.list();
        assert_eq!(rows.len(), KNOWN_KEYS.len());
        for (_, v) in &rows {
            assert_eq!(v, "(unset)");
        }
    }

    #[test]
    fn known_keys_in_list_match_get() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.set("output", "table").unwrap();
        let map: BTreeMap<_, _> = c.list().into_iter().collect();
        assert_eq!(map["output"], "table");
    }

    // ----- agents ------------------------------------------------

    #[test]
    fn create_agent_writes_uuid_and_timestamp() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let e = c.create_agent("work", "prod notebook").unwrap().clone();
        Uuid::parse_str(&e.id).expect("id is a uuid");
        assert!(e.created_at.contains('T') && e.created_at.ends_with('Z'));
        assert_eq!(e.note, "prod notebook");
    }

    #[test]
    fn create_agent_duplicate_errors() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.create_agent("work", "").unwrap();
        let err = c.create_agent("work", "").unwrap_err();
        assert!(
            matches!(err, ConfigError::AgentExists { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn rename_agent_atomic() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.create_agent("work", "").unwrap();
        c.rename_agent("work", "prod").unwrap();
        assert!(c.get_agent("work").is_err());
        assert!(c.get_agent("prod").is_ok());
    }

    #[test]
    fn rename_to_existing_target_preserves_source() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.create_agent("a", "").unwrap();
        c.create_agent("b", "").unwrap();
        let err = c.rename_agent("a", "b").unwrap_err();
        assert!(
            matches!(err, ConfigError::AgentExists { .. }),
            "got {err:?}"
        );
        assert!(c.get_agent("a").is_ok());
        assert!(c.get_agent("b").is_ok());
    }

    #[test]
    fn delete_agent_returns_removed_entry() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let original = c.create_agent("work", "").unwrap().id.clone();
        let removed = c.delete_agent("work").unwrap();
        assert_eq!(removed.id, original);
        assert!(c.get_agent("work").is_err());
    }

    #[test]
    fn delete_unknown_errors_with_hint() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        c.create_agent("work", "").unwrap();
        let err = c.delete_agent("wokr").unwrap_err();
        match err {
            ConfigError::AgentUnknown { name, suggestion } => {
                assert_eq!(name, "wokr");
                assert_eq!(suggestion.as_deref(), Some("work"));
            }
            other => panic!("expected AgentUnknown, got {other:?}"),
        }
    }

    #[test]
    fn import_agent_accepts_external_uuid() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let id = "019e3b00-0000-7000-8000-000000000001";
        let e = c
            .import_agent("shared", id, "from teammate")
            .unwrap()
            .clone();
        assert_eq!(e.id, id);
        assert_eq!(e.note, "from teammate");
    }

    #[test]
    fn import_agent_rejects_bad_uuid() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        let err = c.import_agent("shared", "not-a-uuid", "").unwrap_err();
        assert!(matches!(err, ConfigError::AgentBadId(_)), "got {err:?}");
    }

    #[test]
    fn invalid_agent_name_errors() {
        let t = TempDir::new().unwrap();
        let mut c = Config::load_or_default_at(&cfg_path(&t)).unwrap().0;
        for bad in ["", "has space", "has/slash", &"x".repeat(100)] {
            let err = c.create_agent(bad, "").unwrap_err();
            assert!(
                matches!(err, ConfigError::AgentBadName { .. }),
                "got {err:?}"
            );
        }
    }

    // ----- migration ---------------------------------------------

    #[test]
    fn legacy_singleton_file_migrates_to_named_default() {
        let t = TempDir::new().unwrap();
        let path = cfg_path(&t);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy_uuid = "019e3b00-0000-7000-8000-000000000001";
        fs::write(&path, format!("agent_id = \"{legacy_uuid}\"\n")).unwrap();

        let (c, note) = Config::load_or_default_at(&path).unwrap();
        let note = note.expect("migration note");
        assert_eq!(note.migrated_name, "default");
        assert!(note.backup_path.exists(), "backup file present");
        // Backup carries the original contents.
        let backup_contents = fs::read_to_string(&note.backup_path).unwrap();
        assert!(backup_contents.contains(legacy_uuid));
        // Migrated file has the new schema.
        let entry = c.get_agent("default").unwrap();
        assert_eq!(entry.id, legacy_uuid);
        assert_eq!(entry.note, "migrated from legacy singleton");
        // And the on-disk file is the new shape.
        let new_contents = fs::read_to_string(&path).unwrap();
        assert!(new_contents.contains("[agents.default]"));
        assert!(!new_contents.contains("agent_id ="));
    }

    #[test]
    fn legacy_file_with_bad_uuid_does_not_migrate() {
        let t = TempDir::new().unwrap();
        let path = cfg_path(&t);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "agent_id = \"not-a-uuid\"\n").unwrap();
        let err = Config::load_or_default_at(&path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentBadId(_)), "got {err:?}");
    }
}
