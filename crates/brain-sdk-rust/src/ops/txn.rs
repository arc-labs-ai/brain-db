//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT (spec §07/9-11 + §13/02 §9).
//!
//! 10.5 ships these as plain `Client::txn_*` methods returning the
//! TxnId / response. The fluent `let txn = client.txn().begin();`
//! sugar is a Phase-11 polish — for now the user threads `TxnId`
//! into op builders via `.txn(id)`.

use brain_protocol::opcode::Opcode;
use brain_protocol::request::{TxnAbortRequest, TxnBeginRequest, TxnCommitRequest};
use brain_protocol::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};
use brain_protocol::{Frame, RequestBody, ResponseBody};
use uuid::Uuid;

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

/// Default txn timeout — spec §07/9 default 30 s.
pub const DEFAULT_TXN_TIMEOUT_SECONDS: u32 = 30;

pub(crate) async fn txn_begin(
    client: &Client,
    timeout_seconds: u32,
) -> Result<TxnBeginResponse, ClientError> {
    // Mint a fresh txn id (UUIDv7).
    let txn_id_bytes: [u8; 16] = *Uuid::now_v7().as_bytes();
    let client_inner = client.clone();
    client
        .run_op("txn_begin", || {
            let client = client_inner.clone();
            async move {
                let body = RequestBody::TxnBegin(TxnBeginRequest {
                    txn_id: txn_id_bytes,
                    timeout_seconds,
                });
                let mut guard = client.acquire().await?;
                let stream_id = guard.next_stream_id();
                let frame =
                    Frame::new(Opcode::TxnBegin.as_u16(), FLAG_EOS, stream_id, body.encode());
                let resp = send_and_read_one(&mut guard, frame, Opcode::TxnBeginResp).await?;
                match ResponseBody::decode(Opcode::TxnBeginResp, &resp.payload)? {
                    ResponseBody::TxnBegin(r) => Ok(r),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "TxnBeginAck opcode but body variant didn't match".into(),
                        ),
                    )),
                }
            }
        })
        .await
}

pub(crate) async fn txn_commit(
    client: &Client,
    txn_id: [u8; 16],
) -> Result<TxnCommitResponse, ClientError> {
    let client_inner = client.clone();
    client
        .run_op("txn_commit", || {
            let client = client_inner.clone();
            async move {
                let body = RequestBody::TxnCommit(TxnCommitRequest { txn_id });
                let mut guard = client.acquire().await?;
                let stream_id = guard.next_stream_id();
                let frame = Frame::new(
                    Opcode::TxnCommit.as_u16(),
                    FLAG_EOS,
                    stream_id,
                    body.encode(),
                );
                let resp = send_and_read_one(&mut guard, frame, Opcode::TxnCommitResp).await?;
                match ResponseBody::decode(Opcode::TxnCommitResp, &resp.payload)? {
                    ResponseBody::TxnCommit(r) => Ok(r),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "TxnCommitAck opcode but body variant didn't match".into(),
                        ),
                    )),
                }
            }
        })
        .await
}

pub(crate) async fn txn_abort(
    client: &Client,
    txn_id: [u8; 16],
) -> Result<TxnAbortResponse, ClientError> {
    let client_inner = client.clone();
    client
        .run_op("txn_abort", || {
            let client = client_inner.clone();
            async move {
                let body = RequestBody::TxnAbort(TxnAbortRequest { txn_id });
                let mut guard = client.acquire().await?;
                let stream_id = guard.next_stream_id();
                let frame =
                    Frame::new(Opcode::TxnAbort.as_u16(), FLAG_EOS, stream_id, body.encode());
                let resp = send_and_read_one(&mut guard, frame, Opcode::TxnAbortResp).await?;
                match ResponseBody::decode(Opcode::TxnAbortResp, &resp.payload)? {
                    ResponseBody::TxnAbort(r) => Ok(r),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "TxnAbortAck opcode but body variant didn't match".into(),
                        ),
                    )),
                }
            }
        })
        .await
}
