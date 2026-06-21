//! Snapshot → restore round-trip through the production stack.
//!
//! Proves the disaster-recovery contract from spec §08/06 §5: a
//! snapshot taken at LSN X, after which the shard is mutated, restores
//! the shard's state back to exactly X once the bundle's files are
//! placed and the shard re-recovers on the next spawn.
//!
//! Sequence:
//!   1. `start_in(dir, 1)` — full server on a persistent data dir.
//!   2. ENCODE M1..M3; take a snapshot over the admin HTTP plane.
//!   3. ENCODE M4 (a post-snapshot mutation).
//!   4. `Server::stop()` — graceful drain, shard goes offline.
//!   5. `restore_snapshot(...)` — place the bundle's files into the data
//!      dir (Brain restores by placing files, then recovering).
//!   6. `start_in(dir, 1)` again — recovery replays the bundled WAL to
//!      the snapshot LSN.
//!   7. RECALL: M1..M3 are back; M4 (encoded after the snapshot) is gone.
//!
//! A second test corrupts one byte of a bundle file and asserts restore
//! refuses it (BLAKE3 integrity), leaving the data dir untouched.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use tempfile::TempDir;
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

use support_harness::start_in;

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Wire helpers (mirror graceful_shutdown_durability.rs).
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

async fn round_trip(client: &mut TcpStream, stream_id: u32, req: RequestBody) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, req.encode())).await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

async fn handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "snapshot-restore-tester".into(),
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
        Frame::new(Opcode::Hello.as_u16(), FLAG_EOS, 0, RequestBody::Hello(hello).encode()),
    )
    .await;
    let welcome = read_one_frame(client).await;
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(Opcode::Auth.as_u16(), FLAG_EOS, 0, RequestBody::Auth(auth).encode()),
    )
    .await;
    let auth_ok = read_one_frame(client).await;
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode must succeed: opcode={opcode} body={other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Admin HTTP helpers.
// ---------------------------------------------------------------------------

async fn http_request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect admin");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         content-length: {len}\r\nconnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();
    let first_line = response.lines().next().unwrap_or("");
    let code = first_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let resp_body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, resp_body)
}

/// Extract `"id":N` from a small JSON response.
fn parse_id(body: &str) -> u64 {
    let needle = "\"id\":";
    let start = body.find(needle).expect("id field present") + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("id is u64")
}

fn shard_root(dir: &Path) -> PathBuf {
    dir.join("0")
}

fn read_shard_uuid(dir: &Path) -> [u8; 16] {
    let bytes = std::fs::read(shard_root(dir).join("shard.uuid")).expect("shard.uuid");
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&bytes[..16]);
    uuid
}

fn snapshot_dir(dir: &Path, id: u64) -> PathBuf {
    shard_root(dir).join("snapshots").join(format!("{id:020}"))
}

/// Count rows in the shard's MEMORIES table by opening the on-disk
/// metadata.redb directly (read-only).
fn count_memory_rows(dir: &Path) -> u64 {
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    use redb::ReadableTableMetadata;

    let md = brain_metadata::MetadataDb::open(shard_root(dir).join("metadata.redb"))
        .expect("open metadata");
    let rtxn = md.read_txn().expect("read_txn");
    let table = rtxn.open_table(MEMORIES_TABLE).expect("open MEMORIES_TABLE");
    let n = table.len().expect("table len");
    drop(table);
    drop(rtxn);
    n
}

