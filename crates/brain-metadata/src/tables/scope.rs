//! `RowScope` — the `(namespace_id, space_id)` tenant boundary carried
//! onto every typed-graph row and every secondary-index key.
//!
//! Namespace is the outer wall (company-level), space the inner wall
//! (app-level). Together they form the scope key under which all
//! typed-graph data — entities, statements, relations — is isolated:
//! one `(namespace, space)` can physically never traverse another's
//! rows, because the scope is the leading prefix of every secondary
//! index key (mirroring the memory-layer recipe in
//! [`crate::tables::memory`]).
//!
//! The scope is REQUIRED — it has no `Default`, so a row or index key
//! can never be built without naming its owner (fail-closed by
//! construction). Ops thread it explicitly from the authenticated
//! caller's `(namespace, space)`.

use brain_core::{NamespaceId, SpaceId};

/// How widely a scope admits rows on the read path.
///
/// The namespace is always the tenant wall — no mode ever relaxes it.
/// The *space* check is what varies: a single-space read pins one space,
/// a namespace-wide read spans every space the caller owns within its
/// own namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScopeMode {
    /// A row is admitted only when BOTH its namespace and its space match
    /// the caller's scope. The default — the wall every write, forget,
    /// and single-space read enforces.
    #[default]
    Space,
    /// Read-only widening: a row is admitted when its namespace matches;
    /// the space check is dropped so the caller sees every space it owns
    /// within its own namespace. NEVER crosses namespaces.
    Namespace,
}

/// The `(namespace_id, space_id)` ownership key for a typed-graph row.
///
/// Stored as byte representations (`u32` + `[u8; 16]`) so it composes
/// directly into redb key tuples and rkyv-archived rows without
/// coupling to brain-core's typed ids; typed accessors convert at the
/// API boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowScope {
    /// Owning namespace (tenant) — the outer wall. `0` is the reserved
    /// `brain` system namespace ([`NamespaceId::SYSTEM`]).
    pub namespace_id: u32,
    /// Owning space (app) — the inner wall.
    pub space_id_bytes: [u8; 16],
}

impl RowScope {
    /// Build a scope from the typed brain-core ids.
    #[must_use]
    pub fn new(namespace: NamespaceId, space: SpaceId) -> Self {
        Self {
            namespace_id: namespace.raw(),
            space_id_bytes: space.into(),
        }
    }

    /// Build a scope directly from byte representations — used by ops
    /// that already hold the raw forms (apply path, recovery).
    #[must_use]
    pub fn from_bytes(namespace_id: u32, space_id_bytes: [u8; 16]) -> Self {
        Self {
            namespace_id,
            space_id_bytes,
        }
    }

    /// The owning namespace as a typed id.
    #[must_use]
    pub fn namespace(&self) -> NamespaceId {
        NamespaceId::from(self.namespace_id)
    }

    /// The owning space as a typed id.
    #[must_use]
    pub fn space(&self) -> SpaceId {
        SpaceId::from(self.space_id_bytes)
    }

    /// Does a row owned by `(row_namespace_id, row_space_id_bytes)` belong
    /// to this scope under `mode`?
    ///
    /// This is the single centralized scope predicate for the read path.
    /// The namespace check is **unconditional** — it is the tenant wall
    /// and holds in every mode, so a namespace-wide read can never return
    /// another tenant's rows. Only the space check is relaxed, and only
    /// under [`ScopeMode::Namespace`].
    ///
    /// Write, forget, and mutation paths do NOT use this — they call the
    /// strict `(namespace, space)` equality directly, because a widened
    /// scope must never affect where a row is written or removed.
    #[must_use]
    pub fn admits(
        &self,
        row_namespace_id: u32,
        row_space_id_bytes: &[u8; 16],
        mode: ScopeMode,
    ) -> bool {
        // Tenant wall — never relaxed.
        if row_namespace_id != self.namespace_id {
            return false;
        }
        match mode {
            ScopeMode::Space => row_space_id_bytes == &self.space_id_bytes,
            ScopeMode::Namespace => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_typed_ids() {
        let space = SpaceId::new();
        let ns = NamespaceId::from(7);
        let s = RowScope::new(ns, space);
        assert_eq!(s.namespace(), ns);
        assert_eq!(s.space(), space);
        assert_eq!(s.namespace_id, 7);
        assert_eq!(s.space_id_bytes, <[u8; 16]>::from(space));
    }

    #[test]
    fn system_namespace_is_zero() {
        let s = RowScope::new(NamespaceId::SYSTEM, SpaceId::NIL);
        assert_eq!(s.namespace_id, 0);
        assert!(s.namespace().is_system());
    }

    #[test]
    fn admits_space_mode_pins_both_walls() {
        let own_space = SpaceId::new();
        let other_space = SpaceId::new();
        let s = RowScope::new(NamespaceId::from(7), own_space);

        // same namespace + same space → admitted
        assert!(s.admits(7, &<[u8; 16]>::from(own_space), ScopeMode::Space));
        // same namespace, different space → rejected in Space mode
        assert!(!s.admits(7, &<[u8; 16]>::from(other_space), ScopeMode::Space));
        // different namespace → rejected regardless of space
        assert!(!s.admits(8, &<[u8; 16]>::from(own_space), ScopeMode::Space));
    }

    #[test]
    fn admits_namespace_mode_relaxes_space_but_not_namespace() {
        let own_space = SpaceId::new();
        let sibling_space = SpaceId::new();
        let s = RowScope::new(NamespaceId::from(7), own_space);

        // same namespace, own space → admitted
        assert!(s.admits(7, &<[u8; 16]>::from(own_space), ScopeMode::Namespace));
        // same namespace, a DIFFERENT space the caller owns → admitted
        assert!(s.admits(7, &<[u8; 16]>::from(sibling_space), ScopeMode::Namespace));
        // TENANT WALL: different namespace → rejected even namespace-wide
        assert!(!s.admits(8, &<[u8; 16]>::from(sibling_space), ScopeMode::Namespace));
        assert!(!s.admits(8, &<[u8; 16]>::from(own_space), ScopeMode::Namespace));
    }

    #[test]
    fn admits_never_crosses_namespaces_in_any_mode() {
        let space = SpaceId::new();
        let s = RowScope::new(NamespaceId::from(1), space);
        for mode in [ScopeMode::Space, ScopeMode::Namespace] {
            for foreign_ns in [0u32, 2, 3, u32::MAX] {
                assert!(
                    !s.admits(foreign_ns, &<[u8; 16]>::from(space), mode),
                    "namespace {foreign_ns} leaked in mode {mode:?}"
                );
            }
        }
    }
}
