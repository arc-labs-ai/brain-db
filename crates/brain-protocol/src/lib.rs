//! # brain-protocol
//!
//! Brain's wire protocol: a custom binary protocol over TCP (with optional
//! TLS). Frames have a fixed 32-byte header, a magic of `b"BRN0"`, header
//! and payload CRC32C, and a 24-bit payload length cap (16 MiB).
//!
//! The crate is laid out by **role**, not by wire direction:
//!
//! - [`codec`] — bytes-on-the-wire plumbing (header, frame, CRC, CBOR,
//!   opcode). Nothing in this module knows what an operation means.
//! - [`envelope`] — the [`RequestBody`] / [`ResponseBody`] dispatch
//!   enums, the [`ErrorResponse`] payload, and core ↔ wire conversions.
//! - [`connection`] — connection-lifecycle ops: handshake and stream
//!   control (PING / PONG / CANCEL_STREAM / BYE / SERVER_PING).
//! - [`ops`] — per-domain wire ops. One file per capability (memory,
//!   entity, statement, relation, query, procedural, txn, subscribe,
//!   admin, extractor) holding request, response, and view types
//!   together.
//! - [`schema`] — schema DSL surface (AST, parser, validator) plus the
//!   schema-management wire ops in [`schema::ops`].
//! - [`shared`] — wire primitives and enums shared across multiple op
//!   families.
//! - [`error`] — the [`ErrorCategory`] / [`ErrorCode`] / [`ProtocolError`]
//!   taxonomy.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod codec;
pub mod connection;
pub mod envelope;
pub mod error;
pub mod ops;
pub mod schema;
pub mod shared;

// -- Codec layer --
pub use codec::frame::Frame;
pub use codec::header::{Header, VERSION};
pub use codec::opcode::Opcode;

// -- Envelope layer --
pub use envelope::error::{ErrorDetails, ErrorResponse};
pub use envelope::request::{RequestBody, WireContextId, WireMemoryId, WireUuid};
pub use envelope::response::ResponseBody;

// -- Error taxonomy --
pub use error::{ErrorCategory, ErrorCode, ProtocolError};

// -- Connection layer --
pub use connection::handshake::{
    negotiate, AgentPermissions, AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload,
    HelloCapabilities, HelloPayload, MtlsClaim, NegotiatedSession, ServerCapabilities,
    ServerFeatures, WelcomePayload,
};
pub use connection::stream::{
    ByeRequest, CancelStreamAck, CancelStreamRequest, ClientPongRequest, PingRequest, PongResponse,
    ServerPingResponse,
};

// -- Shared wire primitives + enums --
pub use shared::enums::*;
pub use shared::primitives::*;

// -- Per-domain ops (flat re-exports preserve every existing
//    `brain_protocol::X` import path) --
pub use ops::admin::*;
pub use ops::entity::*;
pub use ops::extractor::*;
pub use ops::memory::*;
pub use ops::procedural::*;
pub use ops::query::*;
pub use ops::relation::*;
pub use ops::statement::*;
pub use ops::subscribe::*;
pub use ops::txn::*;

// -- Schema (DSL + wire ops) --
pub use schema::ops::*;

/// Frame magic bytes. Identifies a Brain frame on the wire.
pub const MAGIC: [u8; 4] = *b"BRN0";

/// Fixed frame header size in bytes.
pub const HEADER_SIZE: usize = 32;

/// Maximum payload size (16 MiB - 1), enforced by the 24-bit length field.
pub const MAX_PAYLOAD_BYTES: usize = (1 << 24) - 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_brn0() {
        assert_eq!(&MAGIC, b"BRN0");
    }

    #[test]
    fn header_size_is_32() {
        assert_eq!(HEADER_SIZE, 32);
    }

    #[test]
    fn max_payload_fits_in_24_bits() {
        assert_eq!(MAX_PAYLOAD_BYTES, 16 * 1024 * 1024 - 1);
    }
}
