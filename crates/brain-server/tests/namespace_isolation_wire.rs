//! Multi-tenant (namespace) isolation over the full wire stack.
//!
//! This is the gate the dispatch namespace-resolution bug slipped past: an
//! authenticated caller's key namespace must resolve to a real per-shard
//! `NamespaceId` and scope every owned row — never silently fall back to the
//! reserved SYSTEM namespace, which would collapse all tenants into one bucket.
//!
//! To make these assertions about the *namespace* boundary specifically (and
//! not the orthogonal per-space filter that `space_isolation.rs` already
//! covers), every connection here binds the SAME space_id under DIFFERENT
//! namespaces. With the space held constant, the only thing that can keep one
//! tenant's data out of another's is the namespace scope. If dispatch resolved
//! both keys to SYSTEM (the bug), the two tenants would share one
//! (namespace, space) bucket: a foreign `ENTITY_GET` would succeed and a name
//! shared across tenants would resolve across the boundary — both asserted
//! against here.
//!
//! The isolation gate runs on the typed-graph resolver path (ENTITY_CREATE /
//! ENTITY_RESOLVE / ENTITY_GET), which reads metadata synchronously, rather
//! than RECALL: the harness embeds with a zero-vector stub dispatcher, so the
//! semantic read path is degenerate and returns no content to assert on. We
//! still ENCODE under each tenant to prove the write path is *accepted* under a
//! real namespace (the fail-closed dispatch path would otherwise reject it).
//!
//! A single shard (`start(1)`) collocates everything so what's proven is the
//! logical namespace filter, not incidental physical separation.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    EntityCreateRequest, EntityGetRequest, EntityResolveRequest, ResolutionOutcomeWire,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
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

mod support_harness;

use support_harness::start;

const FLAG_EOS: u8 = 1 << 7;
/// The seeded `brain:Person` entity type. A user namespace with no uploaded
/// schema interns entity creates open-vocab, so any type id is accepted.
const PERSON_TYPE_ID: u32 = 1;

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Frame
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("header read");
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload read");
    }
    let (frame, rest) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    debug_assert!(rest.is_empty());
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    req: RequestBody,
) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    send_frame(
        client,
        Frame::new(opcode, FLAG_EOS, stream_id, req.encode()),
    )
    .await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

/// Handshake presenting `token` and return the server-derived `AuthOkPayload`
/// so the caller can assert which tenant the connection was bound to.
async fn handshake_authok(client: &mut TcpStream, token: &[u8]) -> AuthOkPayload {
    let hello = HelloPayload {
        client_id: "ns-isolation-tester".into(),
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
    let welcome = read_one_frame(client).await;
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::Token,
        credentials: AuthCredentials::Token(token.to_vec()),
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
    let auth_ok = read_one_frame(client).await;
    assert_eq!(
        auth_ok.header.opcode_u16(),
        Opcode::AuthOk.as_u16(),
        "expected AuthOk, got 0x{:04x}",
        auth_ok.header.opcode_u16()
    );
    AuthOkPayload::decode(&auth_ok.payload).expect("decode AuthOk")
}

/// Encode `text`; returns the assigned `memory_id`. Proves the write path is
/// *accepted* under the caller's tenant (the fail-closed dispatch path rejects
/// a namespace-less or SYSTEM-resolving caller before any write happens).
async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode failed: opcode={opcode} body={other:?}"),
    }
}

/// Create an entity by canonical name; returns its `entity_id`.
async fn create_entity(client: &mut TcpStream, stream_id: u32, name: &str) -> [u8; 16] {
    let (opcode, body) = round_trip(
        client,
        stream_id,
        RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: PERSON_TYPE_ID,
            canonical_name: name.into(),
            aliases: vec![],
            attributes_blob: Vec::new(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
            act_as: None,
        }),
    )
    .await;
    assert_eq!(
        opcode,
        Opcode::EntityCreateResp.as_u16(),
        "entity create failed: {body:?}"
    );
    match body {
        ResponseBody::EntityCreate(r) => r.entity_id,
        other => panic!("expected EntityCreateResp, got {other:?}"),
    }
}

/// Resolve a candidate name without creating; returns the resolve response.
async fn resolve_entity(
    client: &mut TcpStream,
    stream_id: u32,
    name: &str,
) -> brain_protocol::EntityResolveResponse {
    let (opcode, body) = round_trip(
        client,
        stream_id,
        RequestBody::EntityResolve(EntityResolveRequest {
            candidate_name: name.into(),
            context: String::new(),
            entity_type_hint: 0,
            allow_create: false,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
            act_as: None,
        }),
    )
    .await;
    assert_eq!(
        opcode,
        Opcode::EntityResolveResp.as_u16(),
        "entity resolve failed: {body:?}"
    );
    match body {
        ResponseBody::EntityResolve(r) => r,
        other => panic!("expected EntityResolveResp, got {other:?}"),
    }
}

