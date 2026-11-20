//! Shared catalog of the metric families Brain exposes.

#![cfg(target_os = "linux")]
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

pub const SCRAPE_INJECTED_LABELS: &[&str] = &["instance", "job"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

#[derive(Debug)]
pub struct MetricFamily {
    pub name: String,
    pub kind: MetricKind,
    pub labels: BTreeSet<String>,
    pub site_count: usize,
}

#[derive(Debug)]
pub struct Catalog {
    families: BTreeMap<String, MetricFamily>,
    sites: BTreeSet<String>,
    unrecognized: Vec<(String, usize)>,
}

#[derive(Clone)]
struct LabelAssignment {
    offset: usize,
    ident: String,
    labels: BTreeSet<String>,
}

impl Catalog {
    pub fn parse() -> Catalog {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("metrics")
            .join("format.rs");
        let mut text = fs::read_to_string(path).expect("read metrics format.rs");
        if let Some(end) = text.find("#[cfg(test)]") {
            text.truncate(end);
        }

        let mut families = BTreeMap::new();
        for call in calls(&text, "emit_header") {
            let args = split_args(&text[call.1 + 1..call.2]);
            if args.len() >= 4 {
                if let (Some(name), Some(kind)) = (literal(&args[1]), literal(&args[3])) {
                    let kind = match kind {
                        "counter" => MetricKind::Counter,
                        "gauge" => MetricKind::Gauge,
                        "histogram" => MetricKind::Histogram,
                        "summary" => MetricKind::Summary,
                        _ => continue,
                    };
                    families.insert(
                        name.to_string(),
                        MetricFamily {
                            name: name.to_string(),
                            kind,
                            labels: BTreeSet::new(),
                            site_count: 0,
                        },
                    );
                }
            }
        }

        let assignments = label_assignments(&text);
        let mut sites = BTreeSet::new();
        let mut unrecognized = Vec::new();
        for (macro_name, start, end) in all_calls(&text) {
            if macro_name == "emit_header" {
                continue;
            }
            let args = split_args(&text[start + 1..end]);
            let site = if macro_name == "writeln!" {
                args.get(1)
                    .and_then(|arg| writeln_site(arg, start, &assignments))
            } else if matches!(
                macro_name.as_str(),
                "emit_counter" | "emit_gauge" | "emit_scalar"
            ) {
                args.get(1)
                    .and_then(|arg| literal(arg))
                    .map(|name| (name.to_string(), BTreeSet::new()))
            } else if matches!(
                macro_name.as_str(),
                "emit_counter_labeled"
                    | "emit_gauge_labeled"
                    | "emit_info"
                    | "emit_histogram"
                    | "emit_worker_histogram"
            ) {
                let name = args.get(1).and_then(|arg| literal(arg));
                let labels = args.get(2).and_then(|arg| {
                    let arg = arg.trim();
                    if let Some(value) = literal(arg) {
                        Some(keys(value))
                    } else {
                        let ident = arg.strip_prefix('&').unwrap_or(arg).trim();
                        if is_ident(ident) {
                            resolve_assignment(&assignments, ident, start).map(|a| a.labels)
                        } else {
                            None
                        }
                    }
                });
                match (name, labels) {
                    (Some(name), Some(labels)) => Some((name.to_string(), labels)),
                    _ => None,
                }
            } else {
                continue;
            };

            let line = text[..start].bytes().filter(|b| *b == b'\n').count() + 1;
            if let Some((name, labels)) = site {
                if let Some(family) = families.get_mut(&name) {
                    family.site_count += 1;
                    family.labels.extend(labels);
                    sites.insert(name);
                } else if name.starts_with("brain_") || name.starts_with("process_") {
                    unrecognized.push((name, line));
                }
            } else if macro_name == "writeln!" {
                if let Some(arg) = args.get(1) {
                    if let Some(name) = writeln_metric_name(arg) {
                        unrecognized.push((name.to_string(), line));
                    }
                }
            } else if macro_name.starts_with("emit_") {
                let name = args
                    .get(1)
                    .and_then(|arg| literal(arg))
                    .unwrap_or("<unresolved>")
                    .to_string();
                unrecognized.push((name, line));
            }
        }

        Catalog {
            families,
            sites,
            unrecognized,
        }
    }

    pub fn names(&self) -> BTreeSet<String> {
        self.families.keys().cloned().collect()
    }

    pub fn family(&self, name: &str) -> Option<&MetricFamily> {
        self.families.get(name)
    }

    pub fn resolve(&self, referenced: &str) -> Option<(&MetricFamily, Option<&'static str>)> {
        if let Some(family) = self.families.get(referenced) {
            return Some((family, None));
        }
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(stem) = referenced.strip_suffix(suffix) {
                if let Some(family) = self.families.get(stem) {
                    if family.kind == MetricKind::Histogram {
                        return Some((family, Some(suffix)));
                    }
                }
            }
        }
        None
    }

    pub fn allowed_labels(&self, referenced: &str) -> Option<BTreeSet<String>> {
        let (family, suffix) = self.resolve(referenced)?;
        let mut labels = family.labels.clone();
        labels.extend(
            SCRAPE_INJECTED_LABELS
                .iter()
                .map(|label| (*label).to_string()),
        );
        if suffix == Some("_bucket") {
            labels.insert("le".to_string());
        }
        Some(labels)
    }

    pub fn unrecognized_sites(&self) -> &[(String, usize)] {
        &self.unrecognized
    }
}

