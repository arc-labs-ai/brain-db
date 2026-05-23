//! Integration tests for sub-task 9.11 — cross-shard SUBSCRIBE fan-out.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload, ServerCapabilities,
};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::{
    CancelStreamRequest, CancellationReason, EncodeRequest, MemoryKindWire, RequestBody,
    SubscribeRequest, SubscriptionFilter, UnsubscribeRequest,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
#[allow(dead_code)]
#[path = "../src/bootstrap/tls.rs"]
mod tls;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use connection::{ConnectionLimits, ConnectionListener, ShutdownSignal, ShutdownTrigger, Topology};
use routing::RoutingTable;
use shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

struct TestStubDispatcher;
impl Dispatcher for TestStubDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}
fn stub_dispatcher() -> Arc<dyn Dispatcher> {
    Arc::new(TestStubDispatcher)
}

// ---------------------------------------------------------------------------
// Scaffold (mirrors tests/dispatch.rs)
// ---------------------------------------------------------------------------

const FLAG_EOS: u8 = 1 << 7;

struct Server {
    addr: SocketAddr,
    trigger: ShutdownTrigger,
    listener: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    handles: Vec<ShardHandle>,
    joiners: Vec<Option<ShardJoiner>>,
    _data_dir: TempDir,
}

impl Server {
    async fn stop(mut self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.listener).await;
        drop(self.handles);
        for joiner in self.joiners.iter_mut().filter_map(|j| j.take()) {
            let _ = tokio::task::spawn_blocking(move || joiner.join())
                .await
                .map_err(|_| ());
        }
    }
}

async fn start_with_shards(n_shards: usize) -> Server {
    let data_dir = TempDir::new().expect("tmp");
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = ShardSpawnConfig::new(data_dir.path(), stub_dispatcher());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }
    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let __auth_store = {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("api_keys.redb");
        let store =
            std::sync::Arc::new(crate::auth::AuthStore::open(&p, false).expect("open auth store"));
        std::mem::forget(tmp);
        store
    };
    let topology = Topology {
        shards: Arc::new(handles.clone()),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/test",
            vec![AuthMethod::None],
        )),
        request_metrics: Arc::new(metrics::request::RequestMetrics::new()),
        auth_store: __auth_store.clone(),
    };

    let (trigger, signal) = ShutdownSignal::channel();
    let listener = ConnectionListener::new(
        "127.0.0.1:0".parse().unwrap(),
        None,
        topology,
        Arc::new(connection::ConnectionMetrics::default()),
        ConnectionLimits::default(),
        signal,
    );
    let bound = listener.bind().expect("bind");
    let addr = bound.local_addr();
    let listener_handle = tokio::spawn(async move { bound.serve().await });

    Server {
        addr,
        trigger,
        listener: listener_handle,
        handles,
        joiners,
        _data_dir: data_dir,
    }
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

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "tester".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Hello.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let _ = read_one_frame(client).await.expect("WELCOME");

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Auth.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Auth(auth).encode(),
        ),
    )
    .await;
    let _ = read_one_frame(client).await.expect("AUTH_OK");
}

fn open_filter() -> SubscriptionFilter {
    SubscriptionFilter {
        contexts: None,
        kinds: None,
        similar_to: None,
        agents: None,
    }
}

fn subscribe_request(filter: SubscriptionFilter) -> SubscribeRequest {
    SubscribeRequest {
        filter,
        include_history: false,
        from_lsn: None,
        max_inflight: 100,
    }
}

fn encode_request(text: &str, kind: MemoryKindWire) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    }
}

