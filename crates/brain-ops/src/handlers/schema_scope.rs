//! Shared ground for the three schema-mutating handlers.
//!
//! `SCHEMA_UPLOAD`, `SCHEMA_REPLACE` and `SCHEMA_DROP` each need the same four
//! things before they touch anything: the caller's namespace name, the
//! namespace's active version, the set of type names a document may reference
//! without re-declaring, and a mapping from the stores' error types to
//! [`OpError`]. They lived in `schema.rs` as `pub(crate)` items that the other
//! two reached across for, which both hid the sharing and made `schema.rs`
//! look like the owner of behaviour all three define.
//!
//! The module boundary is "what a schema mutation needs to know about its
//! caller and the current state", not "whatever `schema.rs` happened to have
//! first".

use brain_metadata::schema::store::{declared_context, schema_active, SchemaStoreError};
use brain_metadata::system_schema::SYSTEM_SCHEMA_NAMESPACE;
use brain_planner::WriterError;
use brain_protocol::schema::DeclaredContext;

use crate::context::OpsContext;
use crate::error::OpError;

/// 1 MiB cap on an uploaded schema document.
pub const MAX_SCHEMA_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Resolve the authenticated caller's namespace id to its registered name,
/// for comparison against a client-supplied namespace string.
///
/// Every schema mutation binds to this before writing: a caller may only
/// declare, replace or narrow their own namespace's vocabulary. The caller
/// namespace is interned at dispatch time, so a live request always has a
/// registry row; a miss is an internal invariant break (surfaced as
/// `Internal`, never as a client-visible authz bypass).
pub fn caller_namespace_name(ctx: &OpsContext) -> Result<String, OpError> {
    let caller = ctx.executor.caller_namespace;
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    brain_metadata::namespace::namespace_name(&rtxn, caller)
        .map_err(|e| OpError::Internal(format!("namespace_name lookup: {e}")))?
        .ok_or_else(|| {
            OpError::Internal(format!(
                "caller namespace id {} has no registry row",
                caller.raw()
            ))
        })
}

/// The namespace's active schema version, or `None` when it has never
/// declared one.
pub fn current_active(ctx: &OpsContext, namespace: &str) -> Result<Option<u32>, OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    schema_active(&rtxn, namespace).map_err(map_schema_store_error)
}

/// Type names an **additive** upload to `namespace` may reference without
/// re-declaring: the namespace's own active schema plus the system schema.
///
/// This is the context for a document that merges into what is already there.
/// A narrowing operation must not use it — see [`system_declared_context`].
pub fn merge_declared_context(
    ctx: &OpsContext,
    namespace: &str,
) -> Result<DeclaredContext, OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    declared_context(&rtxn, namespace).map_err(map_schema_store_error)
}

/// Type names the **system** schema declares, and nothing else.
///
/// This is the context for re-validating a document after something has been
/// taken out of it, and the distinction from [`merge_declared_context`] is the
/// whole point of having two.
///
/// A drop re-validates to answer one question: does removing this type leave a
/// reference dangling? Resolving against the namespace's active schema would
/// answer "no" every time, because the active schema is the pre-drop document
/// and still declares the type being removed. Resolving against nothing at all
/// is the opposite mistake, and was the live bug: the system schema declares
/// the shared entity types (`Person`, `Organization`, `Place`, …), a tenant
/// document references them without re-declaring them, and validating with an
/// empty context therefore reported every one of them as undeclared. Every
/// `SCHEMA_DROP` against a namespace with a single relation_type failed that
/// way, whatever the caller was trying to drop.
///
/// The system schema plus the narrowed document is the only pairing that
/// answers the question asked.
pub fn system_declared_context(ctx: &OpsContext) -> Result<DeclaredContext, OpError> {
    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    declared_context(&rtxn, SYSTEM_SCHEMA_NAMESPACE).map_err(map_schema_store_error)
}

/// Map a schema-store fault to the status the caller should see.
pub fn map_schema_store_error(e: SchemaStoreError) -> OpError {
    match e {
        SchemaStoreError::VersionOverflow { namespace } => OpError::Conflict(format!(
            "schema_version overflow for namespace {namespace:?}"
        )),
        other => OpError::Internal(other.to_string()),
    }
}

/// Map a writer fault to the status the caller should see.
pub fn map_writer_err(err: WriterError) -> OpError {
    match err {
        WriterError::Conflict(msg) => OpError::Conflict(msg),
        WriterError::Overloaded => OpError::Overloaded("writer overloaded".into()),
        WriterError::Internal(msg) => OpError::Internal(msg),
    }
}
