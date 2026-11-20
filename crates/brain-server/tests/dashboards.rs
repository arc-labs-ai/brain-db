//! CI gate for shipped Grafana dashboards.
//!
//! Verifies JSON shape, emitted metric families, legal labels, and PromQL
//! range functions. These checks keep dashboards aligned with the exposition
//! so an operator does not mistake an empty or invalid series for zero.

#![cfg(target_os = "linux")]

mod metrics_catalog;

use std::fs;
use std::path::PathBuf;

use metrics_catalog::{
    expr_grouping_labels, expr_metrics, expr_range_functions, expr_selector_labels, Catalog,
    MetricKind,
};

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

fn dashboards_dir() -> PathBuf {
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

fn dashboard_exprs() -> Vec<(String, usize, String, String)> {
    let dir = dashboards_dir();
    let mut out = Vec::new();
    for name in EXPECTED_DASHBOARDS {
        let raw = fs::read_to_string(dir.join(format!("{name}.json"))).expect("read");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        for (index, panel) in value["panels"]
            .as_array()
            .expect("panels")
            .iter()
            .enumerate()
        {
            let title = panel["title"].as_str().unwrap_or("<untitled>").to_string();
            if let Some(targets) = panel["targets"].as_array() {
                for target in targets {
                    if let Some(expr) = target["expr"].as_str() {
                        out.push(((*name).to_string(), index, title.clone(), expr.to_string()));
                    }
                }
            }
        }
    }
    out
}

fn context(dashboard: &str, panel: usize, title: &str, expr: &str, detail: &str) -> String {
    format!("{dashboard}: panel[{panel}] `{title}`, expr `{expr}`: {detail}")
}

#[test]
fn every_dashboard_metric_is_emitted() {
    let catalog = Catalog::parse();
    let mut violations = Vec::new();
    for (dashboard, panel, title, expr) in dashboard_exprs() {
        for metric in expr_metrics(&expr) {
            if catalog.resolve(&metric).is_none() {
                violations.push(context(
                    &dashboard,
                    panel,
                    &title,
                    &expr,
                    &format!("catalog has no family for `{metric}`"),
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "dashboard metric violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_dashboard_grouping_label_exists() {
    let catalog = Catalog::parse();
    let mut violations = Vec::new();
    for (dashboard, panel, title, expr) in dashboard_exprs() {
        for label in expr_grouping_labels(&expr) {
            for metric in expr_metrics(&expr) {
                match catalog.allowed_labels(&metric) {
                    Some(allowed) if allowed.contains(&label) => {}
                    Some(allowed) => violations.push(context(
                        &dashboard,
                        panel,
                        &title,
                        &expr,
                        &format!(
                            "`{metric}` does not allow grouping label `{label}`; catalog allows {allowed:?}"
                        ),
                    )),
                    None => violations.push(context(
                        &dashboard,
                        panel,
                        &title,
                        &expr,
                        &format!(
                            "`{metric}` is unresolved while checking grouping label `{label}`"
                        ),
                    )),
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "dashboard grouping-label violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_dashboard_selector_label_exists() {
    let catalog = Catalog::parse();
    let mut violations = Vec::new();
    for (dashboard, panel, title, expr) in dashboard_exprs() {
        for (metric, labels) in expr_selector_labels(&expr) {
            for label in labels {
                match catalog.allowed_labels(&metric) {
                    Some(allowed) if allowed.contains(&label) => {}
                    Some(allowed) => violations.push(context(
                        &dashboard,
                        panel,
                        &title,
                        &expr,
                        &format!(
                            "`{metric}` does not allow selector label `{label}`; catalog allows {allowed:?}"
                        ),
                    )),
                    None => violations.push(context(
                        &dashboard,
                        panel,
                        &title,
                        &expr,
                        &format!(
                            "selector metric `{metric}` is unresolved for label `{label}`"
                        ),
                    )),
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "dashboard selector-label violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_dashboard_range_function_matches_metric_type() {
    let catalog = Catalog::parse();
    let counter_functions = ["rate", "irate", "increase", "resets"];
    let gauge_functions = ["delta", "idelta", "deriv", "predict_linear", "holt_winters"];
    let mut violations = Vec::new();
    for (dashboard, panel, title, expr) in dashboard_exprs() {
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
                    violations.push(context(
                        &dashboard,
                        panel,
                        &title,
                        &expr,
                        &format!(
                            "range function `{function}` is incompatible with catalog metric `{metric}`"
                        ),
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "dashboard range-function violations:\n  {}",
        violations.join("\n  ")
    );
}
