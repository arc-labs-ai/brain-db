//! Shared helpers used by every op's `send()` path.
//!
//! - [`FLAG_EOS`] — last-frame-of-stream flag (spec §03/03 §4).
//! - [`send_and_read_one`] — write one request frame, read one
//!   response frame. Maps `ERROR` opcode to [`ClientError::Server`].
//! - [`send_and_collect_until_eos`] — write request, collect
//!   response frames until `FLAG_EOS`. Used by streaming ops
//!   (RECALL/PLAN/REASON) in their pre-10.6 Vec form.
//! - [`map_error_frame`] — decode an ERROR payload into the SDK
//!   error.

use brain_protocol::error::ProtocolError;
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, ResponseBody};

use crate::error::ClientError;
use crate::pool::{Connection, PoolGuard};
use crate::proto::frames::{read_one_frame, write_frame};

/// Spec §03/03 §4 last-frame-of-stream flag.
pub const FLAG_EOS: u8 = 1 << 7;

/// Classify a [`ClientError`] as "the connection is dead, throw it
/// away" vs "the connection is fine, just the op failed."
///
/// `Io` / `Closed` / `Protocol` indicate the underlying socket is
/// unusable: a write to it produced EPIPE, a read EOF'd, or we
/// saw a frame we couldn't parse (the byte stream is desynced and
/// not recoverable). Returning these on the next op would just
/// repeat the failure, so the pool slot must be discarded.
///
/// `Server { .. }` is a legitimate wire-level error response from
/// the server (e.g. `MemoryNotFound`); the connection is healthy
/// and should stay in the pool.
fn is_connection_fatal(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Io(_) | ClientError::Closed | ClientError::Protocol(_)
    )
}

/// Send one request frame and read one response frame. The
/// caller decodes the body. Maps `ERROR` opcode automatically.
///
/// Takes [`PoolGuard`] rather than `&mut Connection` so a fatal
/// I/O or protocol error can mark the slot for discard — without
/// this, a broken connection is recycled into the Idle pool and
/// every subsequent op on the same `Client` will repeat the
/// failure until retry-exhaustion (see plan-mode write-up "Bug B"
/// in `/Users/dodo/.claude/plans/i-want-proper-doc-indexed-brook.md`).
pub async fn send_and_read_one(
    guard: &mut PoolGuard,
    request: Frame,
    expected: Opcode,
) -> Result<Frame, ClientError> {
    let r = send_and_read_one_inner(guard, request, expected).await;
    if let Err(ref e) = r {
        if is_connection_fatal(e) {
            guard.mark_failed();
        }
    }
    r
}

async fn send_and_read_one_inner(
    conn: &mut Connection,
    request: Frame,
    expected: Opcode,
) -> Result<Frame, ClientError> {
    write_frame(conn.stream_mut(), &request).await?;
    let resp = read_one_frame(conn.stream_mut()).await?;
    if resp.header.opcode_u16() == Opcode::Error.as_u16() {
        return Err(map_error_frame(&resp.payload));
    }
    if resp.header.opcode_u16() != expected.as_u16() {
        return Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
            "expected opcode 0x{:02x}, got 0x{:02x}",
            expected.as_u16(),
            resp.header.opcode_u16()
        ))));
    }
    Ok(resp)
}

/// Send one request frame and collect response frames until one
/// carries the `FLAG_EOS` bit. Returns the ordered Vec. Used by
/// the pre-10.6 streaming-as-Vec form of RECALL / PLAN / REASON /
/// SUBSCRIBE.
///
/// `max_frames` bounds the collection so a misbehaving server
/// can't bloat client memory.
///
/// Same `PoolGuard` discipline as [`send_and_read_one`] — fatal
/// I/O or protocol errors mark the slot for discard.
pub async fn send_and_collect_until_eos(
    guard: &mut PoolGuard,
    request: Frame,
    expected: Opcode,
    max_frames: usize,
) -> Result<Vec<Frame>, ClientError> {
    let r = send_and_collect_until_eos_inner(guard, request, expected, max_frames).await;
    if let Err(ref e) = r {
        if is_connection_fatal(e) {
            guard.mark_failed();
        }
    }
    r
}

async fn send_and_collect_until_eos_inner(
    conn: &mut Connection,
    request: Frame,
    expected: Opcode,
    max_frames: usize,
) -> Result<Vec<Frame>, ClientError> {
    write_frame(conn.stream_mut(), &request).await?;
    let mut frames = Vec::with_capacity(8);
    loop {
        let resp = read_one_frame(conn.stream_mut()).await?;
        if resp.header.opcode_u16() == Opcode::Error.as_u16() {
            return Err(map_error_frame(&resp.payload));
        }
        if resp.header.opcode_u16() != expected.as_u16() {
            return Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
                "expected opcode 0x{:02x}, got 0x{:02x}",
                expected.as_u16(),
                resp.header.opcode_u16()
            ))));
        }
        let is_final = resp.header.flags_u8() & FLAG_EOS != 0;
        frames.push(resp);
        if is_final {
            break;
        }
        if frames.len() >= max_frames {
            return Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
                "stream exceeded {max_frames} frames without EOS"
            ))));
        }
    }
    Ok(frames)
}

/// Decode an `ERROR` body into a [`ClientError::Server`]. Falls
/// back to a generic protocol error if the body is malformed.
pub fn map_error_frame(payload: &[u8]) -> ClientError {
    match ResponseBody::decode(Opcode::Error, payload) {
        Ok(ResponseBody::Error(e)) => ClientError::Server {
            code: e.code as u16,
            message: e.message,
        },
        Ok(_) => ClientError::Protocol(ProtocolError::BadFrame(
            "ERROR opcode but body variant didn't match".into(),
        )),
        Err(pe) => ClientError::Protocol(pe),
    }
}

/// Default cap on streamed frames per op. Spec §06/05 default
/// `max_concurrent_streams` is 1024; we cap collection well
/// below that to keep a single op bounded.
pub const DEFAULT_STREAM_FRAME_CAP: usize = 512;
