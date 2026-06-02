//! Handshake codec — `HELLO`, `WELCOME`, `AUTH`, `AUTH_OK`.
//!
//! All four payloads encode as CBOR like the rest of `brain-protocol`. The
//! four message structs are surfaced through `RequestBody` (HELLO, AUTH —
//! server-bound) and `ResponseBody` (WELCOME, AUTH_OK — client-bound) so
//! Frame dispatch is uniform.
//!
//! [`negotiate`] picks the highest mutually-supported wire-protocol
//! version and intersects the [`HelloCapabilities`] flags. Auth-method
//! intersection is *not* part of negotiation — that check happens when
//! the server validates the AUTH frame against the methods it advertised
//! in WELCOME, owned by the connection-layer AUTH handler.

use crate::codec::cbor::{from_cbor_bytes, to_cbor_bytes};
use crate::codec::header::VERSION;
use crate::envelope::request::WireUuid;
use crate::error::ProtocolError;

// ---------------------------------------------------------------------------
// Shared helper types.
// ---------------------------------------------------------------------------

/// — feature flags exchanged during handshake. The same
/// shape appears in HELLO (client-supported) and WELCOME (mutually-
/// supported after intersection).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HelloCapabilities {
    /// Streaming support. Always `true` in v1.
    pub streaming: bool,
    /// zstd payload compression. Reserved; not used in v1.
    pub compression_zstd: bool,
    /// Server-pushed events. Reserved; not used in v1.
    pub server_push: bool,
}

/// — server-declared parameters carried in WELCOME.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServerFeatures {
    /// Server's max accepted payload (spec default 16 MiB).
    pub max_payload_size: u32,
    /// Per-connection concurrent stream limit (spec default 1024).
    pub max_concurrent_streams: u32,
    /// Idle window before the server emits `SERVER_PING` (spec default 300 s).
    pub idle_timeout_seconds: u32,
    /// Auth methods the server accepts. Client picks one from this list
    /// for the subsequent `AUTH` frame.
    pub auth_methods: Vec<AuthMethod>,
}

/// — supported authentication method.
///
/// Numeric repr is stable wire-side: `Token = 0`, `Mtls = 1`, `None = 2`.
/// Adding a new method requires a wire-version bump.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum AuthMethod {
    /// Bearer token (opaque to the protocol; backend-validated).
    Token = 0,
    /// Mutual-TLS — the cert was presented during the TLS handshake.
    Mtls = 1,
    /// No credentials — test/dev only; trusted-network deployments.
    None = 2,
}

/// — credentials carried in the AUTH frame.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AuthCredentials {
    /// Opaque bearer token bytes.
    Token(Vec<u8>),
    /// mTLS-presented certificate claim.
    Mtls(MtlsClaim),
    /// No credentials.
    None,
}

/// — mTLS claim accompanying an mTLS auth.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MtlsClaim {
    /// SHA-256 of the client's certificate.
    #[serde(with = "serde_bytes")]
    pub cert_fingerprint: [u8; 32],
    /// Subject the client claims (typically Subject Alternative Name or CN).
    pub asserted_subject: String,
}

/// — the agent's permitted operations after AUTH_OK.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentPermissions {
    pub can_encode: bool,
    pub can_recall: bool,
    pub can_plan: bool,
    pub can_reason: bool,
    pub can_forget: bool,
    /// Typically `false` for normal agents; required for any `ADMIN_*` op.
    pub can_admin: bool,
}

// ---------------------------------------------------------------------------
// HELLO (0x01) — client → server.
// ---------------------------------------------------------------------------

/// — first frame after TCP/TLS establishment.
///
/// `client_id` and `supported_versions` are the negotiation inputs; the
/// server intersects against its own capabilities and replies with
/// `WelcomePayload`. `client_session_token` is reserved for future
/// session-resumption (not used in v1).
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HelloPayload {
    /// Free-form client identifier (≤ 256 bytes).
    pub client_id: String,
    /// Wire-protocol versions the client can speak.
    pub supported_versions: Vec<u8>,
    pub capabilities: HelloCapabilities,
    /// Reserved for v2 session-resumption.
    #[serde(with = "serde_bytes")]
    pub client_session_token: Option<[u8; 32]>,
}

// ---------------------------------------------------------------------------
// WELCOME (0x81) — server → client.
// ---------------------------------------------------------------------------