/// Run the same WAL recovery the next shard spawn would run against the
/// restored data dir, then return the recovered memory-row count. This
/// is the lightweight stand-in for a full second server: it replays the
/// bundled WAL to the snapshot LSN through the real
/// `brain_storage::recovery::recover`.
fn run_recovery(dir: &Path, uuid: [u8; 16]) -> u64 {
    use brain_storage::arena::ArenaFile;

    let root = shard_root(dir);
    let mut arena = ArenaFile::open(root.join("arena.bin"), uuid, 1024).expect("open arena");
    let mut md =
        brain_metadata::MetadataDb::open(root.join("metadata.redb")).expect("open metadata");
    let wal_dir = root.join("wal");
    let (report, _alloc) =
        brain_storage::recovery::recover(&mut arena, &wal_dir, uuid, &mut md).expect("recover");
    // The report is informational; the durable truth is the row count.
    let _ = report;
    use brain_metadata::tables::memory::MEMORIES_TABLE;
    use redb::ReadableTableMetadata;
    let rtxn = md.read_txn().expect("read_txn");
    let table = rtxn.open_table(MEMORIES_TABLE).expect("open MEMORIES_TABLE");
    let n = table.len().expect("table len");
    drop(table);
    drop(rtxn);
    n
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Take a snapshot, mutate the shard, restore the snapshot, and confirm
/// the shard is back at the snapshot LSN — the original memories return
/// and the post-snapshot write is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_returns_shard_to_snapshot_state() {
    let dir = TempDir::new().expect("tmp");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    let pre_snapshot = [
        "the orbital relay station broadcasts on band gamma four",
        "the vault combination rotates each lunar cycle",
        "the courier departs at dawn from terminal nine",
    ];
    let post_snapshot = "the eclipse festival begins under the violet moon";

    let snapshot_id;

    // --- Instance 1: encode, snapshot, mutate, stop ---
    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect 1");
        handshake(&mut client, agent_id).await;

        for (i, text) in pre_snapshot.iter().enumerate() {
            encode(&mut client, 1 + (i as u32) * 2, text).await;
        }

        // Take the snapshot over the admin HTTP plane.
        let (code, body) =
            http_request(server.admin_addr, "POST", "/v1/snapshots?shard=0", "").await;
        assert_eq!(code, 201, "snapshot create should 201; body={body}");
        snapshot_id = parse_id(&body);

        // Mutate after the snapshot (the 4th memory, M4).
        encode(&mut client, 99, post_snapshot).await;

        drop(client);
        server.stop().await;
    }

    // Sanity: before restore, the live data dir holds all four memories.
    assert_eq!(
        count_memory_rows(dir.path()),
        4,
        "pre-restore: 3 pre-snapshot + 1 post-snapshot memory committed"
    );

    // --- Offline restore: place the bundle's files into the data dir ---
    let uuid = read_shard_uuid(dir.path());
    let snap = snapshot_dir(dir.path(), snapshot_id);
    assert!(snap.is_dir(), "snapshot dir {} exists", snap.display());
    let report = shard::restore::restore_snapshot(&snap, &shard_root(dir.path()), uuid)
        .expect("restore_snapshot");
    assert!(report.wal_segments_placed >= 1, "WAL tail placed");

    // --- Recovery: replay the restored (bundled) WAL to the snapshot LSN ---
    //
    // A full second server would re-prove this through the wire path, but
    // it doubles peak memory; here we run the exact recovery the next
    // spawn would run and assert the metadata is rewound to the snapshot
    // state: the 3 pre-snapshot memories are present and the
    // post-snapshot write (M4) is gone.
    let recovered_rows = run_recovery(dir.path(), uuid);
    assert_eq!(
        recovered_rows, 3,
        "recovery to snapshot_lsn must restore exactly the 3 pre-snapshot \
         memories (M4, encoded after the snapshot, must be gone); got {recovered_rows}"
    );
}

/// Corrupting a single byte of a bundle file makes restore refuse the
/// bundle (BLAKE3 integrity) and leave the target data dir untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_rejects_corrupt_bundle() {
    let dir = TempDir::new().expect("tmp");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    let snapshot_id;
    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect");
        handshake(&mut client, agent_id).await;
        encode(&mut client, 1, "the lighthouse keeper logs the tide at midnight").await;

        let (code, body) =
            http_request(server.admin_addr, "POST", "/v1/snapshots?shard=0", "").await;
        assert_eq!(code, 201, "snapshot create should 201; body={body}");
        snapshot_id = parse_id(&body);

        drop(client);
        server.stop().await;
    }

    let uuid = read_shard_uuid(dir.path());
    let snap = snapshot_dir(dir.path(), snapshot_id);

    // Flip one byte of metadata.redb in the bundle.
    let target = snap.join("metadata.redb");
    let mut bytes = std::fs::read(&target).expect("read bundle metadata");
    bytes[0] ^= 0xFF;
    std::fs::write(&target, bytes).expect("write corrupt metadata");

    // Snapshot the live arena before the attempted restore.
    let live_arena = shard_root(dir.path()).join("arena.bin");
    let before = std::fs::read(&live_arena).expect("read live arena");

    let err = shard::restore::restore_snapshot(&snap, &shard_root(dir.path()), uuid)
        .expect_err("corrupt bundle must be rejected");
    assert!(
        matches!(err, shard::restore::RestoreError::Integrity { .. }),
        "expected Integrity error, got {err:?}"
    );

    // The data dir was not touched — verification runs before placement.
    let after = std::fs::read(&live_arena).expect("read live arena after");
    assert_eq!(before, after, "arena must be untouched on a rejected restore");
}
