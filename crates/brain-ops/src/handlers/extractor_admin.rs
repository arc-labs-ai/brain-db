//! Extractor introspection wire-op handler — `EXTRACTOR_LIST`.
//!
//! Extraction is always-on; there is no runtime enable/disable path.
//! `LIST` is read-only and serves off the direct-rtxn path.

use brain_metadata::extractor::ops::{extractor_list, ExtractorOpError};
use brain_protocol::{ExtractorListItem, ExtractorListRequest, ExtractorListResponseFrame};

use crate::context::OpsContext;
use crate::error::OpError;

// ---------------------------------------------------------------------------
// EXTRACTOR_LIST
// ---------------------------------------------------------------------------

pub async fn handle_extractor_list(
    _req: ExtractorListRequest,
    ctx: &OpsContext,
) -> Result<ExtractorListResponseFrame, OpError> {
    let rows = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        extractor_list(&rtxn).map_err(map_extractor_op_error)?
    };
    let items: Vec<ExtractorListItem> = rows
        .into_iter()
        .map(|r| ExtractorListItem {
            extractor_id: r.extractor_id,
            namespace: r.namespace,
            name: r.name,
            kind: r.kind,
            schema_version: r.schema_version,
            created_at_unix_nanos: r.created_at_unix_nanos,
        })
        .collect();
    let total = items.len() as u32;
    Ok(ExtractorListResponseFrame {
        items,
        total,
        is_final: true,
    })
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