/// — server's response to `HELLO`. The connection is bound
/// to `chosen_version` and `session_id` once this frame is received.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WelcomePayload {
    /// Free-form server identifier (≤ 256 bytes).
    pub server_id: String,
    /// Negotiated wire-protocol version. Highest mutual; fail-closed
    /// otherwise.
    pub chosen_version: u8,
    /// 16 cryptographically-random bytes; per-connection identifier
    #[serde(with = "serde_bytes")]
    pub session_id: [u8; 16],
    /// Mutually-supported feature flags (intersection of client and
    /// server `HelloCapabilities`).
    pub capabilities: HelloCapabilities,
    pub server_features: ServerFeatures,
}

// ---------------------------------------------------------------------------
// AUTH (0x02) — client → server.
// ---------------------------------------------------------------------------

/// — credentials for the agent claiming identity.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthPayload {
    /// Auth method. MUST be one of the methods declared in
    /// `WelcomePayload.server_features.auth_methods` (validated at the
    /// AUTH-frame handler in the connection layer, not by [`negotiate`]).
    pub method: AuthMethod,
    /// The agent the client is identifying as.
    #[serde(with = "serde_bytes")]
    pub agent_id: WireUuid,
    pub credentials: AuthCredentials,
}

// ---------------------------------------------------------------------------
// AUTH_OK (0x82) — server → client.
// ---------------------------------------------------------------------------

/// — server's acknowledgment of successful authentication.
/// After this frame, the connection is in the "established" state and
/// operations can flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthOkPayload {
    /// Confirmed agent_id (echoed from AUTH).
    #[serde(with = "serde_bytes")]
    pub agent_id: WireUuid,
    /// Runtime shard ID this agent is bound to.
    pub bound_shard_id: u16,
    pub permissions: AgentPermissions,
    /// Server's current time, for the client to detect clock skew
    pub server_time_unix_nanos: u64,
}

// ---------------------------------------------------------------------------
// Negotiation.
// ---------------------------------------------------------------------------

/// The server's local view of what it supports — input to [`negotiate`].
///
/// Held by the connection-layer handler when a `HELLO` arrives; combined
/// with the inbound `HelloPayload` to produce a [`NegotiatedSession`] (or
/// `BadVersion` if no version overlaps).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerCapabilities {
    /// Wire-protocol versions the server supports.
    pub supported_versions: Vec<u8>,
    /// Server-side feature flags. The intersection with the client's
    /// `HelloCapabilities` ends up in `WelcomePayload.capabilities`.
    pub capabilities: HelloCapabilities,
    pub server_features: ServerFeatures,
    pub server_id: String,
}

impl ServerCapabilities {
    /// A reasonable default: supports only the current wire `VERSION`,
    /// streaming only, no compression / push. 16 MiB max payload, 1024
    /// concurrent streams, 5 min idle timeout. Convenient for tests.
    #[must_use]
    pub fn v1_default(server_id: impl Into<String>, auth_methods: Vec<AuthMethod>) -> Self {
        Self {
            supported_versions: vec![VERSION],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: false,
                server_push: false,
            },
            server_features: ServerFeatures {
                max_payload_size: crate::MAX_PAYLOAD_BYTES as u32,
                max_concurrent_streams: 1024,
                idle_timeout_seconds: 300,
                auth_methods,
            },
            server_id: server_id.into(),
        }
    }
}

/// Result of a successful version + capability handshake. The server
/// uses this to populate `WelcomePayload`; the connection layer uses it
/// to bind the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedSession {
    /// Highest mutually-supported wire-protocol version.
    pub chosen_version: u8,
    /// AND-intersection of client and server [`HelloCapabilities`].
    pub capabilities: HelloCapabilities,
}

