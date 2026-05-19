//! `extract status <memory_id>` — pulls the extraction audit row.
//!
//! Requires an ExtractionAuditGet wire op the SDK doesn't currently
//! expose. Scaffolded with `todo!` so a follow-up adds it.

use brain_sdk_rust::{Client, ClientError};

use crate::commands::Rendered;
use crate::parser::ExtractStatusArgs;
use crate::session::Session;

pub async fn run(
    _client: &Client,
    _session: &mut Session,
    args: ExtractStatusArgs,
) -> Result<Rendered, ClientError> {
    tracing::warn!(
        target: "brain_shell",
        "extract status {}: requires ExtractionAuditGetReq wire op + SDK builder; \
         not wired yet.",
        args.memory_id.0.raw(),
    );
    todo!(
        "wire op required: ExtractionAuditGet / ExtractionAuditListByMemory \
         for `extract status` (memory id 0x{:x}).",
        args.memory_id.0.raw()
    )
}
