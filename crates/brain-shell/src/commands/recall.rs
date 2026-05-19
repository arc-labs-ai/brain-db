//! `recall` verb.

use brain_core::MemoryId;
use brain_sdk_rust::{Client, ClientError};

use crate::output::table::RecallResults;
use crate::parser::{parse_txn_id, RecallArgs};
use crate::session::Session;

use super::Rendered;

/// Send a `RECALL`, collecting all frames into a `Vec<MemoryResult>`.
/// Pushes every returned id onto the session's recent-id list.
pub async fn run(
    client: &Client,
    session: &mut Session,
    args: RecallArgs,
) -> Result<Rendered, ClientError> {
    let explicit_txn = match args.txn.as_deref() {
        Some(s) => Some(parse_txn_id(s).map_err(ClientError::Internal)?),
        None => None,
    };
    let txn = session.effective_txn(explicit_txn);

    let mut b = client
        .recall(args.query)
        .top_k(args.top_k)
        .confidence_threshold(args.confidence)
        .include_text(args.include_text);
    if !args.filter_context.is_empty() {
        b = b.context_filter(args.filter_context);
    }
    if !args.filter_kind.is_empty() {
        let kinds = args
            .filter_kind
            .into_iter()
            .map(|k| k.into_wire())
            .collect();
        b = b.kind_filter(kinds);
    }
    if let Some(s) = args.strategy {
        b = b.strategy(s.into_wire());
    }
    if let Some(t) = txn {
        b = b.txn(t);
    }
    let results = b.send().await?;
    for r in &results {
        session.push_recent_id(MemoryId::from_raw(r.memory_id));
    }
    Ok(Box::new(RecallResults(results)))
}
