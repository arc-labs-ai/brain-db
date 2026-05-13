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
use crate::pool::Connection;
use crate::proto::frames::{read_one_frame, write_frame};

/// Spec §03/03 §4 last-frame-of-stream flag.
pub const FLAG_EOS: u16 = 1 << 15;

/// Send one request frame and read one response frame. The
/// caller decodes the body. Maps `ERROR` opcode automatically.
pub async fn send_and_read_one(
    conn: &mut Connection,
    request: Frame,
    expected: Opcode,
) -> Result<Frame, ClientError> {
    write_frame(conn.stream_mut(), &request).await?;
    let resp = read_one_frame(conn.stream_mut()).await?;
    if resp.header.opcode == Opcode::Error.as_u8() {
        return Err(map_error_frame(&resp.payload));
    }
    if resp.header.opcode != expected.as_u8() {
        return Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
            "expected opcode 0x{:02x}, got 0x{:02x}",
            expected.as_u8(),
            resp.header.opcode
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
pub async fn send_and_collect_until_eos(
    conn: &mut Connection,
    request: Frame,
    expected: Opcode,
    max_frames: usize,
) -> Result<Vec<Frame>, ClientError> {
    write_frame(conn.stream_mut(), &request).await?;
    let mut frames = Vec::with_capacity(8);
    loop {
        let resp = read_one_frame(conn.stream_mut()).await?;
        if resp.header.opcode == Opcode::Error.as_u8() {
            return Err(map_error_frame(&resp.payload));
        }
        if resp.header.opcode != expected.as_u8() {
            return Err(ClientError::Protocol(ProtocolError::BadFrame(format!(
                "expected opcode 0x{:02x}, got 0x{:02x}",
                expected.as_u8(),
                resp.header.opcode
            ))));
        }
        let flags = u16::from_be_bytes(resp.header.flags);
        let is_final = flags & FLAG_EOS != 0;
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
