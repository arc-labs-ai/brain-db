//! Knowledge-specific error inspection helpers. Phase 16.8.4.
//!
//! Until spec §28/03 Strategy A lands (knowledge error codes promoted
//! to first-class `ErrorCodeWire` variants in the substrate's `ERROR`
//! frame — tracked as §28/09 Q1), the server returns knowledge errors
//! through the Strategy B fallback: substrate codes + message text.
//!
//! This module provides typed inspection over the resulting
//! [`ClientError::Server`] frames so callers can write:
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, ClientError, Person};
//! # use brain_sdk_rust::knowledge::errors::{ClientErrorEntityExt, EntityErrorKind};
//! # async fn ex(client: Client, id: brain_sdk_rust::EntityId) -> Result<(), ClientError> {
//! match client.entity::<Person>().rename(id, "Alice Cooper").await {
//!     Ok(_) => {},
//!     Err(e) if e.entity_error() == Some(EntityErrorKind::NotFound) => {
//!         // surface to user
//!     }
//!     Err(e) => return Err(e),
//! }
//! # Ok(()) }
//! ```
//!
//! Strategy A will replace string-matching with code-byte matching;
//! the public API of this module is forward-stable.

use crate::error::ClientError;

/// Coarse-grained knowledge error category, derived from substrate
/// `ErrorCode` + message inspection. See spec §28/03 §2 for the
/// mapping table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityErrorKind {
    /// `ENTITY_NOT_FOUND` (§28 0x30). Currently surfaced as substrate
    /// `MemoryNotFound` (Strategy B) with "entity not found" in the
    /// message.
    NotFound,

    /// `ENTITY_TYPE_MISMATCH` (§28 0x31). Surfaced as substrate
    /// `InvalidArgument` with "entity_type" / "type mismatch" in the
    /// message.
    TypeMismatch,

    /// `ENTITY_AMBIGUOUS` (§28 0x32). Surfaced as substrate
    /// `IdempotencyConflict` with "canonical_name … already exists"
    /// in the message, OR as the resolver's `Ambiguous` outcome
    /// (which is NOT an error — clients see `ResolutionOutcome::Ambiguous`
    /// from `resolve()`).
    AlreadyExists,

    /// `ENTITY_MERGE_CONFLICT` (§28 0x33). Substrate `Conflict` with
    /// merge-specific text ("already merged", "grace period",
    /// "self-merge", "tombstoned").
    MergeConflict,
}

/// Extension trait letting callers inspect a [`ClientError`] for
/// knowledge-error context without pattern-matching on the inner
/// `Server { code, message }` shape.
pub trait ClientErrorEntityExt {
    /// Returns the entity error category, if `self` is a server-side
    /// error matching one of the knowledge patterns. Returns `None`
    /// for transport / protocol / other server errors.
    fn entity_error(&self) -> Option<EntityErrorKind>;

    /// `true` iff this error indicates the entity (or the referenced
    /// row) doesn't exist on the server.
    fn is_entity_not_found(&self) -> bool {
        self.entity_error() == Some(EntityErrorKind::NotFound)
    }

    /// `true` iff this error indicates a type-id mismatch between the
    /// caller's `<T>` and the server's stored entity_type_id.
    fn is_entity_type_mismatch(&self) -> bool {
        self.entity_error() == Some(EntityErrorKind::TypeMismatch)
    }

    /// `true` iff this error indicates a duplicate `canonical_name`
    /// for the entity's type.
    fn is_entity_already_exists(&self) -> bool {
        self.entity_error() == Some(EntityErrorKind::AlreadyExists)
    }

    /// `true` iff this error indicates a merge pre-condition failure
    /// (self-merge, already-merged, type mismatch, tombstoned, out of
    /// grace, etc.).
    fn is_entity_merge_conflict(&self) -> bool {
        self.entity_error() == Some(EntityErrorKind::MergeConflict)
    }
}

