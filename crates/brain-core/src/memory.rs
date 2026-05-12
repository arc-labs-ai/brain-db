//! Memory and salience types.
//!
//! See `spec/02_data_model/02_memory_entity.md`.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ContextId, MemoryId};

/// Three durable kinds, per `spec/02_data_model/02_memory_entity.md`.
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
/// The decay worker reduces salience over time per `spec/11_background_workers/`.
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
/// This is the "fully hydrated" view. The on-disk slot layout is defined in
/// `spec/05_storage_arena_wal/02_arena_layout.md`.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salience_clamps() {
        assert_eq!(Salience::new(2.0).raw(), 1.0);
        assert_eq!(Salience::new(-1.0).raw(), 0.0);
        assert_eq!(Salience::new(0.5).raw(), 0.5);
    }

    #[test]
    fn salience_default_is_neutral() {
        assert_eq!(Salience::default().raw(), 0.5);
    }
}