/// Pick the highest mutually-supported wire-protocol version and
/// intersect the capability flags.
///
/// Returns [`ProtocolError::BadVersion`] if no version overlaps. The
/// over-the-wire failure path emits an `ERROR` frame with code
/// `VersionNotSupported`; mapping `BadVersion` → that code is the
/// connection layer's responsibility.
///
/// Auth-method intersection is **not** performed here — that's checked
/// when the AUTH frame arrives, not at handshake-negotiation time.
pub fn negotiate(
    client: &HelloPayload,
    server: &ServerCapabilities,
) -> Result<NegotiatedSession, ProtocolError> {
    let chosen_version = client
        .supported_versions
        .iter()
        .filter(|v| server.supported_versions.contains(v))
        .copied()
        .max()
        .ok_or_else(|| {
            // Surface the highest version each side claimed so the
            // resulting ERROR frame can give an informative message.
            let server_max = server.supported_versions.iter().copied().max().unwrap_or(0);
            let client_max = client.supported_versions.iter().copied().max().unwrap_or(0);
            ProtocolError::BadVersion {
                got: client_max,
                expected: server_max,
            }
        })?;

    let capabilities = HelloCapabilities {
        // Streaming is always true in v1; the AND below
        // produces `true` whenever both sides set it, which they always
        // should. If a client somehow sends `streaming=false`, the
        // intersection still falls back to false and the server can
        // reject at a higher layer.
        streaming: client.capabilities.streaming && server.capabilities.streaming,
        compression_zstd: client.capabilities.compression_zstd
            && server.capabilities.compression_zstd,
        server_push: client.capabilities.server_push && server.capabilities.server_push,
    };

    Ok(NegotiatedSession {
        chosen_version,
        capabilities,
    })
}

// ---------------------------------------------------------------------------
// Public encode / decode helpers.
// ---------------------------------------------------------------------------

impl HelloPayload {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        to_cbor_bytes(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        from_cbor_bytes::<Self>(bytes)
    }
}

impl WelcomePayload {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        to_cbor_bytes(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        from_cbor_bytes::<Self>(bytes)
    }
}

impl AuthPayload {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        to_cbor_bytes(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        from_cbor_bytes::<Self>(bytes)
    }
}

impl AuthOkPayload {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        to_cbor_bytes(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        from_cbor_bytes::<Self>(bytes)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn sample_session_id(seed: u8) -> [u8; 16] {
        sample_uuid(seed)
    }

    fn sample_token(seed: u8) -> [u8; 32] {
        let mut t = [0u8; 32];
        for (i, b) in t.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        t
    }

    fn full_caps() -> HelloCapabilities {
        HelloCapabilities {
            streaming: true,
            compression_zstd: true,
            server_push: true,
        }
    }

    fn v1_caps() -> HelloCapabilities {
        HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        }
    }

    // ---- HELLO -------------------------------------------------------------

    #[test]
    fn hello_payload_round_trips() {
        let original = HelloPayload {
            client_id: "example-client/1.0".into(),
            supported_versions: vec![1, 2],
            capabilities: full_caps(),
            client_session_token: Some(sample_token(1)),
        };
        let bytes = original.encode();
        let decoded = HelloPayload::decode(&bytes).expect("hello round-trip");
        assert_eq!(decoded, original);
    }

    #[test]
    fn hello_payload_round_trips_without_session_token() {
        let original = HelloPayload {
            client_id: "client".into(),
            supported_versions: vec![crate::VERSION],
            capabilities: v1_caps(),
            client_session_token: None,
        };
        assert_eq!(HelloPayload::decode(&original.encode()).unwrap(), original);
    }

    // ---- WELCOME -----------------------------------------------------------

    #[test]
    fn welcome_payload_round_trips() {
        let original = WelcomePayload {
            server_id: "brain-server/0.5.0".into(),
            chosen_version: 1,
            session_id: sample_session_id(2),
            capabilities: v1_caps(),
            server_features: ServerFeatures {
                max_payload_size: crate::MAX_PAYLOAD_BYTES as u32,
                max_concurrent_streams: 1024,
                idle_timeout_seconds: 300,
                auth_methods: vec![AuthMethod::Token, AuthMethod::Mtls],
            },
        };
        let bytes = original.encode();
        let decoded = WelcomePayload::decode(&bytes).expect("welcome round-trip");
        assert_eq!(decoded, original);
    }

    // ---- AUTH (each method) ------------------------------------------------

    #[test]
    fn auth_payload_round_trips_token() {
        let original = AuthPayload {
            method: AuthMethod::Token,
            agent_id: sample_uuid(3),
            credentials: AuthCredentials::Token(b"opaque-token-bytes".to_vec()),
        };
        assert_eq!(AuthPayload::decode(&original.encode()).unwrap(), original);
    }

