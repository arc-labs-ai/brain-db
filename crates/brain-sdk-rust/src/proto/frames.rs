//! Frame I/O over a Tokio `TcpStream`.
//!
//! `read_one_frame` / `write_frame` are the SDK's only direct
//! contact with the socket. The handshake FSM ([`super::handshake`])
//! and the future op methods (10.5+) build on top of these.

use brain_protocol::{Frame, HEADER_SIZE, MAX_PAYLOAD_BYTES};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::ClientError;

/// Read exactly one frame off the stream. Returns
/// [`ClientError::Closed`] on EOF mid-header (peer dropped the
/// connection cleanly), [`ClientError::Io`] on socket errors,
/// and [`ClientError::Protocol`] on frame-decode failures.
pub async fn read_one_frame<R>(stream: &mut R) -> Result<Frame, ClientError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; HEADER_SIZE];
    // Distinguish "clean EOF before any bytes" from "truncated mid-header".
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ClientError::Closed);
        }
        Err(e) => return Err(ClientError::Io(e)),
    }
    let payload_len_be = [header[16], header[17], header[18]];
    let payload_len =
        u32::from_be_bytes([0, payload_len_be[0], payload_len_be[1], payload_len_be[2]]) as usize;

    let mut buf = Vec::with_capacity(HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(HEADER_SIZE + payload_len, 0);
        stream.read_exact(&mut buf[HEADER_SIZE..]).await?;
    }
    let (frame, rest) = Frame::decode_with_max(&buf, MAX_PAYLOAD_BYTES as u32)?;
    debug_assert!(rest.is_empty(), "frame decoded with leftover bytes");
    Ok(frame)
}

/// Write a frame to the stream and flush. Returns
/// [`ClientError::Io`] on socket errors.
pub async fn write_frame<W>(stream: &mut W, frame: &Frame) -> Result<(), ClientError>
where
    W: AsyncWrite + Unpin,
{
    stream.write_all(&frame.encode()).await?;
    stream.flush().await?;
    Ok(())
}
