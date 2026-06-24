//! Memory and salience types.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ContextId, MemoryId};

/// Three durable kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MemoryKind {
    /// Default. 30-day half-life. Created by clients via `ENCODE`.
    Episodic,
    /// 365-day half-life. Created by clients via `ENCODE` with `kind=semantic`.
    Semantic,
    /// 90-day half-life. Created only by the consolidation worker.
    Consolidated,
}

/// A salience score in `[0.0, 1.0]`. Higher = more important.
///
/// The decay worker reduces salience over time.
/// Recall ranking blends salience with similarity, recency, and graph proximity.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Salience(f32);

impl Salience {
    /// Construct a salience score, clamping to `[0.0, 1.0]`.
    #[must_use]
    pub fn new(value: f32) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    #[must_use]
    pub const fn raw(self) -> f32 {
        self.0
    }
}

impl Default for Salience {
    fn default() -> Self {
        Self(0.5)
    }
}

/// A stored memory, as returned by recall and other read operations.
///
/// This is the "fully hydrated" view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    pub id: MemoryId,
    pub agent: AgentId,
    pub context: ContextId,
    pub kind: MemoryKind,
    pub salience: Salience,
    pub text: Option<String>,
    pub created_at_unix_ms: u64,
    pub last_accessed_at_unix_ms: u64,
    /// Client-supplied event time (when the content happened), in unix
    /// nanoseconds. `None` when unsupplied. The temporal-expressions
    /// extractor anchors relative dates ("last week") to this when
    /// present, falling back to `created_at` — so a memory ingested today
    /// about a 2020 event resolves its relative dates against 2020.
    pub occurred_at_unix_nanos: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salience_clamps_out_of_range_values_to_unit_interval() {
        assert_eq!(Salience::new(2.0).raw(), 1.0);
        assert_eq!(Salience::new(-1.0).raw(), 0.0);
        assert_eq!(Salience::new(0.5).raw(), 0.5);
    }
}
