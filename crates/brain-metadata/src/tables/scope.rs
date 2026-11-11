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

/// The all-zero space: "this declaration belongs to the whole namespace".
///
/// Not a real [`brain_core::SpaceId`] — `SpaceId::NIL` is the anonymous
/// caller, and a declaration is never owned by a caller. The two values
/// coincide numerically and must not be conflated in reasoning: this one
/// means *unscoped by space*, which is why it is named for what it is.
pub const SPACE_BASE: [u8; 16] = [0u8; 16];

/// The scope a DECLARATION is read or written in: a tenant namespace, and
/// either one space inside it or the namespace-wide base.
///
/// Sibling of [`RowScope`], and the distinction is the point. `RowScope`
/// scopes DATA — the entities, statements and relations a tenant owns, keyed
/// by interned ids. `DeclScope` scopes VOCABULARY — the entity types,
/// predicates, relation types, extractors and schema documents that say what
/// data may exist, keyed by the namespace NAME the qname carries.
///
/// The namespace and the space travel together because neither answers a
/// lookup alone, and passing them as two loose arguments made two mistakes
/// representable that this type makes unstateable: omitting the space (which
/// silently meant the tenant-wide base, so a space read another scope's
/// vocabulary) and transposing a namespace against the wrong space.
///
/// Construct it by naming which kind of scope it is — [`DeclScope::base`] or
/// [`DeclScope::space`] — so a call site that means the namespace base says
/// so instead of passing an all-zero array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeclScope<'a> {
    namespace: &'a str,
    space: [u8; 16],
}

impl<'a> DeclScope<'a> {
    /// The namespace-wide base: the declarations every space in the tenant
    /// resolves, and the only scope that existed before spaces could
    /// declare.
    ///
    /// Reserved for declarations that genuinely belong to the whole tenant
    /// (or, for the system namespace, the whole deployment). A read on
    /// behalf of a caller acting in a space uses [`DeclScope::space`], which
    /// falls back here on its own.
    #[must_use]
    pub const fn base(namespace: &'a str) -> Self {
        Self {
            namespace,
            space: SPACE_BASE,
        }
    }

    /// One space's scope. Reads fall back to the namespace base; writes land
    /// in this space alone.
    ///
    /// Passing [`SPACE_BASE`] is the same scope [`DeclScope::base`] names —
    /// this constructor takes a space id that came from a caller's identity,
    /// where the base is a legitimate value meaning "no effective space".
    #[must_use]
    pub const fn space(namespace: &'a str, space: [u8; 16]) -> Self {
        Self { namespace, space }
    }

    /// The tenant namespace name.
    #[must_use]
    pub const fn namespace(&self) -> &'a str {
        self.namespace
    }

    /// The space id, [`SPACE_BASE`] for the namespace-wide base.
    #[must_use]
    pub const fn space_id(&self) -> [u8; 16] {
        self.space
    }
}

/// The scope an ENTITY TYPE is read or written in: a tenant namespace by id,
/// and either one space inside it or the namespace-wide base.
///
/// Sibling of [`DeclScope`], and the difference is what the key is made of.
/// A predicate, relation type or extractor is addressed by a QNAME, which
/// carries its namespace as text. An entity type is addressed by a BARE name
/// against an interned [`NamespaceId`], because the label surface offers the
/// classifier `brain:<Name>` for every type whatever its owner — scoping is
/// a registry concern, not a prompt one.
///
/// So this scope names the namespace by id, and its ladder carries a third
/// rung the qname registries have no use for: the shared system namespace,
/// whose built-ins every tenant resolves after its own declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeScope {
    namespace_id: u32,
    space: [u8; 16],
}

impl TypeScope {
    /// A tenant's namespace-wide base: the declarations every space in the
    /// namespace resolves.
    #[must_use]
    pub const fn base(namespace_id: u32) -> Self {
        Self {
            namespace_id,
            space: SPACE_BASE,
        }
    }

    /// One space's scope. Reads fall back to the namespace base and then to
    /// the built-ins; writes land in this space alone.
    #[must_use]
    pub const fn space(namespace_id: u32, space: [u8; 16]) -> Self {
        Self {
            namespace_id,
            space,
        }
    }

    /// The owning tenant's interned namespace id.
    #[must_use]
    pub const fn namespace_id(&self) -> u32 {
        self.namespace_id
    }

    /// The space id, [`SPACE_BASE`] for the namespace-wide base.
    #[must_use]
    pub const fn space_id(&self) -> [u8; 16] {
        self.space
    }

    /// The scopes a lookup consults, narrowest first:
    ///
    /// 1. `(namespace, space)` — the space's own overlay. Absent when the
    ///    caller asked the namespace-wide question, so the common path is
    ///    exactly the two lookups it always was.
    /// 2. `(namespace, base)` — the tenant's own declarations.
    /// 3. `(system, base)` — the shared built-ins. A system-namespace caller
    ///    consults them once, not twice.
    ///
    /// THE ORDER IS THE WHOLE POINT. Each rung may shadow the one after it:
    /// a tenant that declares `Person` gets THEIR `Person` rather than the
    /// built-in; a space that declares `Person` gets ITS `Person` rather
    /// than the tenant's. Consulting a broader rung earlier would let a more
    /// general declaration shadow a more specific one, which is backwards —
    /// the narrower scope is the one that was chosen deliberately.
    ///
    /// READS CHAIN, WRITES PROBE EXACTLY, as for [`DeclScope`]: a write that
    /// chained would find the tenant's or the built-in row and hand it back,
    /// so declaring an override would silently return the shadowed row.
    pub fn chain(&self) -> impl Iterator<Item = (u32, [u8; 16])> {
        let system = NamespaceId::SYSTEM.raw();
        let overlay = (self.space != SPACE_BASE).then_some((self.namespace_id, self.space));
        let base = std::iter::once((self.namespace_id, SPACE_BASE));
        let builtins = (self.namespace_id != system).then_some((system, SPACE_BASE));
        overlay.into_iter().chain(base).chain(builtins)
    }
}

/// The space ladder a scoped lookup walks, narrowest first.
///
/// A registry's index helpers take the ladder directly rather than a
/// [`DeclScope`], because their key is `(qname, space)` and the qname already
/// carries the namespace — a scope there would name it twice and let the two
/// disagree.
pub fn space_chain(space: [u8; 16]) -> impl Iterator<Item = [u8; 16]> {
    let overlay = (space != SPACE_BASE).then_some(space);
    overlay.into_iter().chain(std::iter::once(SPACE_BASE))
}

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
