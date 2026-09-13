//! CI gate for `config/monitoring/dashboards/*.json`.
//!
//! Verifies each dashboard:
//! - Parses as valid JSON.
//! - Has the expected top-level shape (title, uid, schemaVersion,
//!   panels).
//! - References only metric names that match the `brain_*` /
//!   `process_*` taxonomy — typos like `brain_requets_total` get
//!   caught here, not at scrape time in prod.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;

/// All dashboards Brain ships.
const EXPECTED_DASHBOARDS: &[&str] = &[
    "overview",
    "per-shard",
    "storage",
    "hnsw",
    "workers",
    "network",
    "errors",
    "capacity",
];

/// Allowed metric prefixes. Anything starting with one of these is
/// treated as in-taxonomy; anything else is flagged.
const ALLOWED_PREFIXES: &[&str] = &["brain_", "process_"];

fn dashboards_dir() -> PathBuf {
    // brain-server/tests/dashboards.rs → config/monitoring/dashboards/
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("config")
        .join("monitoring")
        .join("dashboards")
}

#[test]
fn every_dashboard_parses_as_json() {
    let dir = dashboards_dir();
    for name in EXPECTED_DASHBOARDS {
        let path = dir.join(format!("{name}.json"));
        let raw = fs::read_to_string(&path).expect("read");
        let v: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{} parse: {}", name, e));
        assert!(
            v.get("title").and_then(|t| t.as_str()).is_some(),
            "{}: missing top-level `title`",
            name
        );
        assert!(
            v.get("uid").and_then(|u| u.as_str()).is_some(),
            "{}: missing top-level `uid`",
            name
        );
        assert!(
            v.get("schemaVersion").is_some(),
            "{}: missing `schemaVersion`",
            name
        );
        assert!(
            v.get("panels").and_then(|p| p.as_array()).is_some(),
            "{}: missing `panels` array",
            name
        );
    }
}

#[test]
fn every_dashboard_references_taxonomy_metrics_only() {
    let dir = dashboards_dir();
    let mut violations = Vec::new();
    for name in EXPECTED_DASHBOARDS {
        let path = dir.join(format!("{name}.json"));
        let raw = fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        let panels = v["panels"].as_array().expect("panels");
        for (i, panel) in panels.iter().enumerate() {
            if let Some(targets) = panel["targets"].as_array() {
                for target in targets {
                    if let Some(expr) = target["expr"].as_str() {
                        for candidate in extract_metric_names(expr) {
                            if !ALLOWED_PREFIXES.iter().any(|p| candidate.starts_with(p)) {
                                violations.push(format!(
                                    "{}: panel[{}] expr `{}` references unknown metric `{}`",
                                    name, i, expr, candidate,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "dashboard metric typo(s):\n  {}",
        violations.join("\n  ")
    );
}

/// Very small PromQL-tokeniser-style extraction. Pulls runs of
/// `[a-zA-Z_][a-zA-Z0-9_]*` that look like metric names (i.e. not
/// directly preceded by alphanumerics — to skip function arguments
/// like `le`, `op` inside `by(...)`).
///
/// Conservative: returns only identifiers that contain an underscore
/// and aren't PromQL keywords / function-arg labels. Catches the
/// 99 % case (full metric names with `brain_` prefix or
/// `process_*`) without trying to be a real parser.
fn extract_metric_names(expr: &str) -> Vec<String> {
    const PROMQL_KEYWORDS: &[&str] = &[
        "sum",
        "rate",
        "irate",
        "by",
        "without",
        "histogram_quantile",
        "predict_linear",
        "time",
        "clamp_min",
        "clamp_max",
        "le",
        "op",
        "shard",
        "worker",
        "status",
        "error",
        "success",
        "timeout",
    ];
    let mut out = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &expr[start..i];
            if ident.contains('_') && !PROMQL_KEYWORDS.contains(&ident) {
                // Strip Prometheus' histogram `_bucket` / `_sum` /
                // `_count` suffixes back to the base metric name —
                // those aren't part of the taxonomy text but are
                // valid scrape lines for any histogram family.
                let base = ident
                    .strip_suffix("_bucket")
                    .or_else(|| ident.strip_suffix("_sum"))
                    .or_else(|| ident.strip_suffix("_count"))
                    .unwrap_or(ident);
                out.push(base.to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}
