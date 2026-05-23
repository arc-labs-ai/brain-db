//! Metrics-snapshot integration smoke test.

mod common;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::response::EncodeResponse;
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn metrics_snapshot_records_request_and_op_breakdown() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        let _ = RequestBody::decode(Opcode::EncodeReq, &frame.payload).expect("decode");
        let resp = EncodeResponse {
            memory_id: 0xAB,
            was_deduplicated: false,
            salience: 0.5,
            auto_edges_added: 0,
            lsn: 0,
            agent_id: [0; 16],
            context_id: 0,
            kind: brain_protocol::envelope::request::MemoryKindWire::Episodic,
            created_at_unix_nanos: 0,
            edges_out_count: 0,
            embedding_model_fp: [0; 16],
            pending_stages: Vec::new(),
            has_active_schema: false,
            has_llm_extractor: false,
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

    let before = client.metrics_snapshot();
    assert_eq!(before.requests_total, 0);
    assert_eq!(before.errors_total, 0);

    let _ = client.encode("hi").send().await.expect("encode");

    let after = client.metrics_snapshot();
    assert_eq!(after.requests_total, 1);
    assert_eq!(after.errors_total, 0);
    assert_eq!(after.retries_total, 0);
    assert_eq!(after.in_flight_gauge, 0); // op finished
    assert_eq!(after.by_op["encode"].requests_total, 1);

    client.bye().await.expect("bye");
}
