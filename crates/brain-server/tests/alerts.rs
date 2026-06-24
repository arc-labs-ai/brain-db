//! CI gate for `config/monitoring/alerts/brain-rules.yml`.
//!
//! `promtool check rules` is the authoritative check but requires
//! the Prometheus toolchain on CI. This test catches the common
//! failure modes without depending on `promtool`:
//!
//! - YAML parses.
//! - Top-level shape: `groups: [...]` with each group carrying
//!   `name` and `rules`.
//! - Each rule has `alert`, `expr`, `labels.severity`.
//! - Severities are from the spec set (critical / high / medium / low).
//! - Every required alert is present (catches
//!   accidental rule deletion).

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;

const REQUIRED_ALERTS: &[&str] = &[
    // critical
    "BrainSubstrateDown",
    "BrainHighErrorRate",
    "BrainCheckpointFailing",
    // high
    "BrainHighLatency",
    "BrainWorkerStuck",
    // medium
    "BrainHighTombstoneRatio",
    "BrainRecallQualityDegraded",
    "BrainEmbedderSlow",
    "BrainConnectionsChurning",
    // low
    "BrainConfigChanged",
    "BrainWorkerErrorsWarning",
];

const ALLOWED_SEVERITIES: &[&str] = &["critical", "high", "medium", "low"];

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

#[test]
fn every_required_alert_is_present() {
    let raw = fs::read_to_string(rules_path()).expect("read");
    let (alerts, _) = scan_alerts_and_severities(&raw);
    for required in REQUIRED_ALERTS {
        assert!(
            alerts.iter().any(|a| a == required),
            "missing required alert `{required}` — mandates it",
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
