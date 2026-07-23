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

use brain_core::{SpaceId, NamespaceId};

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
}
