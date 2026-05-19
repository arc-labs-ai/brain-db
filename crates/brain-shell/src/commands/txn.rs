//! `txn begin` / `txn commit` / `txn abort`.

use brain_sdk_rust::{Client, ClientError};

use crate::parser::{parse_txn_id, TxnCommand};
use crate::session::Session;

use super::{is_txn_terminal, Rendered};

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: TxnCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        TxnCommand::Begin => {
            let resp = client.txn_begin().await?;
            session.active_txn = Some(resp.txn_id);
            Ok(Box::new(resp))
        }
        TxnCommand::Commit { id } => {
            let bytes = parse_txn_id(&id).map_err(ClientError::Internal)?;
            let result = client.txn_commit(bytes).await;
            clear_if_matches(session, bytes, &result);
            let resp = result?;
            Ok(Box::new(resp))
        }
        TxnCommand::Abort { id } => {
            let bytes = parse_txn_id(&id).map_err(ClientError::Internal)?;
            let result = client.txn_abort(bytes).await;
            clear_if_matches(session, bytes, &result);
            let resp = result?;
            Ok(Box::new(resp))
        }
    }
}

/// Drop `session.active_txn` if the call matched the active id AND
/// either succeeded or failed with a terminal txn error (the server
/// no longer knows the id or it's no longer Active). Other errors
/// (network, validation, …) leave the session untouched — the user
/// can retry.
fn clear_if_matches<T>(
    session: &mut Session,
    bytes: [u8; 16],
    result: &Result<T, ClientError>,
) {
    if session.active_txn != Some(bytes) {
        return;
    }
    let should_clear = match result {
        Ok(_) => true,
        Err(e) => is_txn_terminal(e),
    };
    if should_clear {
        session.active_txn = None;
    }
}