impl ClientErrorEntityExt for ClientError {
    fn entity_error(&self) -> Option<EntityErrorKind> {
        let message = match self {
            ClientError::Server { message, .. } => message,
            _ => return None,
        };
        let lower = message.to_lowercase();

        // Merge conflicts — these come back as substrate `Conflict`
        // ([§28/03 Strategy B mapping](../spec/28_knowledge_wire_protocol/03_errors.md)).
        // Match on the unambiguous keywords first.
        if lower.contains("merge") {
            return Some(EntityErrorKind::MergeConflict);
        }
        if lower.contains("survivor") && lower.contains("same entity") {
            return Some(EntityErrorKind::MergeConflict);
        }
        if lower.contains("grace period") {
            return Some(EntityErrorKind::MergeConflict);
        }
        if lower.contains("not currently merged") {
            return Some(EntityErrorKind::MergeConflict);
        }

        // Type mismatch — substrate `InvalidArgument`.
        if lower.contains("entity_type") || lower.contains("type mismatch") {
            return Some(EntityErrorKind::TypeMismatch);
        }

        // Duplicate canonical_name — Strategy B routes through
        // `IdempotencyConflict`.
        if lower.contains("canonical_name") && lower.contains("already exists") {
            return Some(EntityErrorKind::AlreadyExists);
        }

        // Entity not found.
        if lower.contains("entity") && lower.contains("not found") {
            return Some(EntityErrorKind::NotFound);
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Statement error inspection (17.8).
// ---------------------------------------------------------------------------

/// Statement error category, derived from substrate `ErrorCode` +
/// message inspection. Mirrors [`EntityErrorKind`] but for the
/// statement-layer opcodes (spec §28/06).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatementErrorKind {
    /// `STATEMENT_NOT_FOUND` (§28 0x40).
    NotFound,
    /// `STATEMENT_OBJECT_TYPE_MISMATCH` (§28 0x41) — the statement's
    /// object variant violates the predicate's `object_type_constraint`.
    ObjectTypeMismatch,
    /// `STATEMENT_CONTRADICTS_EXISTING` (§28 0x42). In v1 this is
    /// purely an audit signal: Fact create still succeeds even when
    /// it contradicts, so this is reserved for future explicit-reject
    /// modes.
    ContradictsExisting,
    /// `predicate {qname} not registered`. The statement references a
    /// predicate that hasn't been registered via `SCHEMA_UPLOAD` (or
    /// is built-in but referenced under the wrong qname).
    PredicateUnknown,
    /// The statement references a subject that doesn't exist on the
    /// server.
    SubjectUnknown,
    /// Chain-state pre-condition failure (already superseded, already
    /// tombstoned, kind/subject/predicate mismatch on supersede,
    /// Event supersession).
    ChainConflict,
}

/// Extension trait for inspecting statement-layer errors on a
/// [`ClientError`]. Mirrors [`ClientErrorEntityExt`].
pub trait ClientErrorStatementExt {
    /// Returns the statement error category if the inner error matches
    /// one of the spec §28/06 patterns; `None` otherwise.
    fn statement_error(&self) -> Option<StatementErrorKind>;

    fn is_statement_not_found(&self) -> bool {
        self.statement_error() == Some(StatementErrorKind::NotFound)
    }

    fn is_statement_object_type_mismatch(&self) -> bool {
        self.statement_error() == Some(StatementErrorKind::ObjectTypeMismatch)
    }

    fn is_statement_predicate_unknown(&self) -> bool {
        self.statement_error() == Some(StatementErrorKind::PredicateUnknown)
    }

    fn is_statement_subject_unknown(&self) -> bool {
        self.statement_error() == Some(StatementErrorKind::SubjectUnknown)
    }

    fn is_statement_chain_conflict(&self) -> bool {
        self.statement_error() == Some(StatementErrorKind::ChainConflict)
    }
}

impl ClientErrorStatementExt for ClientError {
    fn statement_error(&self) -> Option<StatementErrorKind> {
        let message = match self {
            ClientError::Server { message, .. } => message,
            _ => return None,
        };
        let lower = message.to_lowercase();

        // Chain / state conflicts.
        if lower.contains("already superseded")
            || lower.contains("already tombstoned")
            || lower.contains("events cannot be superseded")
            || lower.contains("kind mismatch on supersede")
            || lower.contains("subject must match on supersede")
            || lower.contains("predicate must match on supersede")
        {
            return Some(StatementErrorKind::ChainConflict);
        }

        if lower.contains("contradict") {
            return Some(StatementErrorKind::ContradictsExisting);
        }

        if lower.contains("object variant") || lower.contains("object_type_constraint") {
            return Some(StatementErrorKind::ObjectTypeMismatch);
        }

        if lower.contains("unknown predicate") || lower.contains("predicate") && lower.contains("not registered") {
            return Some(StatementErrorKind::PredicateUnknown);
        }

        if lower.contains("subject entity") && lower.contains("not found") {
            return Some(StatementErrorKind::SubjectUnknown);
        }

        if lower.contains("statement") && lower.contains("not found") {
            return Some(StatementErrorKind::NotFound);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_err(message: &str) -> ClientError {
        ClientError::Server {
            code: 0x05, // substrate MemoryNotFound (placeholder)
            message: message.to_string(),
        }
    }

    #[test]
    fn detects_not_found() {
        let e = server_err("entity EntityId(...) not found");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::NotFound));
        assert!(e.is_entity_not_found());
    }

    #[test]
    fn detects_type_mismatch() {
        let e = server_err("unknown entity_type EntityTypeId(99)");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::TypeMismatch));
        assert!(e.is_entity_type_mismatch());
    }

    #[test]
    fn detects_already_exists() {
        let e = server_err("canonical_name \"Alice\" already exists for type EntityTypeId(1): EntityId(...)");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::AlreadyExists));
        assert!(e.is_entity_already_exists());
    }

    #[test]
    fn detects_merge_conflict_self() {
        let e = server_err("survivor and merged are the same entity");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::MergeConflict));
        assert!(e.is_entity_merge_conflict());
    }

    #[test]
    fn detects_merge_conflict_already_merged() {
        let e = server_err("entity EntityId(...) already merged into EntityId(...)");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::MergeConflict));
    }

    #[test]
    fn detects_merge_conflict_grace() {
        let e = server_err("merge grace period expired");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::MergeConflict));
    }

    #[test]
    fn detects_merge_conflict_not_merged() {
        let e = server_err("entity EntityId(...) is not currently merged");
        assert_eq!(e.entity_error(), Some(EntityErrorKind::MergeConflict));
    }

    #[test]
    fn unrelated_errors_return_none() {
        assert_eq!(server_err("write_txn: io error").entity_error(), None);
        assert_eq!(
            ClientError::Internal("something else".into()).entity_error(),
            None
        );
        assert_eq!(ClientError::Closed.entity_error(), None);
    }
}