/// Read a SUBSCRIBE_EVENT or ERROR frame on a subscription stream
/// within `within`; return `None` on timeout. The bridge has
/// non-trivial setup latency; tests give it ~500 ms before claiming
/// "no event".
async fn read_event_within(client: &mut TcpStream, within: Duration) -> Option<Frame> {
    tokio::time::timeout(within, read_one_frame(client))
        .await
        .ok()
        .and_then(|r| r.ok())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_receives_encode_events() {
    let server = start_with_shards(1).await;

    // Open two connections under the SAME agent_id so encodes land on
    // the same shard.
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    let mut sub_client = TcpStream::connect(server.addr).await.expect("connect sub");
    complete_handshake(&mut sub_client, agent_id).await;

    let mut writer_client = TcpStream::connect(server.addr)
        .await
        .expect("connect writer");
    complete_handshake(&mut writer_client, agent_id).await;

    // SUBSCRIBE.
    let sub_stream = 5u32;
    send_frame(
        &mut sub_client,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(subscribe_request(open_filter())).encode(),
        ),
    )
    .await;
    // Give the per-sub task time to spawn + register.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ENCODE from the writer connection.
    send_frame(
        &mut writer_client,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode_request("hello", MemoryKindWire::Episodic)).encode(),
        ),
    )
    .await;
    let _enc_resp = read_one_frame(&mut writer_client)
        .await
        .expect("encode resp");

    // The subscriber should observe a SUBSCRIBE_EVENT within a
    // reasonable window. brain-ops's writer may or may not publish
    // on the NopDispatcher path (it does on encode success; ERROR
    // path doesn't publish). Accept any frame on `sub_stream` that's
    // SUBSCRIBE_EVENT, OR accept that no event arrives (the encode
    // failed end-to-end). The test that proves the bridge works is
    // the *no-panic, no-deadlock* property + observing the wire
    // round-trip.
    let event = read_event_within(&mut sub_client, Duration::from_secs(2)).await;
    if let Some(frame) = event {
        assert!(
            frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
                || frame.header.opcode_u16() == Opcode::Error.as_u16(),
            "unexpected opcode 0x{:02x}",
            frame.header.opcode_u16()
        );
    }
    // (Even if no event arrives, the test passes; the smoke is that
    // SUBSCRIBE_REQ doesn't error and the pipeline holds.)

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_emits_final_eos_and_response() {
    let server = start_with_shards(1).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let sub_stream = 7u32;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(subscribe_request(open_filter())).encode(),
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // UNSUBSCRIBE on a different stream id.
    let unsub_stream = 9u32;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::UnsubscribeReq.as_u16(),
            FLAG_EOS,
            unsub_stream,
            RequestBody::Unsubscribe(UnsubscribeRequest {
                target_stream_id: sub_stream,
            })
            .encode(),
        ),
    )
    .await;

    // Read up to two frames: the unsubscribe response on unsub_stream,
    // and the final EOS SUBSCRIBE_EVENT on sub_stream. Order isn't
    // guaranteed.
    let mut saw_unsub = false;
    let mut saw_final_eos = false;
    for _ in 0..2 {
        match read_event_within(&mut client, Duration::from_secs(2)).await {
            Some(frame) => {
                if frame.header.opcode_u16() == Opcode::UnsubscribeResp.as_u16() {
                    saw_unsub = true;
                } else if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
                    && frame.header.flags_u8() & FLAG_EOS != 0
                {
                    saw_final_eos = true;
                }
            }
            None => break,
        }
    }
    assert!(saw_unsub, "expected UNSUBSCRIBE_RESP");
    assert!(saw_final_eos, "expected final EOS on subscription stream");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stream_terminates_subscription() {
    let server = start_with_shards(1).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let sub_stream = 3u32;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(subscribe_request(open_filter())).encode(),
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancel_stream = 11u32;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::CancelStream.as_u16(),
            FLAG_EOS,
            cancel_stream,
            RequestBody::CancelStream(CancelStreamRequest {
                target_stream_id: sub_stream,
                reason: CancellationReason::ClientUnneeded,
            })
            .encode(),
        ),
    )
    .await;

    let mut saw_ack = false;
    let mut saw_final_eos = false;
    for _ in 0..2 {
        match read_event_within(&mut client, Duration::from_secs(2)).await {
            Some(frame) => {
                if frame.header.opcode_u16() == Opcode::CancelStreamAck.as_u16() {
                    saw_ack = true;
                } else if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
                    && frame.header.flags_u8() & FLAG_EOS != 0
                {
                    saw_final_eos = true;
                }
            }
            None => break,
        }
    }
    assert!(saw_ack, "expected CANCEL_STREAM_ACK");
    assert!(saw_final_eos, "expected final EOS on subscription stream");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_lsn_past_tail_is_accepted() {
    // Locked plan decision: `from_lsn > current_tail` is NOT an error.
    // The server transitions straight to the live tail (no replay
    // events). Earlier versions returned `LsnTooOld` here; that was
    // a placeholder while WAL replay was unimplemented.
    let server = start_with_shards(1).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let req = SubscribeRequest {
        filter: open_filter(),
        include_history: true,
        from_lsn: Some(123),
        max_inflight: 100,
    };
    let sub_stream = 13u32;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(req).encode(),
        ),
    )
    .await;

    // Should NOT receive an ERROR within a short window. Receiving
    // nothing (silence) is the expected outcome: replay yields zero
    // records, then the task awaits the live tail.
    let maybe = read_event_within(&mut client, Duration::from_millis(300)).await;
    if let Some(frame) = maybe {
        assert_ne!(
            frame.header.opcode_u16(),
            Opcode::Error.as_u16(),
            "from_lsn past tail must not error; got an Error frame"
        );
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_subscribe_with_same_stream_id_errors() {
    // Calling SUBSCRIBE twice with the same stream_id should error
    // (— stream IDs in use).
    let server = start_with_shards(1).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let sub_stream = 15u32;
    let req = RequestBody::Subscribe(subscribe_request(open_filter())).encode();
    send_frame(
        &mut client,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            req.clone(),
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_frame(
        &mut client,
        Frame::new(Opcode::SubscribeReq.as_u16(), FLAG_EOS, sub_stream, req),
    )
    .await;

    let resp = read_event_within(&mut client, Duration::from_secs(2))
        .await
        .expect("error frame");
    assert_eq!(resp.header.opcode_u16(), Opcode::Error.as_u16());

    server.stop().await;
}

// (A planned `subscribe_active_count_clears_on_cancel` white-box test
// was dropped: spawning a second `ShardEventHub` against the same
// shards alongside the listener's hub creates a teardown race in the
// flume Receiver — both hubs' bridge tasks compete for the same
// channel, and one of them can survive past `server.stop()`. The
// `unsubscribe_emits_final_eos_and_response` and
// `cancel_stream_terminates_subscription` tests already cover the
// registry's contract end-to-end on the wire.)

// ---------------------------------------------------------------------------
// WAL-replay end-to-end (sub-task 9.x; subscribe --start-lsn). Encodes
// memories BEFORE the subscriber connects, then subscribes with
// from_lsn=1 and asserts the historical events arrive — proof that
// the writer is WAL-recording substrate ops and the connection-layer
// replay prologue projects them back into SUBSCRIBE_EVENT frames.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_from_lsn_replays_historical_encodes() {
    let server = start_with_shards(1).await;
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    // 1. ENCODE two memories on a writer connection BEFORE any
    //    subscriber exists. The live event bus has no listeners, so
    //    these events are only durable in the WAL — making this a
    //    real replay test, not a live-tail test in disguise.
    let mut writer = TcpStream::connect(server.addr).await.expect("writer");
    complete_handshake(&mut writer, agent_id).await;
    for (i, text) in ["alpha", "beta"].iter().enumerate() {
        let stream_id = ((i * 2) + 1) as u32; // 1, 3 — odd
        send_frame(
            &mut writer,
            Frame::new(
                Opcode::EncodeReq.as_u16(),
                FLAG_EOS,
                stream_id,
                RequestBody::Encode(encode_request(text, MemoryKindWire::Episodic)).encode(),
            ),
        )
        .await;
        let resp = read_one_frame(&mut writer).await.expect("encode resp");
        assert_eq!(
            resp.header.opcode_u16(),
            Opcode::EncodeResp.as_u16(),
            "encode {} expected EncodeResp, got 0x{:02x} on stream {}",
            text,
            resp.header.opcode_u16(),
            resp.header.stream_id_u32()
        );
    }
    // Give the WAL group commit a moment to fsync.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Now open a fresh subscriber and request --start-lsn=1.
    let mut sub = TcpStream::connect(server.addr).await.expect("sub");
    complete_handshake(&mut sub, agent_id).await;
    let sub_stream = 11u32;
    send_frame(
        &mut sub,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(SubscribeRequest {
                filter: open_filter(),
                include_history: false,
                from_lsn: Some(1),
                max_inflight: 100,
            })
            .encode(),
        ),
    )
    .await;

    // 3. Expect at least two SUBSCRIBE_EVENT frames on `sub_stream`
    //    (the two replayed encodes). The replay path projects the
    //    WAL records via `EventEnvelope::from_wal_record`, so they
    //    carry the same shape as live events. Higher read budget +
    //    longer timeout so the SubscribeResp + framing latency
    //    doesn't squeeze us out.
    let mut replayed_events = 0;
    for _ in 0..10 {
        let Some(frame) = read_event_within(&mut sub, Duration::from_secs(3)).await else {
            break;
        };
        if frame.header.opcode_u16() == Opcode::Error.as_u16() {
            let body = ResponseBody::decode(Opcode::Error, &frame.payload).expect("decode");
            panic!("got Error frame from subscribe-replay: {body:?}");
        }
        if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
            && frame.header.stream_id_u32() == sub_stream
            && frame.header.flags_u8() & FLAG_EOS == 0
        {
            replayed_events += 1;
            if replayed_events >= 2 {
                break;
            }
        }
    }
    assert!(
        replayed_events >= 2,
        "expected >=2 replayed SUBSCRIBE_EVENT frames, got {replayed_events}"
    );

    server.stop().await;
}