pub fn expr_metrics(expr: &str) -> Vec<String> {
    identifiers(expr)
        .into_iter()
        .filter(|name| name.starts_with("brain_") || name.starts_with("process_"))
        .collect()
}

pub fn expr_grouping_labels(expr: &str) -> Vec<String> {
    clause_labels(expr, &["by", "without", "on", "ignoring"])
}

pub fn expr_selector_labels(expr: &str) -> Vec<(String, Vec<String>)> {
    let ids = expr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        if ids[i] == b'{' {
            let mut start = i;
            while start > 0 && (ids[start - 1].is_ascii_alphanumeric() || ids[start - 1] == b'_') {
                start -= 1;
            }
            let name = &expr[start..i];
            if !name.is_empty() {
                if let Some(end) = balanced_end(expr, i, b'{', b'}') {
                    let mut labels = Vec::new();
                    let body = &expr[i + 1..end];
                    for part in body.split(',') {
                        let key = part.trim().split_once('=').map(|(key, _)| key.trim());
                        if let Some(key) = key.filter(|key| is_ident(key)) {
                            labels.push(key.to_string());
                        }
                    }
                    out.push((name.to_string(), labels));
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

pub fn expr_range_functions(expr: &str) -> Vec<(String, Vec<String>)> {
    const FUNCTIONS: &[&str] = &[
        "rate",
        "irate",
        "increase",
        "resets",
        "delta",
        "idelta",
        "deriv",
        "predict_linear",
        "holt_winters",
    ];
    let mut out = Vec::new();
    for (name, start, end) in function_calls(expr) {
        if FUNCTIONS.contains(&name.as_str()) {
            out.push((name, expr_metrics(&expr[start + 1..end])));
        }
    }
    out
}

fn all_calls(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    for name in [
        "writeln!",
        "emit_header",
        "emit_counter",
        "emit_gauge",
        "emit_scalar",
        "emit_counter_labeled",
        "emit_gauge_labeled",
        "emit_info",
        "emit_histogram",
        "emit_worker_histogram",
    ] {
        out.extend(
            calls(text, name)
                .into_iter()
                .map(|(_, start, end)| (name.to_string(), start, end)),
        );
    }
    out.sort_by_key(|(_, start, _)| *start);
    out
}

fn function_calls(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'"' {
            i = string_end(text, i).map(|end| end + 1).unwrap_or(text.len());
            continue;
        }
        if text.as_bytes()[i].is_ascii_alphabetic() || text.as_bytes()[i] == b'_' {
            let start = i;
            i += 1;
            while i < text.len()
                && (text.as_bytes()[i].is_ascii_alphanumeric() || text.as_bytes()[i] == b'_')
            {
                i += 1;
            }
            let name = &text[start..i];
            let mut open = i;
            while open < text.len() && text.as_bytes()[open].is_ascii_whitespace() {
                open += 1;
            }
            if open < text.len() && text.as_bytes()[open] == b'(' {
                if let Some(close) = balanced_end(text, open, b'(', b')') {
                    out.push((name.to_string(), open, close));
                    i = close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn calls(text: &str, wanted: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'"' {
            i = string_end(text, i).map(|end| end + 1).unwrap_or(text.len());
            continue;
        }
        if text.as_bytes()[i..].starts_with(wanted.as_bytes())
            && (i == 0 || !is_ident_char(text.as_bytes()[i - 1]))
        {
            if text[..i].trim_end().ends_with("fn") {
                i += wanted.len();
                continue;
            }
            let open = i + wanted.len();
            if text[open..].starts_with('(') {
                if let Some(close) = balanced_end(text, open, b'(', b')') {
                    out.push((wanted.to_string(), open, close));
                    i = close + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'"' {
            i = string_end(text, i).map(|end| end + 1).unwrap_or(text.len());
            continue;
        }
        match text.as_bytes()[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(text[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < text.len() {
        out.push(text[start..].trim().to_string());
    }
    out
}

fn literal(text: &str) -> Option<&str> {
    let text = text.trim();
    if !text.starts_with('"') {
        return None;
    }
    let end = string_end(text, 0)?;
    Some(&text[1..end])
}

fn label_assignments(text: &str) -> Vec<LabelAssignment> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = text[i..].find("let ") {
        let start = i + pos;
        let rest = &text[start + 4..];
        let ident_end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(0);
        let ident = &rest[..ident_end];
        let statement_end = rest.find(';').unwrap_or(rest.len());
        let statement = &rest[..statement_end];
        if let Some(fmt) = statement.find("format!") {
            let format_end = fmt + "format!".len();
            if let Some(open_quote) = statement[format_end..].find('"') {
                let quote = start + 4 + format_end + open_quote;
                if let Some(end) = string_end(text, quote) {
                    out.push(LabelAssignment {
                        offset: start,
                        ident: ident.to_string(),
                        labels: keys(&text[quote + 1..end]),
                    });
                }
            }
        }
        i = start + 4;
    }
    out
}

fn resolve_assignment(
    assignments: &[LabelAssignment],
    ident: &str,
    offset: usize,
) -> Option<LabelAssignment> {
    assignments
        .iter()
        .filter(|a| a.ident == ident && a.offset < offset)
        .max_by_key(|a| a.offset)
        .cloned()
}

fn writeln_site(
    arg: &str,
    offset: usize,
    assignments: &[LabelAssignment],
) -> Option<(String, BTreeSet<String>)> {
    let fmt = literal(arg)?;
    let end = metric_name_end(fmt);
    let name = &fmt[..end];
    if !(name.starts_with("brain_") || name.starts_with("process_")) {
        return None;
    }
    let rest = &fmt[end..];
    let labels = if rest.starts_with("{{") {
        let close = rest.find("}}").unwrap_or(rest.len());
        keys(&rest[2..close])
    } else if rest.starts_with('{') {
        let close = rest.find('}')?;
        let ident = &rest[1..close];
        resolve_assignment(assignments, ident, offset)?.labels
    } else if rest.is_empty() || rest.starts_with(' ') {
        BTreeSet::new()
    } else {
        return None;
    };
    Some((name.to_string(), labels))
}

fn writeln_metric_name(arg: &str) -> Option<&str> {
    let fmt = literal(arg)?;
    let end = metric_name_end(fmt);
    let name = &fmt[..end];
    if name.starts_with("brain_") || name.starts_with("process_") {
        Some(name)
    } else {
        None
    }
}

fn metric_name_end(fmt: &str) -> usize {
    fmt.find(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_')
        .unwrap_or(fmt.len())
}

fn keys(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i].is_ascii_lowercase() || bytes[i] == b'_')
            && (i == 0 || !is_ident_char(bytes[i - 1]))
        {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit() || bytes[i] == b'_')
            {
                i += 1;
            }
            if text[i..].starts_with("=\\\"") {
                out.insert(text[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

fn identifiers(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(text[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

fn clause_labels(expr: &str, clauses: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for clause in clauses {
        let mut from = 0;
        while let Some(pos) = expr[from..].find(clause) {
            let clause_start = from + pos;
            let clause_end = clause_start + clause.len();
            if clause_start > 0 && is_ident_char(expr.as_bytes()[clause_start - 1]) {
                from = clause_end;
                continue;
            }
            let start = clause_end;
            let rest = &expr[start..];
            let whitespace = rest.len() - rest.trim_start().len();
            if rest[whitespace..].starts_with('(') {
                let open = start + whitespace;
                if let Some(end) = balanced_end(expr, open, b'(', b')') {
                    out.extend(identifiers(&expr[open + 1..end]));
                    from = end + 1;
                    continue;
                }
            }
            from = start;
        }
    }
    out
}

fn balanced_end(text: &str, open: usize, left: u8, right: u8) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < text.len() {
        if text.as_bytes()[i] == b'"' {
            i = string_end(text, i).map(|end| end + 1).unwrap_or(text.len());
            continue;
        }
        if text.as_bytes()[i] == left {
            depth += 1;
        }
        if text.as_bytes()[i] == right {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn string_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == b'"' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn is_ident(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(is_ident_char)
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[test]
fn catalog_resolves_known_families() {
    let c = Catalog::parse();
    for (name, kind, labels) in [
        (
            "brain_request_total",
            MetricKind::Counter,
            &["op", "status"][..],
        ),
        ("brain_request_active", MetricKind::Gauge, &["op"][..]),
        (
            "brain_request_duration_ms",
            MetricKind::Histogram,
            &["op"][..],
        ),
        (
            "brain_worker_cycles_total",
            MetricKind::Counter,
            &["shard", "worker"][..],
        ),
        (
            "brain_frame_size_bytes",
            MetricKind::Histogram,
            &["direction"][..],
        ),
        (
            "brain_connections_closed_total",
            MetricKind::Counter,
            &["reason"][..],
        ),
        ("brain_frame_recv_total", MetricKind::Counter, &[][..]),
        ("brain_connections_active", MetricKind::Gauge, &[][..]),
        ("process_memory_resident_bytes", MetricKind::Gauge, &[][..]),
        ("process_memory_virtual_bytes", MetricKind::Gauge, &[][..]),
        (
            "brain_writer_stage_duration_ms",
            MetricKind::Histogram,
            &["shard", "stage"][..],
        ),
        (
            "brain_query_latency_ms",
            MetricKind::Histogram,
            &["shard"][..],
        ),
        (
            "brain_extraction_queue_depth",
            MetricKind::Gauge,
            &["shard"][..],
        ),
        ("brain_hnsw_node_count", MetricKind::Gauge, &["shard"][..]),
        (
            "brain_writer_submit_total",
            MetricKind::Counter,
            &["shard", "phase", "outcome"][..],
        ),
        ("brain_query_total", MetricKind::Counter, &["shard"][..]),
    ] {
        let family = c.family(name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(family.kind, kind, "kind for {name}");
        assert_eq!(
            family.labels,
            labels.iter().map(|s| (*s).to_string()).collect()
        );
    }
    assert!(c.resolve("brain_schema_version").is_none());
}

#[test]
fn catalog_has_no_unrecognized_emission_sites() {
    let catalog = Catalog::parse();
    assert!(
        catalog.unrecognized_sites().is_empty(),
        "unrecognized sites: {:?}",
        catalog.unrecognized_sites()
    );
}

#[test]
fn every_declared_family_is_emitted_and_every_emitted_family_is_declared() {
    let c = Catalog::parse();
    let declared = c.names();
    let emitted = c.sites.clone();
    let diff: BTreeSet<_> = declared.symmetric_difference(&emitted).cloned().collect();
    assert!(
        diff.is_empty(),
        "header/site symmetric difference: {diff:?}"
    );
}

#[test]
fn catalog_is_not_vacuous() {
    let c = Catalog::parse();
    assert!(c.names().len() >= 90);
    assert!(c
        .allowed_labels("brain_request_duration_ms_bucket")
        .unwrap()
        .contains("le"));
    assert!(!c
        .allowed_labels("brain_request_total")
        .unwrap()
        .contains("le"));
}
