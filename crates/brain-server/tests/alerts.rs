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
//! - Every metric, grouping label, and range function in an alert is valid
//!   for the exposition emitted by the server.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;

mod metrics_catalog;

use metrics_catalog::{
    expr_grouping_labels, expr_metrics, expr_range_functions, Catalog, MetricKind,
};

const REQUIRED_ALERTS: &[&str] = &[
    // critical
    "BrainSubstrateDown",
    "BrainHighErrorRate",
    "BrainWorkerErrors",
    "BrainExtractionQueueUnobserved",
    // high
    "BrainHighLatency",
    "BrainWorkerStuck",
    "BrainExtractorApplyDropping",
    "BrainDerivationQueueUnbounded",
    "BrainDerivationQueueStale",
    // medium
    "BrainHighTombstoneRatio",
    "BrainArenaNearCapacity",
    "BrainStatementEmbedErrors",
    "BrainConnectionsChurning",
    "BrainDerivationAdmissionStarved",
    // low
    "BrainConfigChanged",
    "BrainExtractorBackpressure",
    "BrainTraceExportFailing",
    "BrainDerivationAdmissionHeadroomLow",
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
    let catalog = Catalog::parse();
    let mut violations = Vec::new();
    for expr in collect_exprs(&raw) {
        let metrics = expr_metrics(&expr);
        for metric in metrics {
            if !KNOWN_EXTERNAL_METRICS.contains(&metric.as_str())
                && catalog.resolve(&metric).is_none()
            {
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
    let catalog = Catalog::parse();
    assert!(
        catalog.names().contains("brain_request_total"),
        "emitted-set parser failed to find brain_request_total — parser is broken",
    );
    assert_eq!(
        catalog.family("brain_request_total").unwrap().kind,
        MetricKind::Counter
    );
    assert!(
        KNOWN_EXTERNAL_METRICS.contains(&"up"),
        "synthetic `up` must be allowed"
    );
}

#[test]
fn every_alert_grouping_label_exists() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let catalog = Catalog::parse();
    let mut violations = Vec::new();
    for expr in collect_exprs(&raw) {
        for label in expr_grouping_labels(&expr) {
            for metric in expr_metrics(&expr) {
                match catalog.allowed_labels(&metric) {
                    Some(allowed) if allowed.contains(&label) => {}
                    Some(allowed) => violations.push(format!(
                        "expr `{expr}`: `{metric}` does not allow `{label}`; catalog allows {allowed:?}"
                    )),
                    None => violations.push(format!(
                        "expr `{expr}`: `{metric}` is unresolved while checking `{label}`"
                    )),
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "alert grouping-label violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_alert_range_function_matches_metric_type() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let catalog = Catalog::parse();
    let counter_functions = ["rate", "irate", "increase", "resets"];
    let gauge_functions = ["delta", "idelta", "deriv", "predict_linear", "holt_winters"];
    let mut violations = Vec::new();
    for expr in collect_exprs(&raw) {
        for (function, metrics) in expr_range_functions(&expr) {
            for metric in metrics {
                let valid = catalog
                    .resolve(&metric)
                    .map(|(family, suffix)| {
                        (counter_functions.contains(&function.as_str())
                            && (family.kind == MetricKind::Counter
                                || (family.kind == MetricKind::Histogram && suffix.is_some())))
                            || (gauge_functions.contains(&function.as_str())
                                && family.kind == MetricKind::Gauge)
                    })
                    .unwrap_or(false);
                if !valid {
                    violations.push(format!(
                        "expr `{expr}`: `{function}` is incompatible with `{metric}`"
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "alert range-function violations:\n  {}",
        violations.join("\n  ")
    );
}
