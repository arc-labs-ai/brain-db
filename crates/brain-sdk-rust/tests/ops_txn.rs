//! TXN_BEGIN / TXN_COMMIT / TXN_ABORT smoke tests.

mod common;

use brain_protocol::opcode::Opcode;
use brain_protocol::response::{TxnAbortResponse, TxnBeginResponse, TxnCommitResponse};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn txn_begin_then_commit() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        // BEGIN
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::TxnBegin.as_u16());
        let body = RequestBody::decode(Opcode::TxnBegin, &frame.payload).expect("decode");
        let begin_req = match body {
            RequestBody::TxnBegin(r) => r,
            _ => panic!("wrong variant"),
        };
        let txn_id = begin_req.txn_id;
        let resp = TxnBeginResponse {
            txn_id,
            timeout_seconds: 30,
            started_at_unix_nanos: 1_000_000,
        };
        common::write_frame(
            &mut socket,
            Opcode::TxnBeginResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::TxnBegin(resp).encode(),
            true,
        )
        .await;

        // COMMIT
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::TxnCommit.as_u16());
        let body = RequestBody::decode(Opcode::TxnCommit, &frame.payload).expect("decode");
        let commit_req = match body {
            RequestBody::TxnCommit(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(commit_req.txn_id, txn_id);
        let resp = TxnCommitResponse {
            txn_id,
            committed_at_unix_nanos: 1_100_000,
            operations_applied: 3,
        };
        common::write_frame(
            &mut socket,
            Opcode::TxnCommitResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::TxnCommit(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let begin = client.txn_begin().await.expect("begin");
    let commit = client.txn_commit(begin.txn_id).await.expect("commit");
    assert_eq!(commit.txn_id, begin.txn_id);
    assert_eq!(commit.operations_applied, 3);
    client.bye().await.expect("bye");
}

#[tokio::test]
async fn txn_begin_then_abort() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        let begin_req = match RequestBody::decode(Opcode::TxnBegin, &frame.payload).unwrap() {
            RequestBody::TxnBegin(r) => r,
            _ => panic!(),
        };
        let txn_id = begin_req.txn_id;
        common::write_frame(
            &mut socket,
            Opcode::TxnBeginResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::TxnBegin(TxnBeginResponse {
                txn_id,
                timeout_seconds: 30,
                started_at_unix_nanos: 1_000_000,
            })
            .encode(),
            true,
        )
        .await;

        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::TxnAbort.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::TxnAbortResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::TxnAbort(TxnAbortResponse {
                txn_id,
                operations_discarded: 1,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let begin = client.txn_begin().await.expect("begin");
    let abort = client.txn_abort(begin.txn_id).await.expect("abort");
    assert_eq!(abort.operations_discarded, 1);
    client.bye().await.expect("bye");
}
