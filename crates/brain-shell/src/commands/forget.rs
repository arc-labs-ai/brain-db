//! `forget` verb.

use brain_sdk_rust::{Client, ClientError};

use crate::parser::ForgetArgs;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    args: ForgetArgs,
) -> Result<Rendered, ClientError> {
    let txn = session.active_txn;
    let mut b = client.forget(args.id.0).mode(args.mode.into_wire());
    if let Some(t) = txn {
        b = b.txn(t);
    }
    let resp = b.send().await?;
    Ok(Box::new(resp))
}