/// True iff `entity_id` is readable by this connection. A foreign-tenant id is
/// walled off and comes back as an ERROR frame (NotFound).
async fn entity_get_visible(client: &mut TcpStream, stream_id: u32, entity_id: [u8; 16]) -> bool {
    let (opcode, _body) = round_trip(
        client,
        stream_id,
        RequestBody::EntityGet(EntityGetRequest {
            entity_id,
            act_as: None,
        }),
    )
    .await;
    opcode == Opcode::EntityGetResp.as_u16()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The end-to-end tenancy gate. Two keys bound to DIFFERENT namespaces but the
/// SAME space must be fully isolated: AUTH_OK echoes each tenant, a name shared
/// across tenants resolves only within each tenant, and a foreign entity id is
/// unreadable. Holding the space constant pins the assertion to the namespace
/// boundary — the exact path the dispatch SYSTEM-fallback bug defeated (under
/// which both keys share one (SYSTEM, space) bucket and these all leak).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_namespaces_are_isolated_end_to_end() {
    let server = start(1).await; // one shard → both tenants collocated

    // Same space under both tenants, so only the namespace can separate them.
    let space = [0xCCu8; 16];
    let acme_key = server.mint("acme", space, brain_metadata::api_keys::bits::FULL);
    let globex_key = server.mint("globex", space, brain_metadata::api_keys::bits::FULL);

    let mut acme = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect acme");
    let mut globex = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect globex");

    // (a) Each connection's AUTH_OK echoes its own tenant.
    let acme_authok = handshake_authok(&mut acme, &acme_key).await;
    let globex_authok = handshake_authok(&mut globex, &globex_key).await;
    assert_eq!(
        acme_authok.namespace, "acme",
        "acme connection must bind to its own tenant"
    );
    assert_eq!(
        globex_authok.namespace, "globex",
        "globex connection must bind to its own tenant"
    );

    // The write path is accepted under a real (non-SYSTEM) namespace; a
    // namespace-less or SYSTEM-resolving caller is rejected before this point.
    let _acme_mem = encode(&mut acme, 1, "acme secret: the vault code is alpha").await;
    let _globex_mem = encode(&mut globex, 1, "globex secret: the vault code is omega").await;

    // Same canonical name under each tenant mints distinct entities.
    let acme_co = create_entity(&mut acme, 3, "Shared Co").await;
    let globex_co = create_entity(&mut globex, 3, "Shared Co").await;
    assert_ne!(
        acme_co, globex_co,
        "same name under distinct namespaces must mint distinct entities"
    );

    // (b) Each tenant resolves "Shared Co" to ITS OWN entity, never the other's.
    let acme_res = resolve_entity(&mut acme, 5, "Shared Co").await;
    assert_eq!(acme_res.outcome, ResolutionOutcomeWire::Resolved);
    assert_eq!(
        acme_res.resolved_entity, acme_co,
        "acme must resolve to its own entity"
    );
    let globex_res = resolve_entity(&mut globex, 5, "Shared Co").await;
    assert_eq!(globex_res.outcome, ResolutionOutcomeWire::Resolved);
    assert_eq!(
        globex_res.resolved_entity, globex_co,
        "globex must resolve to its own entity, never acme's"
    );
    assert_ne!(
        globex_res.resolved_entity, acme_co,
        "TENANCY BREACH: globex resolved across the namespace boundary to acme's entity"
    );

    // (c) A foreign-tenant entity id is unreadable across the boundary.
    assert!(
        !entity_get_visible(&mut globex, 7, acme_co).await,
        "TENANCY BREACH: globex read acme's entity via ENTITY_GET"
    );
    assert!(
        !entity_get_visible(&mut acme, 7, globex_co).await,
        "TENANCY BREACH: acme read globex's entity via ENTITY_GET"
    );

    server.stop().await;
}

/// The same tenant is reachable by a second key: an entity created under the
/// `acme` tenant by one key is resolvable and readable by a SECOND `acme` key
/// (same space). This proves the key → namespace → data path lands in one
/// stable per-shard tenant bucket, not a fresh fallback per connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_tenant_is_reachable_by_a_second_key() {
    let server = start(1).await;

    let space = [0xDDu8; 16];
    let key1 = server.mint("acme", space, brain_metadata::api_keys::bits::FULL);
    let key2 = server.mint("acme", space, brain_metadata::api_keys::bits::FULL);

    let mut conn1 = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect 1");
    let authok1 = handshake_authok(&mut conn1, &key1).await;
    assert_eq!(authok1.namespace, "acme");
    let entity = create_entity(&mut conn1, 1, "Acme Vault").await;

    // A fresh connection with a DIFFERENT key for the SAME (namespace, space).
    let mut conn2 = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect 2");
    let authok2 = handshake_authok(&mut conn2, &key2).await;
    assert_eq!(authok2.namespace, "acme");

    // Entity name resolution reads the trigram index, which lands just after
    // the (WAL-acked) ENTITY_CREATE commits. Under heavy parallel test load
    // that propagation window is observable, so poll with fresh odd stream ids
    // until the second key sees the first's entity — the eventual-consistency
    // pattern the other wire suites use. The tenancy boundary never wavers; we
    // are waiting on index propagation, not on isolation.
    let mut res = resolve_entity(&mut conn2, 1, "Acme Vault").await;
    let mut stream_id = 3u32;
    for _ in 0..50 {
        if res.outcome == ResolutionOutcomeWire::Resolved {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        res = resolve_entity(&mut conn2, stream_id, "Acme Vault").await;
        stream_id += 2;
    }
    assert_eq!(
        res.outcome,
        ResolutionOutcomeWire::Resolved,
        "second acme key must resolve an entity created by the first"
    );
    assert_eq!(
        res.resolved_entity, entity,
        "second acme key must reach the same tenant's entity {entity:?}"
    );
    assert!(
        entity_get_visible(&mut conn2, stream_id, entity).await,
        "second acme key must be able to ENTITY_GET the same tenant's entity"
    );

    server.stop().await;
}
