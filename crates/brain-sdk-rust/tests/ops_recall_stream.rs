//! RECALL streaming-form smoke test.

mod common;

use brain_protocol::opcode::Opcode;
use brain_protocol::request::MemoryKindWire;
use brain_protocol::response::{MemoryResult, RecallResponseFrame};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;
use futures_lite::StreamExt;

fn mock_result(idx: u32) -> MemoryResult {
    MemoryResult {
        memory_id: idx as u128,
        text: format!("r-{idx}"),
        similarity_score: 1.0,
        confidence: 0.9,
        salience: 0.5,
        kind: MemoryKindWire::Episodic,
        context_id: 0,
        created_at_unix_nanos: 0,
        last_accessed_at_unix_nanos: 0,
        vector_offset: 0,
        vector_dim: 0,
        edges: None,
    }
}

#[tokio::test]
async fn recall_stream_yields_items_one_at_a_time() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RecallReq.as_u16());
        let _ = RequestBody::decode(Opcode::RecallReq, &frame.payload).expect("decode");
        let sid = frame.header.stream_id_u32();

        // 2 mid-stream frames + 1 final, 4 items total.
        let r1 = RecallResponseFrame {
            results: vec![mock_result(0), mock_result(1)],
            is_final: false,
            cumulative_count: 2,
            estimated_remaining: Some(2),
        };
        common::write_frame(
            &mut socket,
            Opcode::RecallResp.as_u16(),
            sid,
            ResponseBody::Recall(r1).encode(),
            false,
        )
        .await;

        let r2 = RecallResponseFrame {
            results: vec![mock_result(2)],
            is_final: false,
            cumulative_count: 3,
            estimated_remaining: Some(1),
        };
        common::write_frame(
            &mut socket,
            Opcode::RecallResp.as_u16(),
            sid,
            ResponseBody::Recall(r2).encode(),
            false,
        )
        .await;

        let r3 = RecallResponseFrame {
            results: vec![mock_result(3)],
            is_final: true,
            cumulative_count: 4,
            estimated_remaining: Some(0),
        };
        common::write_frame(
            &mut socket,
            Opcode::RecallResp.as_u16(),
            sid,
            ResponseBody::Recall(r3).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let mut stream = client
        .recall("cat")
        .top_k(10)
        .send_stream()
        .await
        .expect("open stream");

    let mut ids = Vec::new();
    while let Some(item) = stream.next().await {
        ids.push(item.expect("item").memory_id);
    }
    assert_eq!(ids, vec![0, 1, 2, 3]);
    drop(stream);
    client.bye().await.expect("bye");
}
