//! Transactions: TXN_BEGIN / TXN_COMMIT / TXN_ABORT stubs.
//!
//! Real implementation lands in sub-task 7.9 — single-shard, buffered
//! op list, bounded duration per spec §09/08.

use brain_protocol::request::{TxnAbortRequest, TxnBeginRequest, TxnCommitRequest};
use brain_protocol::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_txn_begin(
    _req: TxnBeginRequest,
    _ctx: &OpsContext,
) -> Result<TxnBeginResponse, OpError> {
    Err(OpError::NotYetImplemented("TXN_BEGIN — sub-task 7.9"))
}

pub async fn handle_txn_commit(
    _req: TxnCommitRequest,
    _ctx: &OpsContext,
) -> Result<TxnCommitResponse, OpError> {
    Err(OpError::NotYetImplemented("TXN_COMMIT — sub-task 7.9"))
}

pub async fn handle_txn_abort(
    _req: TxnAbortRequest,
    _ctx: &OpsContext,
) -> Result<TxnAbortResponse, OpError> {
    Err(OpError::NotYetImplemented("TXN_ABORT — sub-task 7.9"))
}