    #[test]
    fn auth_payload_round_trips_mtls() {
        let original = AuthPayload {
            method: AuthMethod::Mtls,
            agent_id: sample_uuid(4),
            credentials: AuthCredentials::Mtls(MtlsClaim {
                cert_fingerprint: sample_token(5),
                asserted_subject: "CN=client.example.com".into(),
            }),
        };
        assert_eq!(AuthPayload::decode(&original.encode()).unwrap(), original);
    }

    #[test]
    fn auth_payload_round_trips_none() {
        let original = AuthPayload {
            method: AuthMethod::None,
            agent_id: sample_uuid(6),
            credentials: AuthCredentials::None,
        };
        assert_eq!(AuthPayload::decode(&original.encode()).unwrap(), original);
    }

    // ---- AUTH_OK -----------------------------------------------------------

    #[test]
    fn auth_ok_payload_round_trips() {
        let original = AuthOkPayload {
            agent_id: sample_uuid(7),
            bound_shard_id: 3,
            permissions: AgentPermissions {
                can_encode: true,
                can_recall: true,
                can_plan: true,
                can_reason: true,
                can_forget: true,
                can_admin: false,
            },
            server_time_unix_nanos: 1_700_000_000_000_000_000,
        };
        assert_eq!(AuthOkPayload::decode(&original.encode()).unwrap(), original);
    }

    // ---- Negotiation: version intersection ---------------------------------

    #[test]
    fn negotiate_picks_highest_mutual_version() {
        let client = HelloPayload {
            client_id: "c".into(),
            supported_versions: vec![1, 2, 3],
            capabilities: v1_caps(),
            client_session_token: None,
        };
        let mut server = ServerCapabilities::v1_default("s", vec![AuthMethod::None]);
        server.supported_versions = vec![1, 2];
        let session = negotiate(&client, &server).expect("overlap on 1, 2");
        assert_eq!(session.chosen_version, 2);
    }

    #[test]
    fn negotiate_one_overlap() {
        let client = HelloPayload {
            client_id: "c".into(),
            supported_versions: vec![crate::VERSION],
            capabilities: v1_caps(),
            client_session_token: None,
        };
        let mut server = ServerCapabilities::v1_default("s", vec![AuthMethod::None]);
        server.supported_versions = vec![1, 2];
        // Client only speaks the current wire version; server speaks
        // both 1 and the current. Negotiation must pick the current,
        // not the older. Pinning the assert to `crate::VERSION` keeps
        // the test honest across future bumps.
        let session = negotiate(&client, &server).expect("overlap on current version");
        assert_eq!(session.chosen_version, crate::VERSION);
    }

    #[test]
    fn negotiate_no_overlap_fails() {
        let client = HelloPayload {
            client_id: "c".into(),
            supported_versions: vec![3, 4],
            capabilities: v1_caps(),
            client_session_token: None,
        };
        let mut server = ServerCapabilities::v1_default("s", vec![AuthMethod::None]);
        server.supported_versions = vec![1, 2];
        let err = negotiate(&client, &server).expect_err("no overlap");
        assert!(matches!(
            err,
            ProtocolError::BadVersion {
                got: 4,
                expected: 2
            }
        ));
    }

    // ---- Negotiation: capabilities intersection ----------------------------

    #[test]
    fn negotiate_intersects_capabilities() {
        let client = HelloPayload {
            client_id: "c".into(),
            supported_versions: vec![VERSION],
            capabilities: HelloCapabilities {
                streaming: true,
                compression_zstd: true, // client supports
                server_push: true,
            },
            client_session_token: None,
        };
        let mut server = ServerCapabilities::v1_default("s", vec![AuthMethod::None]);
        server.capabilities = HelloCapabilities {
            streaming: true,
            compression_zstd: false, // server doesn't
            server_push: true,
        };
        let session = negotiate(&client, &server).expect("overlap");
        assert!(session.capabilities.streaming);
        assert!(
            !session.capabilities.compression_zstd,
            "AND-intersect should drop"
        );
        assert!(session.capabilities.server_push);
    }

    #[test]
    fn negotiate_streaming_always_true_in_v1() {
        let client = HelloPayload {
            client_id: "c".into(),
            supported_versions: vec![VERSION],
            capabilities: v1_caps(),
            client_session_token: None,
        };
        let server = ServerCapabilities::v1_default("s", vec![AuthMethod::None]);
        let session = negotiate(&client, &server).expect("happy path");
        assert!(session.capabilities.streaming);
    }
}
