//! Extractor governance wire-op handlers — `EXTRACTOR_LIST /
//! _DISABLE / _ENABLE` (spec §28/05 §6-§7, phase 20.8).
//!
//! Each handler:
//!
//! 1. Validates wire-layer input.
//! 2. For LIST: opens an rtxn → `extractor_list(rtxn)` → filter
//!    + project to `ExtractorListItem`. The persisted state is
//!    authoritative; the in-memory registry tracks it.
//! 3. For DISABLE / ENABLE: opens a wtxn → `extractor_set_enabled`
//!    → captures previous state → updates the in-memory registry
//!    → commits. No event emission per §28/05 §7.2.

use brain_core::ExtractorId;
use brain_metadata::extractor_ops::{
    extractor_get, extractor_list, extractor_set_enabled, ExtractorOpError,
};
use brain_protocol::knowledge::{
    ExtractorDisableRequest, ExtractorDisableResponse, ExtractorEnableRequest,
    ExtractorEnableResponse, ExtractorListItem, ExtractorListRequest, ExtractorListResponseFrame,
};

use crate::context::OpsContext;
use crate::error::OpError;

const REASON_MAX_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// EXTRACTOR_LIST
// ---------------------------------------------------------------------------

pub async fn handle_extractor_list(
    req: ExtractorListRequest,
    ctx: &OpsContext,
) -> Result<ExtractorListResponseFrame, OpError> {
    let rows = {
        let db_guard = ctx.executor.metadata.lock();
        let rtxn = db_guard
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        extractor_list(&rtxn).map_err(map_extractor_op_error)?
    };
    let items: Vec<ExtractorListItem> = rows
        .into_iter()
        .filter(|r| req.include_disabled || r.is_enabled())
        .map(|r| {
            let enabled = r.is_enabled();
            ExtractorListItem {
                extractor_id: r.extractor_id,
                namespace: r.namespace,
                name: r.name,
                kind: r.kind,
                enabled,
                schema_version: r.schema_version,
                created_at_unix_nanos: r.created_at_unix_nanos,
            }
        })
        .collect();
    let total = items.len() as u32;
    Ok(ExtractorListResponseFrame {
        items,
        total,
        is_final: true,
    })
}

// ---------------------------------------------------------------------------
// EXTRACTOR_DISABLE
// ---------------------------------------------------------------------------

pub async fn handle_extractor_disable(
    req: ExtractorDisableRequest,
    ctx: &OpsContext,
) -> Result<ExtractorDisableResponse, OpError> {
    if req.extractor_id == 0 {
        return Err(OpError::InvalidRequest(
            "extractor_id must be non-zero".into(),
        ));
    }
    if req.reason.len() > REASON_MAX_BYTES {
        return Err(OpError::InvalidRequest(format!(
            "reason exceeds {REASON_MAX_BYTES}-byte cap (got {})",
            req.reason.len()
        )));
    }
    set_enabled_inner(ctx, req.extractor_id, false).map(|previous| {
        ExtractorDisableResponse {
            previously_enabled: previous,
            disabled_at_unix_nanos: crate::txn::now_unix_nanos_pub(),
        }
    })
}

// ---------------------------------------------------------------------------
// EXTRACTOR_ENABLE
// ---------------------------------------------------------------------------

pub async fn handle_extractor_enable(
    req: ExtractorEnableRequest,
    ctx: &OpsContext,
) -> Result<ExtractorEnableResponse, OpError> {
    if req.extractor_id == 0 {
        return Err(OpError::InvalidRequest(
            "extractor_id must be non-zero".into(),
        ));
    }
    set_enabled_inner(ctx, req.extractor_id, true).map(|previous| ExtractorEnableResponse {
        previously_disabled: !previous,
        enabled_at_unix_nanos: crate::txn::now_unix_nanos_pub(),
    })
}

// ---------------------------------------------------------------------------
// Shared core.
// ---------------------------------------------------------------------------

fn set_enabled_inner(
    ctx: &OpsContext,
    extractor_id_raw: u32,
    enabled: bool,
) -> Result<bool, OpError> {
    let id = ExtractorId::from(extractor_id_raw);

    let previous = {
        let mut db_guard = ctx.executor.metadata.lock();

        // Existence probe via read txn (gives a precise NotFound
        // before opening the wtxn).
        {
            let rtxn = db_guard
                .read_txn()
                .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
            if extractor_get(&rtxn, id)
                .map_err(map_extractor_op_error)?
                .is_none()
            {
                return Err(OpError::NotFound {
                    what: "extractor",
                    detail: format!("id {extractor_id_raw}"),
                });
            }
        }

        let wtxn = db_guard
            .write_txn()
            .map_err(|e| OpError::Internal(format!("write_txn: {e}")))?;
        let previous =
            extractor_set_enabled(&wtxn, id, enabled).map_err(map_extractor_op_error)?;
        wtxn.commit()
            .map_err(|e| OpError::Internal(format!("commit: {e}")))?;
        previous
    };

    // Sync the in-memory registry. The persisted state is the
    // source of truth; this keeps the dispatch path's
    // `iter_enabled` view consistent without re-querying.
    ctx.extractor_registry.write().set_enabled(id, enabled);

    Ok(previous)
}

fn map_extractor_op_error(e: ExtractorOpError) -> OpError {
    match e {
        ExtractorOpError::NotFound { id } => OpError::NotFound {
            what: "extractor",
            detail: format!("id {}", id.raw()),
        },
        ExtractorOpError::InvalidIdentifier { reason } => {
            OpError::InvalidRequest(reason.to_string())
        }
        ExtractorOpError::AlreadyExists { qname, existing_id } => OpError::Conflict(format!(
            "extractor {qname:?} already exists with id {}",
            existing_id.raw()
        )),
        other => OpError::Internal(other.to_string()),
    }
}
