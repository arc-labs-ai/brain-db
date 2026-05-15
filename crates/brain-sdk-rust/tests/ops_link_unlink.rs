//! LINK + UNLINK smoke tests.

mod common;

use brain_core::MemoryId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::EdgeKindWire;
use brain_protocol::response::{LinkResponse, UnlinkResponse};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn link_round_trip() {
    let src = MemoryId::pack(0, 1, 1);
    let tgt = MemoryId::pack(0, 2, 1);
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::LinkReq.as_u16());
        let body = RequestBody::decode(Opcode::LinkReq, &frame.payload).expect("decode");
        let req = match body {
            RequestBody::Link(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.source, src.raw());
        assert_eq!(req.target, tgt.raw());
        assert!((req.weight - 0.9).abs() < 1e-6);

        let resp = LinkResponse {
            source: req.source,
            target: req.target,
            kind: req.kind,
            weight: req.weight,
            created_at_unix_nanos: 12345,
            already_existed: false,
        };
        common::write_frame(
            &mut socket,
            Opcode::LinkResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Link(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client
        .link(src, EdgeKindWire::Caused, tgt)
        .weight(0.9)
        .send()
        .await
        .expect("link");
    assert!(!result.already_existed);
    assert_eq!(result.created_at_unix_nanos, 12345);
    client.bye().await.expect("bye");
}

#[tokio::test]
async fn unlink_round_trip() {
    let src = MemoryId::pack(0, 3, 1);
    let tgt = MemoryId::pack(0, 4, 1);
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::UnlinkReq.as_u16());
        let body = RequestBody::decode(Opcode::UnlinkReq, &frame.payload).expect("decode");
        let req = match body {
            RequestBody::Unlink(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.source, src.raw());
        assert_eq!(req.target, tgt.raw());

        let resp = UnlinkResponse {
            source: req.source,
            target: req.target,
            kind: req.kind,
            removed: true,
        };
        common::write_frame(
            &mut socket,
            Opcode::UnlinkResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Unlink(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client
        .unlink(src, EdgeKindWire::Caused, tgt)
        .send()
        .await
        .expect("unlink");
    assert!(result.removed);
    client.bye().await.expect("bye");
}
