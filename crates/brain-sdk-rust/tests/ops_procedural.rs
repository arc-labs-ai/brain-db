//! `MATERIALIZE_PROCEDURAL` SDK smoke test.

mod common;

use brain_protocol::{MaterializeProceduralRequest, MaterializeProceduralResponse};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;

#[tokio::test]
async fn materialize_procedural_round_trip() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::MaterializeProceduralReq.as_u16()
        );
        let body =
            RequestBody::decode(Opcode::MaterializeProceduralReq, &frame.payload).expect("decode");
        let req: MaterializeProceduralRequest = match body {
            RequestBody::MaterializeProcedural(r) => r,
            _ => panic!("wrong variant"),
        };
        // Builder defaults: top_k=20, min_confidence=0.5, no categories,
        // no agent override (= [0; 16]).
        assert_eq!(req.top_k, 20);
        assert!((req.min_confidence - 0.5).abs() < 1e-6);
        assert!(req.categories.is_empty());
        assert_eq!(req.agent_id, [0u8; 16]);

        let resp = MaterializeProceduralResponse {
            system_block: "# Learned behaviors\n\n- be concise (0.90)\n".into(),
            statement_ids: vec![[7u8; 16]],
            total_candidates: 1,
            trimmed_by_budget: false,
        };
        common::write_frame(
            &mut socket,
            Opcode::MaterializeProceduralResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::MaterializeProcedural(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let resp = client
        .materialize_procedural()
        .send()
        .await
        .expect("materialize");
    assert_eq!(resp.total_candidates, 1);
    assert!(!resp.trimmed_by_budget);
    assert_eq!(resp.statement_ids.len(), 1);
    assert!(resp.system_block.contains("be concise"));
    client.bye().await.expect("bye");
}

#[tokio::test]
async fn materialize_procedural_passes_builder_knobs() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        let body =
            RequestBody::decode(Opcode::MaterializeProceduralReq, &frame.payload).expect("decode");
        let req: MaterializeProceduralRequest = match body {
            RequestBody::MaterializeProcedural(r) => r,
            _ => panic!("wrong variant"),
        };
        assert_eq!(req.top_k, 5);
        assert!((req.min_confidence - 0.75).abs() < 1e-6);
        assert_eq!(
            req.categories,
            vec!["tone".to_string(), "style".to_string()]
        );

        let resp = MaterializeProceduralResponse {
            system_block: String::new(),
            statement_ids: Vec::new(),
            total_candidates: 0,
            trimmed_by_budget: false,
        };
        common::write_frame(
            &mut socket,
            Opcode::MaterializeProceduralResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::MaterializeProcedural(resp).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let _ = client
        .materialize_procedural()
        .top_k(5)
        .min_confidence(0.75)
        .category("tone")
        .category("style")
        .send()
        .await
        .expect("materialize");
    client.bye().await.expect("bye");
}
