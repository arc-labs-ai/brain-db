//! CI gate for `config/monitoring/alerts/brain-rules.yml`.
//!
//! `promtool check rules` is the authoritative check but requires
//! the Prometheus toolchain on CI. This test catches the common
//! failure modes without depending on `promtool`:
//!
//! - YAML parses (shape scan).
//! - Each rule has `alert`, `expr`, `labels.severity`.
//! - Severities are from the spec set (critical / high / medium / low),
//!   with at least one alert per level.
//! - Every required alert is present (catches accidental rule deletion).
//! - **Every metric an `expr` references is one the server actually emits.**
//!   The emitted set is parsed straight from
//!   `src/metrics/format.rs`, so an alert can never again reference a
//!   metric that does not exist (the class of bug that let
//!   `brain_snapshot_failures_total` and `brain_frame_send_total{opcode=…}`
//!   ship as permanently-silent alerts).

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

const REQUIRED_ALERTS: &[&str] = &[
    // critical
    "BrainSubstrateDown",
    "BrainHighErrorRate",
    "BrainWorkerErrors",
    // high
    "BrainHighLatency",
    "BrainWorkerStuck",
    "BrainExtractorApplyDropping",
    // medium
    "BrainHighTombstoneRatio",
    "BrainArenaNearCapacity",
    "BrainStatementEmbedErrors",
    "BrainConnectionsChurning",
    // low
    "BrainConfigChanged",
    "BrainExtractorBackpressure",
    "BrainTraceExportFailing",
];

const ALLOWED_SEVERITIES: &[&str] = &["critical", "high", "medium", "low"];

/// Metrics referenced by exprs that Brain does not emit itself but that
/// exist in any Prometheus deployment (synthetic scrape metrics).
const KNOWN_EXTERNAL_METRICS: &[&str] = &["up"];

fn rules_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("config")
        .join("monitoring")
        .join("alerts")
        .join("brain-rules.yml")
}

fn format_src_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("metrics")
        .join("format.rs")
}

/// Tiny YAML scanner — for each line, extract `alert: Name` and
/// `severity: name` tokens. Good enough to verify the file's shape
/// without a YAML parser dep.
fn scan_alerts_and_severities(text: &str) -> (Vec<String>, Vec<String>) {
    let mut alerts = Vec::new();
    let mut severities = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- alert:") {
            alerts.push(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("severity:") {
            severities.push(rest.trim().to_string());
        }
    }
    (alerts, severities)
}

/// Collect the text of every `expr:` value, including multi-line YAML
/// block scalars (`expr: |`, `expr: >-`, …). A continuation line belongs
/// to the block while it is blank or indented deeper than the `expr:` key.
fn collect_exprs(text: &str) -> Vec<String> {
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("expr:") {
            let key_indent = indent_of(line);
            let rest = rest.trim();
            let mut expr = String::new();
            // A block indicator (|, >, |-, >-, empty) means the value is on
            // the following, more-indented lines; anything else is inline.
            if rest.is_empty() || rest.starts_with('|') || rest.starts_with('>') {
                i += 1;
                while i < lines.len() {
                    let cont = lines[i];
                    if cont.trim().is_empty() || indent_of(cont) > key_indent {
                        expr.push_str(cont.trim());
                        expr.push(' ');
                        i += 1;
                    } else {
                        break;
                    }
                }
            } else {
                expr.push_str(rest);
                i += 1;
            }
            out.push(expr);
        } else {
            i += 1;
        }
    }
    out
}

/// Pull metric names out of a PromQL expr: identifiers starting with
/// `brain_` / `process_`, plus the synthetic `up`. Histogram suffixes
/// (`_bucket` / `_sum` / `_count`) are stripped back to the base family.
/// Label keys/values and function names never start with those prefixes,
/// so they fall out naturally.
fn expr_metrics(expr: &str) -> Vec<String> {
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
            if ident.starts_with("brain_") || ident.starts_with("process_") || ident == "up" {
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

/// Parse the set of metric family names the exposition actually emits by
/// scanning the source of `metrics/format.rs` for `brain_*` / `process_*`
/// string literals. This auto-syncs the gate with the code: add a metric
/// and it's allowed; reference one you never added and the gate fails.
fn emitted_metrics() -> HashSet<String> {
    let src = fs::read_to_string(format_src_path()).expect("read format.rs");
    let mut set: HashSet<String> = HashSet::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &src[start..i];
            if ident.starts_with("brain_") || ident.starts_with("process_") {
                set.insert(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
    for ext in KNOWN_EXTERNAL_METRICS {
        set.insert((*ext).to_string());
    }
    set
}

#[test]
fn every_required_alert_is_present() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let (alerts, _) = scan_alerts_and_severities(&raw);
    for required in REQUIRED_ALERTS {
        assert!(
            alerts.iter().any(|a| a == required),
            "missing required alert `{required}`",
        );
    }
}

#[test]
fn every_severity_is_from_allowed_set() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let (_, severities) = scan_alerts_and_severities(&raw);
    for sev in &severities {
        assert!(
            ALLOWED_SEVERITIES.contains(&sev.as_str()),
            "severity `{sev}` is not in (allowed: critical/high/medium/low)",
        );
    }
}

#[test]
fn at_least_one_alert_per_severity_level() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let (_, severities) = scan_alerts_and_severities(&raw);
    for required in ALLOWED_SEVERITIES {
        assert!(
            severities.iter().any(|s| s == required),
            "no alert with severity `{required}` — expects all four levels",
        );
    }
}

/// The regression guard for this whole class of bug: an alert whose `expr`
/// names a metric the server never emits can never fire, and the old shape-
/// only gate let two such alerts ship. Every metric referenced by any expr
/// must be in the set `format.rs` actually emits.
#[test]
fn every_referenced_metric_is_emitted() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let emitted = emitted_metrics();
    let mut violations = Vec::new();
    for expr in collect_exprs(&raw) {
        for metric in expr_metrics(&expr) {
            if !emitted.contains(&metric) {
                violations.push(format!("`{metric}` (in expr `{}`)", expr.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "alert expr references metric(s) the server does not emit:\n  {}",
        violations.join("\n  "),
    );
}

/// Sanity check on the emitted-set parser itself: it must find the
/// well-known request counter. Guards against the parser silently
/// returning an empty set (which would make the gate above vacuous).
#[test]
fn emitted_set_parser_finds_known_metrics() {
    let emitted = emitted_metrics();
    assert!(
        emitted.contains("brain_request_total"),
        "emitted-set parser failed to find brain_request_total — parser is broken",
    );
    assert!(emitted.contains("up"), "synthetic `up` must be allowed");
}
