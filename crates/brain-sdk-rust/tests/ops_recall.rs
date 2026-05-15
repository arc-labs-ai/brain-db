//! RECALL streaming-collect smoke test.

mod common;

use brain_protocol::opcode::Opcode;
use brain_protocol::request::MemoryKindWire;
use brain_protocol::response::{MemoryResult, RecallResponseFrame};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

fn mock_result(idx: u32, kind: MemoryKindWire) -> MemoryResult {
    MemoryResult {
        memory_id: idx as u128,
        text: format!("result-{idx}"),
        similarity_score: 1.0 / (idx as f32 + 1.0),
        confidence: 0.9,
        salience: 0.5,
        kind,
        context_id: 0,
        created_at_unix_nanos: 0,
        last_accessed_at_unix_nanos: 0,
        vector_offset: 0,
        vector_dim: 0,
        edges: None,
    }
}

#[tokio::test]
async fn recall_collects_multi_frame_stream() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RecallReq.as_u16());
        let body = RequestBody::decode(Opcode::RecallReq, &frame.payload).expect("decode");
        let req = match body {
            RequestBody::Recall(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.cue_text, "cat");
        assert_eq!(req.top_k, 5);
        let sid = frame.header.stream_id_u32();

        // 2 mid-stream frames (no EOS), 1 final.
        let r1 = RecallResponseFrame {
            results: vec![
                mock_result(0, MemoryKindWire::Episodic),
                mock_result(1, MemoryKindWire::Episodic),
            ],
            is_final: false,
            cumulative_count: 2,
            estimated_remaining: Some(1),
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
            results: vec![mock_result(2, MemoryKindWire::Semantic)],
            is_final: true,
            cumulative_count: 3,
            estimated_remaining: Some(0),
        };
        common::write_frame(
            &mut socket,
            Opcode::RecallResp.as_u16(),
            sid,
            ResponseBody::Recall(r2).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let results = client.recall("cat").top_k(5).send().await.expect("recall");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].memory_id, 0);
    assert_eq!(results[2].memory_id, 2);
    client.bye().await.expect("bye");
}
