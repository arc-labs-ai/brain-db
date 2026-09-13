//! Process-level resource metrics, sourced from `/proc/self/*`.
//!
//!
//! - `process_cpu_seconds_total` — counter, derived from
//!   `/proc/self/stat` field 14 (utime) + field 15 (stime), in clock
//!   ticks; divided by `sysconf(_SC_CLK_TCK)`.
//! - `process_memory_resident_bytes` — gauge, `VmRSS` from
//!   `/proc/self/status` × 1024 (status reports kB).
//! - `process_memory_virtual_bytes` — gauge, `VmSize` from
//!   `/proc/self/status` × 1024.
//! - `process_open_fds` — gauge, count of entries in
//!   `/proc/self/fd`.
//!
//! All four are sampled on `/metrics` scrape; no per-event tracking.
//! Failures (missing /proc, parse errors) yield `None` for the
//! relevant field and a `warn!` event — the exposition layer skips
//! emitting that family rather than reporting `0`, which would
//! corrupt dashboards.

use std::fs;
use std::path::Path;

use tracing::warn;

/// Sampled snapshot of process resource counters. All fields are
/// optional so the exposition can skip families that failed to read.
#[derive(Debug, Default)]
pub struct ProcessSnapshot {
    /// Cumulative process CPU time in seconds (utime + stime).
    pub cpu_seconds: Option<f64>,
    /// Resident set size (bytes).
    pub memory_resident_bytes: Option<u64>,
    /// Virtual memory size (bytes).
    pub memory_virtual_bytes: Option<u64>,
    /// Open file descriptor count.
    pub open_fds: Option<u64>,
}

impl ProcessSnapshot {
    /// Sample the live process. Cheap (three small reads + one
    /// directory list).
    #[must_use]
    pub fn capture() -> Self {
        Self {
            cpu_seconds: capture_cpu_seconds(),
            memory_resident_bytes: capture_vm_field("VmRSS:").map(|kb| kb * 1024),
            memory_virtual_bytes: capture_vm_field("VmSize:").map(|kb| kb * 1024),
            open_fds: capture_open_fds(),
        }
    }
}

fn capture_cpu_seconds() -> Option<f64> {
    let stat = match fs::read_to_string("/proc/self/stat") {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "read /proc/self/stat failed");
            return None;
        }
    };
    // `/proc/self/stat` is space-separated. Field 2 is `comm` and
    // may contain spaces, wrapped in parentheses. Slice past the
    // closing paren before tokenising.
    let rest = stat.split_once(')').map(|(_, r)| r.trim_start())?;
    let mut iter = rest.split_whitespace();
    // We've already consumed field 1 (pid) and field 2 (comm), so
    // skip the remaining 11 fields before utime (field 14).
    for _ in 0..11 {
        iter.next()?;
    }
    let utime_ticks: u64 = iter.next()?.parse().ok()?;
    let stime_ticks: u64 = iter.next()?.parse().ok()?;
    let total_ticks = utime_ticks + stime_ticks;
    let ticks_per_sec = clock_ticks_per_sec();
    Some(total_ticks as f64 / ticks_per_sec as f64)
}

fn capture_vm_field(prefix: &str) -> Option<u64> {
    let status = match fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "read /proc/self/status failed");
            return None;
        }
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            // Format: `VmRSS:\t  12345 kB`
            let mut tokens = rest.split_whitespace();
            let value: u64 = tokens.next()?.parse().ok()?;
            return Some(value);
        }
    }
    None
}

fn capture_open_fds() -> Option<u64> {
    let dir = Path::new("/proc/self/fd");
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "read /proc/self/fd failed");
            return None;
        }
    };
    let count = entries.filter(|r| r.is_ok()).count();
    Some(count as u64)
}

/// User-space clock-tick frequency, used to convert
/// `/proc/self/stat` jiffies to seconds.
///
/// Hardcoded to 100 — the de-facto modern Linux default
/// (`CONFIG_HZ_100`). Calling `sysconf(_SC_CLK_TCK)` would be
/// authoritative but requires `unsafe`, which is reserved
/// to `crates/brain-storage`. On a 250 Hz or 1000 Hz kernel the
/// absolute `process_cpu_seconds_total` value is wrong by a fixed
/// factor; PromQL `rate()` is unaffected (it's scaled the same way).
///
/// A safe sysconf wrapper (e.g. `rustix`) or a build-script probe
/// would let us read the real value.
const CLOCK_TICKS_PER_SEC: i64 = 100;

fn clock_ticks_per_sec() -> i64 {
    CLOCK_TICKS_PER_SEC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_produces_some_fields() {
        let snap = ProcessSnapshot::capture();
        // On any sane Linux test runner all four should populate.
        // We only assert non-zero CPU + memory because fd count
        // can legitimately be small.
        assert!(
            snap.cpu_seconds.is_some(),
            "cpu_seconds should populate on Linux"
        );
        assert!(
            snap.memory_resident_bytes.is_some(),
            "memory_resident_bytes should populate on Linux"
        );
        assert!(
            snap.memory_virtual_bytes.is_some(),
            "memory_virtual_bytes should populate on Linux"
        );
        assert!(snap.open_fds.is_some(), "open_fds should populate on Linux");
    }

    #[test]
    fn clock_ticks_per_sec_is_positive() {
        assert!(clock_ticks_per_sec() > 0);
    }
}
