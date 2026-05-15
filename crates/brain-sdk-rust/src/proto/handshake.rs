//! Client-side handshake FSM: HELLO → WELCOME → AUTH → AUTH_OK.
//!
//! Spec §03/06 + §13/03 §3. Returns the [`WelcomePayload`] and
//! [`AuthOkPayload`] the server sent so `Client` can stash the
//! negotiated capabilities + bound shard.

use brain_core::AgentId;
use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities, HelloPayload,
    WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::ClientError;

use super::frames::{read_one_frame, write_frame};

/// Spec §03/03 §4 — last-frame-of-stream flag. The handshake
/// frames all carry `flags = FLAG_EOS` per spec §03/06 §2.
const FLAG_EOS: u8 = 1 << 7;
/// Spec §03/06 §2: handshake frames travel on the control stream
/// (stream id 0).
const HANDSHAKE_STREAM: u32 = 0;

/// Identification the client sends in HELLO.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// Free-form client id (≤ 256 bytes per spec §03/06 §2.1).
    pub client_id: String,
    /// Wire-protocol versions the client speaks.
    pub supported_versions: Vec<u8>,
    /// Feature capabilities advertised in HELLO.
    pub capabilities: HelloCapabilities,
}

impl ClientIdentity {
    /// v1 default — streaming on, no compression/push, client_id
    /// stamped from the caller.
    #[must_use]
    pub fn v1(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            supported_versions: vec![brain_protocol::header::VERSION],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
        }
    }
}

/// Outcome of a successful handshake. The client retains this so
/// later op methods (10.5+) can check `bound_shard_id` and the
/// negotiated capabilities.
#[derive(Clone, Debug)]
pub struct NegotiatedSession {
    pub welcome: WelcomePayload,
    pub auth_ok: AuthOkPayload,
}

/// Drive the four-frame handshake to completion. Returns the
/// server's WELCOME and AUTH_OK on success; an appropriately-
/// tagged [`ClientError`] otherwise.
///
/// Mirrors the test scaffold in `brain-server/tests/e2e.rs::complete_handshake`
/// but with structured error returns instead of panics.
pub async fn complete_handshake<S>(
    stream: &mut S,
    identity: ClientIdentity,
    agent_id: AgentId,
    auth: AuthMethod,
) -> Result<NegotiatedSession, ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // ---- HELLO ---------------------------------------------------
    let hello = HelloPayload {
        client_id: identity.client_id,
        supported_versions: identity.supported_versions,
        capabilities: identity.capabilities,
        client_session_token: None,
    };
    let frame = Frame::new(
        Opcode::Hello.as_u16(),
        FLAG_EOS,
        HANDSHAKE_STREAM,
        RequestBody::Hello(hello).encode(),
    );
    write_frame(stream, &frame).await?;

    // ---- WELCOME -------------------------------------------------
    let welcome_frame = read_one_frame(stream).await?;
    if welcome_frame.header.opcode_u16() != Opcode::Welcome.as_u16() {
        return Err(ClientError::Handshake(format!(
            "expected WELCOME (0x{:02x}), got opcode 0x{:02x}",
            Opcode::Welcome.as_u16(),
            welcome_frame.header.opcode_u16()
        )));
    }
    let welcome_body = ResponseBody::decode(Opcode::Welcome, &welcome_frame.payload)?;
    let welcome = match welcome_body {
        ResponseBody::Welcome(w) => w,
        other => {
            return Err(ClientError::Handshake(format!(
                "WELCOME opcode but body was {:?}",
                std::mem::discriminant(&other)
            )));
        }
    };

    // ---- AUTH ----------------------------------------------------
    let agent_bytes: [u8; 16] = *agent_id.0.as_bytes();
    let credentials = match auth {
        AuthMethod::None => AuthCredentials::None,
        AuthMethod::Token => AuthCredentials::Token(Vec::new()),
        AuthMethod::Mtls => {
            return Err(ClientError::Auth(
                "mTLS auth requires a TLS-wrapped stream; not supported in 10.1".into(),
            ));
        }
    };
    let auth_payload = AuthPayload {
        method: auth,
        agent_id: agent_bytes,
        credentials,
    };
    let frame = Frame::new(
        Opcode::Auth.as_u16(),
        FLAG_EOS,
        HANDSHAKE_STREAM,
        RequestBody::Auth(auth_payload).encode(),
    );
    write_frame(stream, &frame).await?;

    // ---- AUTH_OK / ERROR -----------------------------------------
    let auth_ok_frame = read_one_frame(stream).await?;
    if auth_ok_frame.header.opcode_u16() == Opcode::Error.as_u16() {
        // Best-effort decode of the ERROR payload for the message.
        let err_body = ResponseBody::decode(Opcode::Error, &auth_ok_frame.payload);
        let msg = match err_body {
            Ok(ResponseBody::Error(e)) => e.message,
            _ => "server returned ERROR during AUTH (payload undecodable)".into(),
        };
        return Err(ClientError::Auth(msg));
    }
    if auth_ok_frame.header.opcode_u16() != Opcode::AuthOk.as_u16() {
        return Err(ClientError::Handshake(format!(
            "expected AUTH_OK (0x{:02x}), got opcode 0x{:02x}",
            Opcode::AuthOk.as_u16(),
            auth_ok_frame.header.opcode_u16()
        )));
    }
    let auth_ok_body = ResponseBody::decode(Opcode::AuthOk, &auth_ok_frame.payload)?;
    let auth_ok = match auth_ok_body {
        ResponseBody::AuthOk(a) => a,
        other => {
            return Err(ClientError::Handshake(format!(
                "AUTH_OK opcode but body was {:?}",
                std::mem::discriminant(&other)
            )));
        }
    };

    Ok(NegotiatedSession { welcome, auth_ok })
}
