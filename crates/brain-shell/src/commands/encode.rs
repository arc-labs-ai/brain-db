//! `encode` verb.

use brain_core::MemoryId;
use brain_sdk_rust::{Client, ClientError};

use crate::output::table::EncodeRendered;
use crate::parser::{parse_txn_id, EncodeArgs};
use crate::session::Session;

use super::Rendered;

/// Send an `ENCODE`. Inherits the session's active txn + sticky
/// context when the caller didn't override them. Pushes the
/// resulting memory id onto the recent-id list.
pub async fn run(
    client: &Client,
    session: &mut Session,
    args: EncodeArgs,
) -> Result<Rendered, ClientError> {
    let explicit_txn = match args.txn.as_deref() {
        Some(s) => Some(parse_txn_id(s).map_err(ClientError::Internal)?),
        None => None,
    };
    let txn = session.effective_txn(explicit_txn);
    let context_id = session.effective_context(args.context);

    let mut b = client
        .encode(args.text)
        .context(context_id)
        .salience(args.salience.unwrap_or(0.5))
        .deduplicate(args.deduplicate);
    if let Some(k) = args.kind {
        b = b.kind(k.into_wire());
    }
    if let Some(t) = txn {
        b = b.txn(t);
    }
    let resp = b.send().await?;
    session.push_recent_id(MemoryId::from_raw(resp.memory_id));
    Ok(Box::new(EncodeRendered {
        response: resp,
        dedup_requested: args.deduplicate,
    }))
}
