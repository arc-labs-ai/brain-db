//! FORGET op smoke test.

mod common;

use brain_core::MemoryId;
use brain_protocol::opcode::Opcode;
use brain_protocol::response::ForgetResponse;
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn forget_round_trip() {
    let mid = MemoryId::pack(0, 42, 1);
    let mid_raw = mid.raw();
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::ForgetReq.as_u16());
        let body = RequestBody::decode(Opcode::ForgetReq, &frame.payload).expect("decode");
        let req = match body {
            RequestBody::Forget(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.memory_id, mid_raw);

        let resp = ForgetResponse {
            memory_id: req.memory_id,
            was_already_forgotten: false,
            edges_removed: 3,
        };
        common::write_frame(
            &mut socket,
            Opcode::ForgetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Forget(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client.forget(mid).send().await.expect("forget");
    assert_eq!(result.memory_id, mid_raw);
    assert_eq!(result.edges_removed, 3);
    client.bye().await.expect("bye");
}
