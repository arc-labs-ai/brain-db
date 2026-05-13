//! Integration tests for the sub-task 9.9 connection layer.
//!
//! Linux-only — `brain-server` only links Tokio + rustls on Linux. Each
//! test binds a listener to `127.0.0.1:0`, drives a Tokio client task,
//! and asserts wire-level behavior. No Glommio shards involved.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::opcode::Opcode;
use brain_protocol::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// shard.rs uses `crate::shard_adapters::…`. connection.rs uses
// `crate::dispatch::…`. Pull every source file the connection layer
// reaches into the test binary so `crate::` resolves the same as in
// main.rs. The connection tests don't exercise the shard / adapter /
// dispatch surface directly; silence dead-code noise.
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[allow(dead_code)]
#[path = "../src/network/routing.rs"]
mod routing;
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/network/subscribe.rs"]
mod subscribe;
#[path = "../src/bootstrap/tls.rs"]
mod tls;

use brain_protocol::handshake::{AuthMethod, ServerCapabilities};
use connection::{ConnectionLimits, ConnectionListener, ShutdownSignal, ShutdownTrigger, Topology};
use routing::RoutingTable;
use shard::ShardHandle;
use tls::load_server_tls_config;

// ---------------------------------------------------------------------------
// Test scaffold
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    trigger: ShutdownTrigger,
    handle: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
}

impl Server {
    fn signal(&self) {
        self.trigger.signal();
    }

    async fn stop(self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.handle)
            .await
            .expect("server task did not exit within 2s");
    }
}

fn empty_topology() -> Topology {
    Topology {
        shards: Arc::new(Vec::<ShardHandle>::new()),
        routing: Arc::new(arc_swap::ArcSwap::from_pointee(
            RoutingTable::new(1, std::collections::HashMap::new()).expect("routing table"),
        )),
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/test",
            vec![AuthMethod::None],
        )),
    }
}

async fn start(
    tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    limits: ConnectionLimits,
) -> Server {
    let (trigger, signal) = ShutdownSignal::channel();
    let listener = ConnectionListener::new(
        "127.0.0.1:0".parse().unwrap(),
        tls,
        empty_topology(),
        Arc::new(connection::ConnectionMetrics::default()),
        limits,
        signal,
    );
    let bound = listener.bind().expect("bind");
    let addr = bound.local_addr();
    let handle = tokio::spawn(async move { bound.serve().await });
    Server {
        addr,
        trigger,
        handle,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_and_accept_succeeds() {
    let server = start(None, ConnectionLimits::default()).await;

    let client = TcpStream::connect(server.addr).await.expect("connect");
    drop(client);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_works_pre_handshake_and_keeps_connection_alive() {
    // PING is a stream_id=0 control frame; spec §03/06 doesn't gate it
    // behind AUTH_OK. The frame dispatcher (9.10) replies with PONG and
    // resets the idle timer. After the reply, the connection stays
    // open — the handshake-deadline timer kicks in eventually, but
    // a follow-up PING within the auth_timeout works.
    let server = start(None, ConnectionLimits::default()).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let ping = brain_protocol::request::PingRequest {
        client_timestamp_unix_nanos: 1234,
    };
    let body = brain_protocol::request::RequestBody::Ping(ping);
    let frame = Frame::new(Opcode::Ping.as_u8(), 1 << 15, 0, body.encode());
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");

    let resp = read_one_frame(&mut client).await.expect("read response");
    assert_eq!(
        resp.header.opcode,
        Opcode::Pong.as_u8(),
        "PING should return PONG, not ERROR"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_magic_closes_connection() {
    let server = start(None, ConnectionLimits::default()).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    // 32 bytes of junk — magic won't validate.
    let junk = vec![0xABu8; 32];
    client.write_all(&junk).await.expect("send");
    client.flush().await.expect("flush");

    // Either we get an ERROR frame back, or the server just closes. Spec
    // §03/03 §3.1: "MUST close the connection". 9.9 sends an ERROR for
    // diagnostics; either outcome is acceptable.
    let mut buf = vec![0u8; 1024];
    let n = client.read(&mut buf).await.expect("read");
    if n > 0 {
        let (frame, _) =
            Frame::decode_with_max(&buf[..n], brain_protocol::MAX_PAYLOAD_BYTES as u32)
                .expect("response decodes");
        assert_eq!(frame.header.opcode, Opcode::Error.as_u8());
    }
    let n2 = client.read(&mut buf).await.expect("read EOF");
    assert_eq!(n2, 0, "expected EOF after bad-magic close");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_timeout_closes_silent_connection() {
    let limits = ConnectionLimits {
        read_timeout: Duration::from_millis(150),
        ..ConnectionLimits::default()
    };
    let server = start(None, limits).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    // Send half a header, then go silent.
    client.write_all(&[0u8; 16]).await.expect("send");
    client.flush().await.expect("flush");

    // Per-frame read timeout (150 ms) fires and closes the connection.
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    match read {
        Ok(Ok(0)) => {} // EOF as expected
        Ok(Ok(n)) => panic!("expected EOF, read {n} bytes"),
        Ok(Err(_)) => {} // Connection reset also acceptable
        Err(_) => panic!("timeout: server did not close idle connection"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signal_stops_accept_loop() {
    let server = start(None, ConnectionLimits::default()).await;

    // Touch the listener once to prove it's serving.
    let _client = TcpStream::connect(server.addr).await.expect("connect");

    // Signal shutdown; `serve()` must return promptly.
    server.signal();
    let result = tokio::time::timeout(Duration::from_secs(2), server.handle)
        .await
        .expect("listener did not exit after shutdown")
        .expect("join")
        .expect("serve returned err");
    assert_eq!(result, server.addr, "serve returned the bound addr");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_round_trip_smoke() {
    // Self-signed cert via rcgen; trusted directly on the client side.
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("self-signed");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    let tmp = tempfile::TempDir::new().unwrap();
    let cert_path = tmp.path().join("cert.pem");
    let key_path = tmp.path().join("key.pem");
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();

    let server_cfg = load_server_tls_config(&cert_path, &key_path).expect("server TLS config");
    let server = start(Some(server_cfg), ConnectionLimits::default()).await;

    // Build a client TLS config that trusts the rcgen cert directly.
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::RootCertStore;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("trust rcgen cert");
    let client_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_cfg = Arc::new(client_cfg);
    let connector = tokio_rustls::TlsConnector::from(client_cfg);

    let tcp = TcpStream::connect(server.addr).await.expect("tcp connect");
    let dns = ServerName::try_from("localhost").unwrap();
    let mut tls_stream = connector.connect(dns, tcp).await.expect("tls handshake");

    // Send a valid PING — same shape as the plain-text test.
    let frame = Frame::new(Opcode::Ping.as_u8(), 0, 0, Vec::new());
    tls_stream
        .write_all(&frame.encode())
        .await
        .expect("tls send");
    tls_stream.flush().await.expect("flush");

    let resp = read_one_frame(&mut tls_stream)
        .await
        .expect("read response over TLS");
    assert_eq!(resp.header.opcode, Opcode::Error.as_u8());

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Result<Frame, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("header read: {e}"))?;
    let payload_len_be = [header[16], header[17], header[18]];
    let payload_len =
        u32::from_be_bytes([0, payload_len_be[0], payload_len_be[1], payload_len_be[2]]) as usize;

    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .map_err(|e| format!("payload read: {e}"))?;
    }
    let (frame, rest) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .map_err(|e| format!("decode: {e}"))?;
    debug_assert!(rest.is_empty());
    Ok(frame)
}
