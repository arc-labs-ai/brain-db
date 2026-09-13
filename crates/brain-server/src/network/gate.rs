//! Connection admission control: global and per-IP caps enforced at accept time.
//!
//! The connection layer accepts sockets on Tokio before any handshake or shard
//! work happens. Left unbounded, a single client — buggy or hostile — can open
//! connections faster than they close and exhaust file descriptors and memory.
//! The gate caps two dimensions: total live connections process-wide, and live
//! connections from any one peer IP.
//!
//! Admission is RAII. [`ConnectionGate::try_admit`] reserves a slot in both the
//! global counter and the per-IP table and returns an [`AdmissionGuard`];
//! dropping the guard when the connection task ends — for any reason — releases
//! both slots. Per-IP entries are pruned back to absent when they reach zero, so
//! a rotating set of source addresses cannot grow the table without bound.
//!
//! A cap of `0` means unlimited for that dimension; the gate still tracks the
//! live count so the value remains available for observability.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

/// Shared admission state. Cheap to clone — every clone shares one set of
/// counters, so the gate can be held by the accept loop and read elsewhere.
#[derive(Clone, Debug)]
pub struct ConnectionGate {
    inner: Arc<GateInner>,
}

#[derive(Debug)]
struct GateInner {
    max_total: usize,
    max_per_ip: usize,
    total: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    rejected: AtomicU64,
}

impl ConnectionGate {
    /// Builds a gate with the given caps. `0` disables enforcement of that cap
    /// (the live count is still tracked).
    pub fn new(max_total: usize, max_per_ip: usize) -> Self {
        Self {
            inner: Arc::new(GateInner {
                max_total,
                max_per_ip,
                total: AtomicUsize::new(0),
                per_ip: Mutex::new(HashMap::new()),
                rejected: AtomicU64::new(0),
            }),
        }
    }

    /// Attempts to admit a connection from `ip`. On success returns a guard that
    /// releases both the global and the per-IP slot when dropped. On rejection
    /// (a cap is full) returns `None` and bumps the rejection counter.
    pub fn try_admit(&self, ip: IpAddr) -> Option<AdmissionGuard> {
        if !self.reserve_global() {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        {
            let mut map = self.inner.per_ip.lock();
            let count = map.entry(ip).or_insert(0);
            if self.inner.max_per_ip != 0 && *count >= self.inner.max_per_ip {
                drop(map);
                self.release_global();
                self.inner.rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            *count += 1;
        }
        Some(AdmissionGuard {
            inner: Arc::clone(&self.inner),
            ip,
        })
    }

    /// Reserves a global slot, honoring the cap. Returns `false` if full.
    fn reserve_global(&self) -> bool {
        if self.inner.max_total == 0 {
            self.inner.total.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let mut cur = self.inner.total.load(Ordering::Relaxed);
        loop {
            if cur >= self.inner.max_total {
                return false;
            }
            match self.inner.total.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    fn release_global(&self) {
        self.inner.total.fetch_sub(1, Ordering::Relaxed);
    }

    // Observability surface for the admission gate. Exercised by the unit tests;
    // wiring these into the admin `/metrics` exposition (a
    // `brain_connections_rejected_total` counter + a live-connections gauge) is
    // the follow-up that consumes them from non-test code.
    /// Count of live admitted connections, process-wide.
    #[allow(dead_code)]
    pub fn live_total(&self) -> usize {
        self.inner.total.load(Ordering::Relaxed)
    }

    /// Count of live admitted connections from `ip`.
    #[allow(dead_code)]
    pub fn live_for_ip(&self, ip: IpAddr) -> usize {
        self.inner.per_ip.lock().get(&ip).copied().unwrap_or(0)
    }

    /// Connections rejected by the gate since start.
    #[allow(dead_code)]
    pub fn rejected(&self) -> u64 {
        self.inner.rejected.load(Ordering::Relaxed)
    }
}

/// RAII slot in a [`ConnectionGate`]. Dropping it frees one global and one
/// per-IP slot, pruning the per-IP entry when it reaches zero.
#[derive(Debug)]
pub struct AdmissionGuard {
    inner: Arc<GateInner>,
    ip: IpAddr,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.inner.total.fetch_sub(1, Ordering::Relaxed);
        let mut map = self.inner.per_ip.lock();
        if let Some(count) = map.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[test]
    fn admits_until_global_cap() {
        let gate = ConnectionGate::new(2, 0);
        let _g1 = gate.try_admit(ip(1)).expect("first under cap");
        let _g2 = gate.try_admit(ip(2)).expect("second under cap");
        assert_eq!(gate.live_total(), 2);
        // Third exceeds the global cap.
        assert!(gate.try_admit(ip(3)).is_none());
        assert_eq!(gate.rejected(), 1);
    }

    #[test]
    fn admits_until_per_ip_cap() {
        let gate = ConnectionGate::new(0, 2);
        let _a = gate.try_admit(ip(1)).expect("first from ip1");
        let _b = gate.try_admit(ip(1)).expect("second from ip1");
        assert_eq!(gate.live_for_ip(ip(1)), 2);
        // Third from the same IP is rejected...
        assert!(gate.try_admit(ip(1)).is_none());
        // ...while a different IP is still admitted.
        assert!(gate.try_admit(ip(2)).is_some());
        assert_eq!(gate.rejected(), 1);
    }

    #[test]
    fn guard_drop_decrements_both() {
        let gate = ConnectionGate::new(1, 1);
        {
            let _g = gate.try_admit(ip(1)).expect("admitted");
            assert_eq!(gate.live_total(), 1);
            // Cap is 1, so a second is rejected while the guard is live.
            assert!(gate.try_admit(ip(1)).is_none());
        }
        // Guard dropped: the slot is free again, globally and per-IP.
        assert_eq!(gate.live_total(), 0);
        assert!(gate.try_admit(ip(1)).is_some());
    }

    #[test]
    fn per_ip_entry_pruned_at_zero() {
        let gate = ConnectionGate::new(0, 4);
        {
            let _g = gate.try_admit(ip(7)).expect("admitted");
            assert_eq!(gate.inner.per_ip.lock().len(), 1);
        }
        // Dropping the last guard for an IP removes its table entry, so a
        // rotating set of source IPs cannot grow the map without bound.
        assert_eq!(gate.inner.per_ip.lock().len(), 0);
        assert_eq!(gate.live_for_ip(ip(7)), 0);
    }

    #[test]
    fn zero_caps_mean_unlimited() {
        let gate = ConnectionGate::new(0, 0);
        let mut guards = Vec::new();
        for _ in 0..1000 {
            guards.push(gate.try_admit(ip(1)).expect("unlimited admits"));
        }
        assert_eq!(gate.live_total(), 1000);
        assert_eq!(gate.live_for_ip(ip(1)), 1000);
        assert_eq!(gate.rejected(), 0);
    }
}
