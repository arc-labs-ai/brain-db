//! Standalone wire smoke for the devcontainer dev-server harness.
//!
//! Drives a *running* brain-server over the §04 wire protocol using the
//! canonical `brain-protocol` codec on a blocking `std::net::TcpStream`:
//! handshake (HELLO/AUTH) → ENCODE → RECALL, and asserts the encoded memory
//! comes back. Connects to `$BRAIN_SMOKE_ADDR` (default `127.0.0.1:9090`).
//!
//! This is dev tooling, not a client SDK — it exists to prove the data plane
//! end-to-end without pulling in a sibling repo. Run:
//!     cargo run -p brain-protocol --example wire_smoke
//!
//! Exits 0 on success; panics (non-zero) on any failure.

use std::io::{Read, Write};
use std::net::TcpStream;

use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::error::ProtocolError;
use brain_protocol::{
    EncodeRequest, Frame, Opcode, RecallRequest, RequestBody, ResponseBody, VERSION,
};

// EOS marks a complete single-frame message (high bit of the flags byte).
const FLAG_EOS: u8 = 0x80;

fn main() {
    let addr = std::env::var("BRAIN_SMOKE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    eprintln!("wire_smoke: connecting to {addr}");
    let mut stream = TcpStream::connect(&addr).expect("connect to brain-server");
    let mut buf: Vec<u8> = Vec::new();

    // ---- Handshake (stream 0) ----
    let hello = HelloPayload {
        client_id: "wire-smoke".into(),
        supported_versions: vec![VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send(
        &mut stream,
        Opcode::Hello,
        0,
        RequestBody::Hello(hello).encode(),
    );
    let welcome = read_response(&mut stream, &mut buf, Opcode::Welcome);
    let ResponseBody::Welcome(w) = welcome else {
        panic!("expected WELCOME, got {welcome:?}");
    };
    eprintln!(
        "wire_smoke: WELCOME server={} version={}",
        w.server_id, w.chosen_version
    );

    let auth = AuthPayload {
        method: AuthMethod::Token,
        credentials: AuthCredentials::Token(b"smoke-token".to_vec()),
    };
    send(
        &mut stream,
        Opcode::Auth,
        0,
        RequestBody::Auth(auth).encode(),
    );
    let auth_ok = read_response(&mut stream, &mut buf, Opcode::AuthOk);
    let ResponseBody::AuthOk(_) = auth_ok else {
        panic!("expected AUTH_OK, got {auth_ok:?}");
    };
    eprintln!("wire_smoke: AUTH_OK");

    // ---- ENCODE (client stream ids MUST be odd) ----
    let text = "the devcontainer wire smoke memory: the evening sky turned teal";
    let encode = EncodeRequest {
        text: text.into(),
        context_id: 1,
        request_id: [0x11u8; 16],
        txn_id: None,
        occurred_at_unix_nanos: None,
    };
    send(
        &mut stream,
        Opcode::EncodeReq,
        1,
        RequestBody::Encode(encode).encode(),
    );
    let encode_resp = read_response(&mut stream, &mut buf, Opcode::EncodeResp);
    let ResponseBody::Encode(er) = encode_resp else {
        panic!("expected ENCODE_RESP, got {encode_resp:?}");
    };
    eprintln!(
        "wire_smoke: ENCODE_RESP memory_id={:#x} salience={}",
        er.memory_id, er.salience
    );

    // ---- RECALL the just-encoded memory ----
    let recall = RecallRequest {
        cue_text: "what color did the evening sky turn".into(),
        subject_name: String::new(),
        max_results: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: true,
        request_id: Some([0x22u8; 16]),
        txn_id: None,
        agent_filter: vec![],
        include_other_agents: false,
    };
    send(
        &mut stream,
        Opcode::RecallReq,
        3,
        RequestBody::Recall(recall).encode(),
    );
    let recall_resp = read_response(&mut stream, &mut buf, Opcode::RecallResp);
    let ResponseBody::Recall(rr) = recall_resp else {
        panic!("expected RECALL_RESP, got {recall_resp:?}");
    };
    eprintln!(
        "wire_smoke: RECALL_RESP memories={} cumulative={}",
        rr.memories.len(),
        rr.cumulative_count
    );
    assert!(
        !rr.memories.is_empty(),
        "RECALL returned no memories — the encoded memory was not retrievable"
    );

    println!("wire_smoke: OK — handshake + encode + recall round-trip succeeded");
}

/// Write one EOS-terminated frame.
fn send(stream: &mut TcpStream, opcode: Opcode, stream_id: u32, payload: Vec<u8>) {
    let frame = Frame::new(opcode.as_u16(), FLAG_EOS, stream_id, payload);
    stream.write_all(&frame.encode()).expect("write frame");
    stream.flush().expect("flush");
}

/// Read the next whole frame, decode its body for `expected`, and surface a
/// server ERROR frame as a panic.
fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>, expected: Opcode) -> ResponseBody {
    let frame = read_frame(stream, buf);
    // The header stores the opcode as raw big-endian bytes.
    let opcode = u16::from_be_bytes(frame.header.opcode);
    if opcode == Opcode::Error.as_u16() {
        let err = ResponseBody::decode(Opcode::Error, &frame.payload);
        panic!("server returned ERROR frame: {err:?}");
    }
    if opcode != expected.as_u16() {
        panic!(
            "expected opcode {:#06x}, got {:#06x}",
            expected.as_u16(),
            opcode
        );
    }
    ResponseBody::decode(expected, &frame.payload).expect("decode response body")
}

/// Block until a complete frame is buffered, returning it and consuming its
/// bytes from `buf` (a partial trailing frame is retried on the next read).
fn read_frame(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Frame {
    loop {
        let decoded = match Frame::decode(buf) {
            Ok((frame, rest)) => Some((frame, buf.len() - rest.len())),
            Err(ProtocolError::Truncated { .. }) => None,
            Err(e) => panic!("frame decode failed: {e}"),
        };
        if let Some((frame, consumed)) = decoded {
            buf.drain(..consumed);
            return frame;
        }
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).expect("read from server");
        assert!(n != 0, "server closed the connection mid-frame");
        buf.extend_from_slice(&chunk[..n]);
    }
}
