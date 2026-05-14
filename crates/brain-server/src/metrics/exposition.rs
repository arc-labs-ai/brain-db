//! Prometheus text-format exposition helpers.
//!
//! Bundles the boilerplate of emitting `# HELP` / `# TYPE` lines plus
//! the data lines for the four supported metric kinds. Used by
//! [`super::format`]; isolated here so unit tests can pin the exact
//! bytes the format produces.

use std::fmt::Write as _;

use super::counter::Counter;
use super::gauge::Gauge;
use super::histogram::Histogram;

/// Emit `# HELP` + `# TYPE` headers. `kind` is one of `counter`,
/// `gauge`, `histogram`, `summary`.
pub fn emit_header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Emit a labelless counter line. Pairs with [`emit_header`].
pub fn emit_counter(out: &mut String, name: &str, c: &Counter) {
    let _ = writeln!(out, "{name} {}", c.get());
}

/// Emit a counter line with the given label string. `labels` must
/// already be the full `{a="x",b="y"}` form including braces.
pub fn emit_counter_labeled(out: &mut String, name: &str, labels: &str, c: &Counter) {
    let _ = writeln!(out, "{name}{labels} {}", c.get());
}

/// Emit a labelless gauge line. The wire-format treats gauges and
/// counters identically; the difference is in the `# TYPE` header.
pub fn emit_gauge(out: &mut String, name: &str, g: &Gauge) {
    let _ = writeln!(out, "{name} {}", g.get());
}

/// Emit a gauge line with the given label string.
pub fn emit_gauge_labeled(out: &mut String, name: &str, labels: &str, g: &Gauge) {
    let _ = writeln!(out, "{name}{labels} {}", g.get());
}

/// Emit a histogram. `label_inner` is the inside-braces portion (e.g.
/// `op="encode"`); pass `""` for labelless histograms.
pub fn emit_histogram(out: &mut String, name: &str, label_inner: &str, h: &Histogram) {
    h.expose(name, label_inner, out);
}

/// Render a raw labels-only gauge line — the `*_info` pattern. Used
/// by `brain_build_info` and `brain_config_info` which carry their
/// payload in labels and always have value=1.
pub fn emit_info(out: &mut String, name: &str, labels: &str) {
    let _ = writeln!(out, "{name}{labels} 1");
}

/// Emit a plain `name value` line. Used for the few metrics that
/// don't fit a Counter/Gauge primitive — e.g. process uptime, which is
/// derived from `Instant::elapsed` at scrape time.
pub fn emit_scalar(out: &mut String, name: &str, value: u64) {
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_header_lines() {
        let mut out = String::new();
        emit_header(&mut out, "brain_test_total", "a test counter.", "counter");
        assert_eq!(
            out,
            "# HELP brain_test_total a test counter.\n# TYPE brain_test_total counter\n"
        );
    }

    #[test]
    fn emit_counter_lines() {
        let c = Counter::new();
        c.add(42);
        let mut out = String::new();
        emit_counter(&mut out, "brain_test_total", &c);
        assert_eq!(out, "brain_test_total 42\n");
    }

    #[test]
    fn emit_counter_labeled_lines() {
        let c = Counter::new();
        c.add(7);
        let mut out = String::new();
        emit_counter_labeled(&mut out, "brain_test_total", "{shard=\"0\"}", &c);
        assert_eq!(out, "brain_test_total{shard=\"0\"} 7\n");
    }

    #[test]
    fn emit_info_renders_labels_with_value_one() {
        let mut out = String::new();
        emit_info(
            &mut out,
            "brain_build_info",
            "{version=\"1.0\",commit=\"abc\"}",
        );
        assert_eq!(out, "brain_build_info{version=\"1.0\",commit=\"abc\"} 1\n");
    }

    #[test]
    fn emit_scalar_lines() {
        let mut out = String::new();
        emit_scalar(&mut out, "process_uptime_seconds", 42);
        assert_eq!(out, "process_uptime_seconds 42\n");
    }
}
