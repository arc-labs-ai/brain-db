//! Schema SDK integration tests. Phase 19.10a.
//!
//! Uses the mock-server harness (`common::spawn_mock_server`) to
//! verify that:
//!
//! - `client.schema().upload_text()` serialises a `SchemaUploadRequest`
//!   over the wire and decodes a `SchemaUploadResponse` back.
//! - `.upload(&Schema)` renders the AST to DSL text via the canonical
//!   printer then sends the same wire frame.
//! - `.validate()` / `.get()` / `.list()` round-trip correctly.
//! - Validation-error responses surface as
//!   `SchemaUploadOutcome { schema_version: None, errors: [...] }`.
//!
//! Full end-to-end against a real shard lives in
//! `crates/brain-server/tests/knowledge_schema_*.rs`; this file
//! covers the SDK side without the storage stack.

mod common;

use brain_protocol::knowledge::{
    SchemaGetResponse, SchemaListItemWire, SchemaListResponseFrame, SchemaUploadResponse,
    SchemaValidateResponse, SchemaValidationErrorWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::schema::{
    AttrType, AttributeDecl, EntityTypeDef, ObjectTypeDecl, PredicateDef, StatementKindAst,
};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::knowledge::SchemaBuilder;
use brain_sdk_rust::Client;

const ACME_V1: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n";

// ---------------------------------------------------------------------------
// upload_text round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_text_round_trip() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::SchemaUploadReq.as_u16());
        let body = RequestBody::decode(Opcode::SchemaUploadReq, &frame.payload).unwrap();
        match body {
            RequestBody::SchemaUpload(r) => {
                assert!(r.schema_document.contains("namespace acme"));
                assert!(!r.dry_run);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::SchemaUploadResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaUpload(SchemaUploadResponse {
                namespace: "acme".into(),
                schema_version: 1,
                validation_errors: vec![],
                backward_compatible: true,
                migration_summary_blob: Vec::new(),
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .schema()
        .upload_text(ACME_V1)
        .await
        .expect("upload");
    assert_eq!(outcome.namespace, "acme");
    assert_eq!(outcome.schema_version, Some(1));
    assert!(outcome.errors.is_empty());
}

// ---------------------------------------------------------------------------
// upload(&Schema) via SchemaBuilder + DSL printer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_via_builder_round_trip() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        let body = RequestBody::decode(Opcode::SchemaUploadReq, &frame.payload).unwrap();
        match body {
            RequestBody::SchemaUpload(r) => {
                assert!(r.schema_document.contains("namespace acme"));
                assert!(r.schema_document.contains("define entity_type Foo"));
                assert!(r.schema_document.contains("define predicate prefers"));
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::SchemaUploadResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaUpload(SchemaUploadResponse {
                namespace: "acme".into(),
                schema_version: 1,
                validation_errors: vec![],
                backward_compatible: true,
                migration_summary_blob: Vec::new(),
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let schema = SchemaBuilder::new("acme")
        .entity_type(EntityTypeDef {
            name: "Foo".into(),
            attributes: vec![AttributeDecl {
                name: "label".into(),
                attr_type: AttrType::Text,
                required: false,
                unique: false,
                indexed: false,
                default: None,
            }],
        })
        .predicate(PredicateDef {
            name: "prefers".into(),
            kind: StatementKindAst::Preference,
            object: ObjectTypeDecl::Value {
                value_type: AttrType::Text,
            },
            description: None,
        })
        .build();

    let outcome = client.schema().upload(&schema).await.expect("upload");
    assert_eq!(outcome.schema_version, Some(1));
}

// ---------------------------------------------------------------------------
// validation errors surface as `schema_version: None`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_validation_error_surfaces_as_none_version() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::SchemaUploadResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaUpload(SchemaUploadResponse {
                namespace: String::new(),
                schema_version: 0,
                validation_errors: vec![SchemaValidationErrorWire {
                    code: "Syntax".into(),
                    message: "boom".into(),
                    line: 1,
                    column: 1,
                    length: 0,
                    severity: 2,
                }],
                backward_compatible: true,
                migration_summary_blob: Vec::new(),
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .schema()
        .upload_text("namespace 123\n")
        .await
        .expect("upload");
    assert!(outcome.schema_version.is_none());
    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].code, "Syntax");
    assert_eq!(outcome.errors[0].line, 1);
}

// ---------------------------------------------------------------------------
// validate / get / list.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validate_returns_would_be_version() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::SchemaValidateReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::SchemaValidateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaValidate(SchemaValidateResponse {
                namespace: "acme".into(),
                would_be_version: 2,
                validation_errors: vec![],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .schema()
        .validate(ACME_V1)
        .await
        .expect("validate");
    assert_eq!(outcome.namespace, "acme");
    assert_eq!(outcome.would_be_version, 2);
    assert!(outcome.errors.is_empty());
}

#[tokio::test]
async fn get_round_trips_view() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::SchemaGetReq.as_u16());
        let body = RequestBody::decode(Opcode::SchemaGetReq, &frame.payload).unwrap();
        match body {
            RequestBody::SchemaGet(r) => {
                assert_eq!(r.namespace, "acme");
                assert_eq!(r.version, 0);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::SchemaGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaGet(SchemaGetResponse {
                namespace: "acme".into(),
                schema_version: 1,
                schema_document: "namespace acme\n".into(),
                source_blob: b"{}".to_vec(),
                uploaded_at_unix_nanos: 42,
                validator_version: 1,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let view = client.schema().get("acme", 0).await.expect("get");
    assert_eq!(view.namespace, "acme");
    assert_eq!(view.schema_version, 1);
    assert_eq!(view.schema_document, "namespace acme\n");
    assert_eq!(view.source_blob, b"{}");
    assert_eq!(view.uploaded_at_unix_nanos, 42);
}

#[tokio::test]
async fn list_newest_first() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::SchemaListResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::SchemaList(SchemaListResponseFrame {
                namespace: "acme".into(),
                items: vec![
                    SchemaListItemWire {
                        schema_version: 2,
                        uploaded_at_unix_nanos: 200,
                        validator_version: 1,
                        has_source_text: true,
                    },
                    SchemaListItemWire {
                        schema_version: 1,
                        uploaded_at_unix_nanos: 100,
                        validator_version: 1,
                        has_source_text: true,
                    },
                ],
                total: 2,
                next_cursor: Vec::new(),
                is_final: true,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let view = client.schema().list("acme").await.expect("list");
    assert_eq!(view.namespace, "acme");
    assert_eq!(view.total, 2);
    assert_eq!(view.items.len(), 2);
    assert_eq!(view.items[0].schema_version, 2);
    assert_eq!(view.items[1].schema_version, 1);
}
