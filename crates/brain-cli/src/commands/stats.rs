//! `brain-cli stats` — snapshots `/metrics` (Prometheus text
//! format) into structured output.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::http::get;
use crate::output::{json, table};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MetricSample {
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// A snapshot of the parsed `/metrics` response. Keys are metric
/// names (e.g. `brain_connections_total`); each maps to one or
/// more samples (one per label set).
pub type StatsReport = BTreeMap<String, Vec<MetricSample>>;

pub fn run(server: &str, output: OutputFormat) -> anyhow::Result<String> {
    let resp = get(server, "/metrics")?;
    if resp.status != 200 {
        anyhow::bail!("/metrics returned HTTP {}", resp.status);
    }
    let report = parse_prom_text(&resp.body);
    match output {
        OutputFormat::Json => json::render(&report),
        OutputFormat::Table => {
            let mut rows: Vec<(String, String)> = Vec::with_capacity(report.len());
            for (name, samples) in &report {
                for s in samples {
                    let labels = if s.labels.is_empty() {
                        String::new()
                    } else {
                        let inner: Vec<String> =
                            s.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        format!("{{{}}}", inner.join(","))
                    };
                    rows.push((format!("{name}{labels}"), format!("{}", s.value)));
                }
            }
            Ok(table::render_kv(&rows))
        }
    }
}

/// Tiny Prometheus text-format parser. Handles the subset
/// brain-server emits (line-per-sample, no HISTOGRAM
/// _bucket reconstruction, no escape sequences in labels).
/// Comments and empty lines are skipped.
pub fn parse_prom_text(body: &str) -> StatsReport {
    let mut out: StatsReport = BTreeMap::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((sample, value)) = split_value(line) else {
            continue;
        };
        let (name, labels) = match sample.find('{') {
            Some(open) => {
                let close = match sample.rfind('}') {
                    Some(c) if c > open => c,
                    _ => continue,
                };
                let name = &sample[..open];
                let inner = &sample[open + 1..close];
                let labels = parse_labels(inner);
                (name.to_string(), labels)
            }
            None => (sample.to_string(), BTreeMap::new()),
        };
        let value: f64 = match value.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.entry(name)
            .or_default()
            .push(MetricSample { labels, value });
    }
    out
}

/// Split a metric line into `(sample, value)`. Last whitespace-
/// separated token is the value; everything before it is the
/// sample name + optional `{labels}`.
fn split_value(line: &str) -> Option<(&str, &str)> {
    let (sample, rest) = match line.rfind(char::is_whitespace) {
        Some(idx) => (line[..idx].trim(), line[idx..].trim()),
        None => return None,
    };
    if sample.is_empty() || rest.is_empty() {
        return None;
    }
    Some((sample, rest))
}

fn parse_labels(inner: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // Naive split on `,`; values are `"..."`-quoted.
    for kv in inner.split(',') {
        let kv = kv.trim();
        let Some(eq) = kv.find('=') else { continue };
        let k = kv[..eq].trim().to_string();
        let v = kv[eq + 1..].trim().trim_matches('"').to_string();
        if !k.is_empty() {
            out.insert(k, v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_counter() {
        let body = "# HELP brain_connections_total Total connections accepted.\n\
                    # TYPE brain_connections_total counter\n\
                    brain_connections_total 12\n";
        let report = parse_prom_text(body);
        assert_eq!(report["brain_connections_total"].len(), 1);
        assert_eq!(report["brain_connections_total"][0].value, 12.0);
        assert!(report["brain_connections_total"][0].labels.is_empty());
    }

    #[test]
    fn parses_labeled_metric() {
        let body = r#"brain_admin_requests_total{endpoint="/metrics"} 3
brain_admin_requests_total{endpoint="/healthz"} 1
"#;
        let report = parse_prom_text(body);
        let samples = &report["brain_admin_requests_total"];
        assert_eq!(samples.len(), 2);
        let mut endpoints: Vec<&str> = samples
            .iter()
            .map(|s| s.labels["endpoint"].as_str())
            .collect();
        endpoints.sort();
        assert_eq!(endpoints, vec!["/healthz", "/metrics"]);
    }

    #[test]
    fn ignores_unparseable_lines() {
        let body = "garbage line with no number\nbrain_x 5\n";
        let report = parse_prom_text(body);
        assert_eq!(report.len(), 1);
        assert_eq!(report["brain_x"][0].value, 5.0);
    }

    #[test]
    fn ignores_comments_and_blanks() {
        let body = "\n# comment\n\nbrain_y 42\n";
        let report = parse_prom_text(body);
        assert_eq!(report.len(), 1);
        assert_eq!(report["brain_y"][0].value, 42.0);
    }
}
