//! `link` verb.

use brain_sdk_rust::{Client, ClientError};

use crate::parser::{parse_txn_id, LinkArgs};
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    args: LinkArgs,
) -> Result<Rendered, ClientError> {
    let explicit_txn = match args.txn.as_deref() {
        Some(s) => Some(parse_txn_id(s).map_err(ClientError::Internal)?),
        None => None,
    };
    let txn = session.effective_txn(explicit_txn);
    let mut b = client
        .link(args.src.0, args.kind.into_wire(), args.tgt.0)
        .weight(args.weight);
    if let Some(t) = txn {
        b = b.txn(t);
    }
    let resp = b.send().await?;
    Ok(Box::new(resp))
}