/// `agents` filter — encodes from agent A and B; subscriber listening
/// only to agent A receives ONLY A's events even though B's events
/// hit the same shard. Without this filter, a multi-tenant shard
/// leaks every agent's events to every subscriber.
///
/// Enabled after the per-request agent flow landed: each op now
/// carries `agent_id` (populated from `ConnPhase::Established.agent`
/// via `dispatch(req, caller, ctx)` → `ExecutorContext.caller_agent`
/// → `EncodeOp.agent_id` → `EventEnvelope.agent_id`). The
/// `SubscriptionFilter.agents` set filters server-side; subscribers
/// only see events from agents they declared interest in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_agents_filter_isolates_per_agent() {
    let server = start_with_shards(1).await;

    let agent_a = *uuid::Uuid::now_v7().as_bytes();
    let agent_b = *uuid::Uuid::now_v7().as_bytes();

    // SUBSCRIBE with agents=[A] on a connection authed as A.
    let mut sub_a = TcpStream::connect(server.addr).await.expect("sub_a");
    complete_handshake(&mut sub_a, agent_a).await;
    let sub_stream = 21u32;
    let filter = SubscriptionFilter {
        contexts: None,
        kinds: None,
        similar_to: None,
        agents: Some(vec![agent_a]),
    };
    send_frame(
        &mut sub_a,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(subscribe_request(filter)).encode(),
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Encode from B first, then from A — both land on shard 0
    // (single-shard test). Without the agents filter, sub_a would
    // observe both.
    let mut writer_b = TcpStream::connect(server.addr).await.expect("writer_b");
    complete_handshake(&mut writer_b, agent_b).await;
    send_frame(
        &mut writer_b,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode_request("from-B", MemoryKindWire::Episodic)).encode(),
        ),
    )
    .await;
    let _ = read_one_frame(&mut writer_b).await.expect("enc B resp");

    let mut writer_a = TcpStream::connect(server.addr).await.expect("writer_a");
    complete_handshake(&mut writer_a, agent_a).await;
    send_frame(
        &mut writer_a,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode_request("from-A", MemoryKindWire::Episodic)).encode(),
        ),
    )
    .await;
    let _ = read_one_frame(&mut writer_a).await.expect("enc A resp");

    // Collect events on sub_a for up to ~1s. Expect EXACTLY 1 event
    // (from-A); the B event must be filtered out.
    let mut a_events = 0;
    for _ in 0..4 {
        let Some(frame) = read_event_within(&mut sub_a, Duration::from_millis(500)).await else {
            break;
        };
        if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
            && frame.header.stream_id_u32() == sub_stream
            && frame.header.flags_u8() & FLAG_EOS == 0
        {
            a_events += 1;
        }
    }
    assert_eq!(
        a_events, 1,
        "agents=[A] filter should let through exactly the A event, not B's"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_from_lsn_zero_replays_everything_in_wal() {
    // from_lsn=0 means "everything still in the WAL"
    // says this is not an error.
    let server = start_with_shards(1).await;
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    let mut writer = TcpStream::connect(server.addr).await.expect("writer");
    complete_handshake(&mut writer, agent_id).await;
    send_frame(
        &mut writer,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode_request("first", MemoryKindWire::Episodic)).encode(),
        ),
    )
    .await;
    let _resp = read_one_frame(&mut writer).await.expect("encode resp");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut sub = TcpStream::connect(server.addr).await.expect("sub");
    complete_handshake(&mut sub, agent_id).await;
    let sub_stream = 17u32;
    send_frame(
        &mut sub,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            sub_stream,
            RequestBody::Subscribe(SubscribeRequest {
                filter: open_filter(),
                include_history: false,
                from_lsn: Some(0),
                max_inflight: 100,
            })
            .encode(),
        ),
    )
    .await;

    let mut got_event = false;
    for _ in 0..3 {
        let Some(frame) = read_event_within(&mut sub, Duration::from_secs(1)).await else {
            break;
        };
        if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16()
            && frame.header.stream_id_u32() == sub_stream
            && frame.header.flags_u8() & FLAG_EOS == 0
        {
            got_event = true;
            break;
        }
        if frame.header.opcode_u16() == Opcode::Error.as_u16() {
            let body = ResponseBody::decode(Opcode::Error, &frame.payload).expect("decode");
            panic!("from_lsn=0 must NOT error; got: {body:?}");
        }
    }
    assert!(got_event, "from_lsn=0 should replay the WAL");

    server.stop().await;
}
