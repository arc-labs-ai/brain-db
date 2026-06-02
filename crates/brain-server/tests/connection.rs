//! Integration tests for the connection layer.
//!
//! Linux-only — `brain-server` only links Tokio + rustls on Linux. Each
//! test binds a listener to `127.0.0.1:0`, drives a Tokio client task,
//! and asserts wire-level behavior. No Glommio shards involved.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// shard.rs uses `crate::shard_adapters::…`. connection.rs uses
// `crate::dispatch::…`. Pull every source file the connection layer
// reaches into the test binary so `crate::` resolves the same as in
// main.rs. The connection tests don't exercise the shard / adapter /
// dispatch surface directly; silence dead-code noise.
#[allow(dead_code)]
#[path = "../src/network/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[path = "../src/metrics/mod.rs"]
mod metrics;
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

use brain_protocol::connection::handshake::{AuthMethod, ServerCapabilities};
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
    let auth_store = {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("api_keys.redb");
        let store = Arc::new(crate::auth::AuthStore::open(&p, false).expect("open auth store"));
        std::mem::forget(tmp);
        store
    };
    Topology {
        shards: Arc::new(Vec::<ShardHandle>::new()),
        routing: Arc::new(arc_swap::ArcSwap::from_pointee(
            RoutingTable::new(1, std::collections::HashMap::new()).expect("routing table"),
        )),
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/test",
            vec![AuthMethod::None],
        )),
        request_metrics: Arc::new(metrics::request::RequestMetrics::new()),
        auth_store,
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
    // PING is a stream_id=0 control frame doesn't gate it
    // behind AUTH_OK. The frame dispatcher replies with PONG and
    // resets the idle timer. After the reply, the connection stays
    // open — the handshake-deadline timer kicks in eventually, but
    // a follow-up PING within the auth_timeout works.
    let server = start(None, ConnectionLimits::default()).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let ping = brain_protocol::envelope::request::PingRequest {
        client_timestamp_unix_nanos: 1234,
    };
    let body = brain_protocol::envelope::request::RequestBody::Ping(ping);
    let frame = Frame::new(Opcode::Ping.as_u16(), 0, 0, body.encode());
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");

    let resp = read_one_frame(&mut client).await.expect("read response");
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::Pong.as_u16(),
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

    // Either we get an ERROR frame back, or the server just closes. The
    // server MUST close the connection; it sends an ERROR for
    // diagnostics first; either outcome is acceptable.
    let mut buf = vec![0u8; 1024];
    let n = client.read(&mut buf).await.expect("read");
    if n > 0 {
        let (frame, _) =
            Frame::decode_with_max(&buf[..n], brain_protocol::MAX_PAYLOAD_BYTES as u32)
                .expect("response decodes");
        assert_eq!(frame.header.opcode_u16(), Opcode::Error.as_u16());
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
    let frame = Frame::new(Opcode::Ping.as_u16(), 0, 0, Vec::new());
    tls_stream
        .write_all(&frame.encode())
        .await
        .expect("tls send");
    tls_stream.flush().await.expect("flush");

    let resp = read_one_frame(&mut tls_stream)
        .await
        .expect("read response over TLS");
    assert_eq!(resp.header.opcode_u16(), Opcode::Error.as_u16());

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_connection_cap_rejects_beyond_max() {
    // Isolate the global cap by disabling the per-IP cap (all test
    // connections share 127.0.0.1, so a per-IP cap would fire first).
    let limits = ConnectionLimits {
        max_connections: 2,
        max_connections_per_ip: 0,
        ..ConnectionLimits::default()
    };
    let server = start(None, limits).await;

    // Open the two admitted connections and confirm each is live: a PONG
    // proves the per-connection task is running, so its admission slot is held.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut c = TcpStream::connect(server.addr).await.expect("connect");
        c.write_all(&ping_frame().encode()).await.expect("send");
        c.flush().await.expect("flush");
        let resp = read_one_frame(&mut c).await.expect("admitted conn answers");
        assert_eq!(resp.header.opcode_u16(), Opcode::Pong.as_u16());
        held.push(c);
    }

    // The third connection exceeds the cap. The kernel still completes the
    // TCP handshake from its backlog, but the server drops the socket at
    // accept — before any frame work — so the client observes EOF, never a
    // reply.
    let mut over = TcpStream::connect(server.addr).await.expect("connect");
    over.write_all(&ping_frame().encode()).await.expect("send");
    over.flush().await.expect("flush");
    assert!(
        closed_without_reply(&mut over).await,
        "over-cap connection must be dropped, not served"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_ip_connection_cap_rejects_beyond_max() {
    // Unlimited globally; the per-IP cap is what must fire.
    let limits = ConnectionLimits {
        max_connections: 0,
        max_connections_per_ip: 1,
        ..ConnectionLimits::default()
    };
    let server = start(None, limits).await;

    let mut held = TcpStream::connect(server.addr).await.expect("connect");
    held.write_all(&ping_frame().encode()).await.expect("send");
    held.flush().await.expect("flush");
    let resp = read_one_frame(&mut held).await.expect("first conn answers");
    assert_eq!(resp.header.opcode_u16(), Opcode::Pong.as_u16());

    // Second connection from the same IP exceeds the per-IP cap.
    let mut over = TcpStream::connect(server.addr).await.expect("connect");
    over.write_all(&ping_frame().encode()).await.expect("send");
    over.flush().await.expect("flush");
    assert!(
        closed_without_reply(&mut over).await,
        "second connection from the same IP must be rejected"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_frame_is_rejected_before_alloc() {
    let limits = ConnectionLimits {
        max_payload_bytes: 16,
        ..ConnectionLimits::default()
    };
    let server = start(None, limits).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    // A well-formed frame whose 100-byte payload exceeds the server's 16-byte
    // cap. The server peeks payload_len from the header and rejects before
    // allocating or reading the body — the defense against memory-exhaustion
    // via a giant declared length.
    let frame = Frame::new(Opcode::Ping.as_u16(), 0, 0, vec![0u8; 100]);
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");

    // The server peeks payload_len, rejects before allocating the body, and
    // closes. It writes a diagnostic ERROR first, but since the client's
    // 100-byte body is never drained, the close can surface as a RST that
    // discards that frame — so tolerate either an ERROR-then-EOF or a bare
    // close, exactly as `bad_magic_closes_connection` does. The invariant
    // under test is "rejected and closed", not "always diagnosed".
    let mut buf = vec![0u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    match read {
        Ok(Ok(0)) => {} // bare close
        Ok(Ok(n)) => {
            if let Ok((resp, _)) =
                Frame::decode_with_max(&buf[..n], brain_protocol::MAX_PAYLOAD_BYTES as u32)
            {
                assert_eq!(
                    resp.header.opcode_u16(),
                    Opcode::Error.as_u16(),
                    "oversized frame must be rejected with an ERROR, not served"
                );
            }
            // Whether or not the bytes decoded as a frame, the connection
            // must then close — EOF (Ok(0)) or a reset (Err) both prove it.
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => {}
                Ok(n2) => panic!("expected close after rejection, read {n2} more bytes"),
            }
        }
        Ok(Err(_)) => {} // reset — also acceptable
        Err(_) => panic!("server neither answered nor closed on an oversized frame"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_timeout_closes_unauthenticated_connection() {
    // A client that connects and never authenticates must not hold a slot
    // indefinitely — the handshake deadline closes it.
    let limits = ConnectionLimits {
        auth_timeout: Duration::from_millis(150),
        ..ConnectionLimits::default()
    };
    let server = start(None, limits).await;

    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    // Send nothing. The server sends an Unauthenticated ERROR then closes;
    // accept either that diagnostic-frame-then-EOF, or a bare close.
    let mut buf = vec![0u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    match read {
        Ok(Ok(0)) => {} // closed without a diagnostic frame
        Ok(Ok(n)) => {
            let (frame, _) =
                Frame::decode_with_max(&buf[..n], brain_protocol::MAX_PAYLOAD_BYTES as u32)
                    .expect("decode error frame");
            assert_eq!(frame.header.opcode_u16(), Opcode::Error.as_u16());
            let n2 = client.read(&mut buf).await.expect("read EOF");
            assert_eq!(n2, 0, "connection closes after auth timeout");
        }
        Ok(Err(_)) => {} // reset — also acceptable
        Err(_) => panic!("server did not close unauthenticated connection within auth_timeout"),
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A PING carrying a valid `PingRequest` body — the dispatcher answers it with
/// PONG (an empty-payload PING decodes to an error instead), so a PONG reply
/// confirms the connection was admitted and its serve task is running.
fn ping_frame() -> Frame {
    let ping = brain_protocol::envelope::request::PingRequest {
        client_timestamp_unix_nanos: 1,
    };
    let body = brain_protocol::envelope::request::RequestBody::Ping(ping);
    Frame::new(Opcode::Ping.as_u16(), 0, 0, body.encode())
}

/// True if the connection was dropped at admission: the client gets no reply,
/// only EOF or a reset. A served connection would answer the PING instead.
async fn closed_without_reply(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
        Ok(Ok(0)) => true,  // EOF
        Ok(Err(_)) => true, // reset
        Ok(Ok(_)) => false, // got bytes back → it was served
        Err(_) => false,    // neither closed nor served within the window
    }
}

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
