//! ENCODE op smoke test.

mod common;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::MemoryKindWire;
use brain_protocol::envelope::response::EncodeResponse;
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn encode_round_trip() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EncodeReq.as_u16());
        let body = RequestBody::decode(Opcode::EncodeReq, &frame.payload).expect("decode");
        let req = match body {
            RequestBody::Encode(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.text, "hello brain");
        assert!((req.salience_hint - 0.75).abs() < 1e-6);

        let resp = EncodeResponse {
            memory_id: 0x0001_0000_0000_0000_0000_0000_0000_0001u128,
            was_deduplicated: false,
            salience: req.salience_hint,
            auto_edges_added: 0,
            lsn: 0,
            agent_id: [0; 16],
            context_id: req.context_id,
            kind: req.kind,
            created_at_unix_nanos: 0,
            edges_out_count: 0,
            embedding_model_fp: [0; 16],
            pending_stages: Vec::new(),
            has_active_schema: false,
        };
        common::write_frame(
            &mut socket,
            Opcode::EncodeResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Encode(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client
        .encode("hello brain")
        .kind(MemoryKindWire::Episodic)
        .salience(0.75)
        .send()
        .await
        .expect("encode");
    assert_eq!(
        result.memory_id,
        0x0001_0000_0000_0000_0000_0000_0000_0001u128
    );
    assert!(!result.was_deduplicated);
    client.bye().await.expect("bye");
}
