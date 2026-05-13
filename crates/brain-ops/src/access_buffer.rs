//! Access-tracking buffer for the access-boost worker (sub-task 8.3).
//!
//! Spec §11/02 §7: RECALL appends every returned memory's id; the
//! boost worker drains the buffer on its 10 s cycle and applies a
//! ×1.10 salience bump per id (deduped).
//!
//! Dedup-on-record is the chosen semantic: a memory accessed N times
//! inside the same drain window receives one boost. Spec §7 phrases
//! the work unit as "MemoryIds in the buffer" rather than "accesses";
//! deduping bounds write amplification and the salience cap at 1.0
//! makes the difference between 1 and N boosts numerically small.

use std::collections::HashSet;

use brain_core::MemoryId;
use parking_lot::Mutex;

/// Default capacity. ~10 K headroom over the boost worker's default
/// batch_size of 1 000 (spec §11/01 §11) covers a recall storm.
pub const DEFAULT_ACCESS_BUFFER_CAPACITY: usize = 10_000;

pub struct AccessBuffer {
    inner: Mutex<AccessBufferInner>,
    capacity: usize,
}

struct AccessBufferInner {
    ids: HashSet<MemoryId>,
    overflowed: u64,
}

impl AccessBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(AccessBufferInner {
                ids: HashSet::with_capacity(capacity.min(4096)),
                overflowed: 0,
            }),
            capacity,
        }
    }

    /// Record a memory access. No-op if the buffer is at capacity
    /// (the boost will be picked up on a future access). Overflow
    /// drops are counted via `overflowed_count()` for observability.
    pub fn record(&self, id: MemoryId) {
        let mut inner = self.inner.lock();
        if inner.ids.contains(&id) {
            return; // dedup
        }
        if inner.ids.len() >= self.capacity {
            inner.overflowed = inner.overflowed.saturating_add(1);
            return;
        }
        inner.ids.insert(id);
    }

    /// Atomically swap out the current set. Returns the deduped ids
    /// in arbitrary order.
    #[must_use]
    pub fn drain(&self) -> Vec<MemoryId> {
        let mut inner = self.inner.lock();
        let drained = std::mem::take(&mut inner.ids);
        drained.into_iter().collect()
    }

    /// Number of ids currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().ids.len()
    }

    /// `true` if no ids are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().ids.is_empty()
    }

    /// Configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total number of records dropped due to overflow since the
    /// buffer was created.
    #[must_use]
    pub fn overflowed_count(&self) -> u64 {
        self.inner.lock().overflowed
    }
}

impl Default for AccessBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_ACCESS_BUFFER_CAPACITY)
    }
}

// Send + Sync via the mutex; explicit guard for the public surface.
