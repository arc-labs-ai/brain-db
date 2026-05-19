//! `mention list --memory M | --entity E` — Mentions edges between
//! memories and entities.
//!
//! No dedicated SDK builder for this today — mentions live on the
//! unified edge index (`EdgeKindRef::Mentions`). The shell scaffolds
//! the command surface; backend work is needed before it returns rows.

use brain_sdk_rust::{Client, ClientError};

use brain_explore::AdHocTable;

use crate::commands::Rendered;
use crate::parser::MentionListArgs;
use crate::session::Session;

pub async fn run(
    _client: &Client,
    _session: &mut Session,
    args: MentionListArgs,
) -> Result<Rendered, ClientError> {
    if args.memory.is_none() && args.entity.is_none() {
        return Err(ClientError::Internal(
            "mention list requires --memory or --entity".into(),
        ));
    }
    tracing::warn!(
        target: "brain_shell",
        "mention list: wire op MentionListReq / EdgeList(Mentions) not yet \
         exposed via the SDK. Returning an empty table; a follow-up needs \
         to add the wire op + SDK builder."
    );
    Ok(Box::new(AdHocTable {
        headers: vec![
            "memory".into(),
            "entity".into(),
            "kind".into(),
            "created_at".into(),
        ],
        rows: Vec::new(),
    }))
}
