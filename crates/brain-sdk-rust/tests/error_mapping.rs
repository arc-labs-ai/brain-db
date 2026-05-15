//! Error-frame → ClientError mapping smoke test.

mod common;

use brain_protocol::error::{ErrorCategory, ErrorCode};
use brain_protocol::opcode::Opcode;
use brain_protocol::response::{ErrorResponse, ResponseBody};
use brain_protocol::RequestBody;
use brain_sdk_rust::{Client, ClientConfig, ClientError, RetryConfig};

#[tokio::test]
async fn error_frame_maps_to_client_error_server() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EncodeReq.as_u16());
        let _ = RequestBody::decode(Opcode::EncodeReq, &frame.payload).expect("decode");

        let err = ErrorResponse {
            code: brain_protocol::response::ErrorCodeWire::from(ErrorCode::InvalidArgument),
            category: brain_protocol::response::ErrorCategoryWire::from(ErrorCategory::Validation),
            message: "bad input".into(),
            details: None,
            retry_after_ms: None,
        };
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Error(err).encode(),
            true,
        )
        .await;
    })
    .await;

    // Disable retries so we see the underlying Server error directly.
    let cfg = ClientConfig::default().with_retry(RetryConfig::none());
    let client = Client::connect_with(addr, brain_core::AgentId::new(), cfg)
        .await
        .expect("connect");
    let result = client.encode("x").send().await;
    match result {
        Err(ClientError::Server { code, message }) => {
            assert_eq!(message, "bad input");
            // ErrorCode::InvalidArgument is in the 0x0xxx range per spec §03/10.
            assert!(
                code > 0,
                "expected a non-zero error code, got 0x{:04x}",
                code
            );
        }
        other => panic!("expected ClientError::Server, got {other:?}"),
    }
    let _ = client.bye().await;
}
