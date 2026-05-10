//! Wire-protocol opcodes per spec §03/05.
//!
//! Numeric values are stable. Server-bound (C→S) opcodes occupy
//! `0x00..=0x7F`; client-bound (S→C) opcodes occupy `0x80..=0xFF`.
//!
//! Reserved ranges (must NOT be assigned without a protocol-version bump,
//! per spec §03/05 §2):
//! - `0x70..=0x7F` — server-bound
//! - `0xF0..=0xFE` — client-bound
//!
//! `BYE` (0x1F) and `ERROR` (0xFF) are documented in the spec as
//! "bidirectional"; their numeric category still places them in a
//! single direction (server-bound and client-bound respectively), and
//! [`Opcode::is_request`] / [`Opcode::is_response`] follow the numeric
//! split so the predicates match the dispatch rule in spec §03/05 §4.

use crate::error::ProtocolError;

/// Wire-protocol opcode. See spec §03/05 §1 for the complete table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum Opcode {
    // §1.1 Connection management
    Hello = 0x01,
    Welcome = 0x81,
    Auth = 0x02,
    AuthOk = 0x82,
    Ping = 0x10,
    Pong = 0x90,
    ServerPing = 0x91,
    ClientPong = 0x11,
    Bye = 0x1F,

    // §1.2 Cognitive operations
    EncodeReq = 0x20,
    EncodeResp = 0xA0,
    RecallReq = 0x21,
    RecallResp = 0xA1,
    PlanReq = 0x22,
    PlanResp = 0xA2,
    ReasonReq = 0x23,
    ReasonResp = 0xA3,
    ForgetReq = 0x24,
    ForgetResp = 0xA4,
    EncodeVectorDirectReq = 0x2A,
    EncodeVectorDirectResp = 0xAA,

    // §1.3 Subscription
    SubscribeReq = 0x30,
    SubscribeEvent = 0xB0,
    UnsubscribeReq = 0x31,
    UnsubscribeResp = 0xB1,

    // §1.4 Transactions
    TxnBegin = 0x40,
    TxnBeginResp = 0xC0,
    TxnCommit = 0x41,
    TxnCommitResp = 0xC1,
    TxnAbort = 0x42,
    TxnAbortResp = 0xC2,

    // §1.5 Stream control
    CancelStream = 0x50,
    CancelStreamAck = 0xD0,

    // §1.6 Admin operations
    AdminStatsReq = 0x60,
    AdminStatsResp = 0xE0,
    AdminSnapshotReq = 0x61,
    AdminSnapshotResp = 0xE1,
    AdminRestoreReq = 0x62,
    AdminRestoreResp = 0xE2,
    AdminIntegrityCheckReq = 0x63,
    AdminIntegrityCheckResp = 0xE3,
    AdminMigrateEmbeddingsReq = 0x64,
    AdminMigrateEmbeddingsResp = 0xE4,
    AdminCreateContextReq = 0x65,
    AdminCreateContextResp = 0xE5,
    AdminRenameContextReq = 0x66,
    AdminRenameContextResp = 0xE6,
    AdminMoveMemoryReq = 0x67,
    AdminMoveMemoryResp = 0xE7,
    AdminReclassifyReq = 0x68,
    AdminReclassifyResp = 0xE8,
    AdminListTombstonedReq = 0x69,
    AdminListTombstonedResp = 0xE9,

    // §1.7 Errors
    Error = 0xFF,
}

impl Opcode {
    /// Decode an opcode byte. Returns [`ProtocolError::UnknownOpcode`] for
    /// values not assigned in the spec table (including reserved ranges
    /// `0x70..=0x7F` and `0xF0..=0xFE`).
    pub fn from_u8(b: u8) -> Result<Self, ProtocolError> {
        Ok(match b {
            0x01 => Self::Hello,
            0x81 => Self::Welcome,
            0x02 => Self::Auth,
            0x82 => Self::AuthOk,
            0x10 => Self::Ping,
            0x90 => Self::Pong,
            0x91 => Self::ServerPing,
            0x11 => Self::ClientPong,
            0x1F => Self::Bye,

            0x20 => Self::EncodeReq,
            0xA0 => Self::EncodeResp,
            0x21 => Self::RecallReq,
            0xA1 => Self::RecallResp,
            0x22 => Self::PlanReq,
            0xA2 => Self::PlanResp,
            0x23 => Self::ReasonReq,
            0xA3 => Self::ReasonResp,
            0x24 => Self::ForgetReq,
            0xA4 => Self::ForgetResp,
            0x2A => Self::EncodeVectorDirectReq,
            0xAA => Self::EncodeVectorDirectResp,

            0x30 => Self::SubscribeReq,
            0xB0 => Self::SubscribeEvent,
            0x31 => Self::UnsubscribeReq,
            0xB1 => Self::UnsubscribeResp,

            0x40 => Self::TxnBegin,
            0xC0 => Self::TxnBeginResp,
            0x41 => Self::TxnCommit,
            0xC1 => Self::TxnCommitResp,
            0x42 => Self::TxnAbort,
            0xC2 => Self::TxnAbortResp,

            0x50 => Self::CancelStream,
            0xD0 => Self::CancelStreamAck,

            0x60 => Self::AdminStatsReq,
            0xE0 => Self::AdminStatsResp,
            0x61 => Self::AdminSnapshotReq,
            0xE1 => Self::AdminSnapshotResp,
            0x62 => Self::AdminRestoreReq,
            0xE2 => Self::AdminRestoreResp,
            0x63 => Self::AdminIntegrityCheckReq,
            0xE3 => Self::AdminIntegrityCheckResp,
            0x64 => Self::AdminMigrateEmbeddingsReq,
            0xE4 => Self::AdminMigrateEmbeddingsResp,
            0x65 => Self::AdminCreateContextReq,
            0xE5 => Self::AdminCreateContextResp,
            0x66 => Self::AdminRenameContextReq,
            0xE6 => Self::AdminRenameContextResp,
            0x67 => Self::AdminMoveMemoryReq,
            0xE7 => Self::AdminMoveMemoryResp,
            0x68 => Self::AdminReclassifyReq,
            0xE8 => Self::AdminReclassifyResp,
            0x69 => Self::AdminListTombstonedReq,
            0xE9 => Self::AdminListTombstonedResp,

            0xFF => Self::Error,
            other => return Err(ProtocolError::UnknownOpcode(other)),
        })
    }

    /// Numeric value as it appears in the frame header.
    #[inline]
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// True if this opcode is server-bound (C→S), i.e. byte in `0x00..=0x7F`.
    /// Per spec §03/05 §4, this is the dispatch rule.
    #[inline]
    #[must_use]
    pub fn is_request(self) -> bool {
        (self as u8) < 0x80
    }

    /// True if this opcode is client-bound (S→C), i.e. byte in `0x80..=0xFF`.
    #[inline]
    #[must_use]
    pub fn is_response(self) -> bool {
        !self.is_request()
    }

    /// True if this opcode is in the admin range (spec §03/05 §1.6):
    /// `0x60..=0x69` for requests, `0xE0..=0xE9` for responses.
    #[inline]
    #[must_use]
    pub fn is_admin(self) -> bool {
        matches!(self as u8, 0x60..=0x69 | 0xE0..=0xE9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every spec-assigned opcode byte. If a row is added/changed in
    /// spec §03/05, this list is the single update site for the test
    /// suite.
    const ALL: &[(u8, Opcode)] = &[
        (0x01, Opcode::Hello),
        (0x81, Opcode::Welcome),
        (0x02, Opcode::Auth),
        (0x82, Opcode::AuthOk),
        (0x10, Opcode::Ping),
        (0x90, Opcode::Pong),
        (0x91, Opcode::ServerPing),
        (0x11, Opcode::ClientPong),
        (0x1F, Opcode::Bye),
        (0x20, Opcode::EncodeReq),
        (0xA0, Opcode::EncodeResp),
        (0x21, Opcode::RecallReq),
        (0xA1, Opcode::RecallResp),
        (0x22, Opcode::PlanReq),
        (0xA2, Opcode::PlanResp),
        (0x23, Opcode::ReasonReq),
        (0xA3, Opcode::ReasonResp),
        (0x24, Opcode::ForgetReq),
        (0xA4, Opcode::ForgetResp),
        (0x2A, Opcode::EncodeVectorDirectReq),
        (0xAA, Opcode::EncodeVectorDirectResp),
        (0x30, Opcode::SubscribeReq),
        (0xB0, Opcode::SubscribeEvent),
        (0x31, Opcode::UnsubscribeReq),
        (0xB1, Opcode::UnsubscribeResp),
        (0x40, Opcode::TxnBegin),
        (0xC0, Opcode::TxnBeginResp),
        (0x41, Opcode::TxnCommit),
        (0xC1, Opcode::TxnCommitResp),
        (0x42, Opcode::TxnAbort),
        (0xC2, Opcode::TxnAbortResp),
        (0x50, Opcode::CancelStream),
        (0xD0, Opcode::CancelStreamAck),
        (0x60, Opcode::AdminStatsReq),
        (0xE0, Opcode::AdminStatsResp),
        (0x61, Opcode::AdminSnapshotReq),
        (0xE1, Opcode::AdminSnapshotResp),
        (0x62, Opcode::AdminRestoreReq),
        (0xE2, Opcode::AdminRestoreResp),
        (0x63, Opcode::AdminIntegrityCheckReq),
        (0xE3, Opcode::AdminIntegrityCheckResp),
        (0x64, Opcode::AdminMigrateEmbeddingsReq),
        (0xE4, Opcode::AdminMigrateEmbeddingsResp),
        (0x65, Opcode::AdminCreateContextReq),
        (0xE5, Opcode::AdminCreateContextResp),
        (0x66, Opcode::AdminRenameContextReq),
        (0xE6, Opcode::AdminRenameContextResp),
        (0x67, Opcode::AdminMoveMemoryReq),
        (0xE7, Opcode::AdminMoveMemoryResp),
        (0x68, Opcode::AdminReclassifyReq),
        (0xE8, Opcode::AdminReclassifyResp),
        (0x69, Opcode::AdminListTombstonedReq),
        (0xE9, Opcode::AdminListTombstonedResp),
        (0xFF, Opcode::Error),
    ];

    #[test]
    fn every_opcode_round_trips_through_u8() {
        for &(byte, op) in ALL {
            assert_eq!(op.as_u8(), byte, "as_u8 for {op:?}");
            assert_eq!(Opcode::from_u8(byte).unwrap(), op, "from_u8(0x{byte:02X})");
        }
    }

    #[test]
    fn unknown_opcode_returns_error() {
        // 0x00 is unassigned (HELLO is 0x01).
        assert!(matches!(
            Opcode::from_u8(0x00),
            Err(ProtocolError::UnknownOpcode(0x00))
        ));
        // 0x70 is in the reserved server-bound range.
        assert!(matches!(
            Opcode::from_u8(0x70),
            Err(ProtocolError::UnknownOpcode(0x70))
        ));
        // 0xF0 is in the reserved client-bound range.
        assert!(matches!(
            Opcode::from_u8(0xF0),
            Err(ProtocolError::UnknownOpcode(0xF0))
        ));
        // 0xFE is the last reserved client-bound byte before ERROR.
        assert!(matches!(
            Opcode::from_u8(0xFE),
            Err(ProtocolError::UnknownOpcode(0xFE))
        ));
    }

    #[test]
    fn predicates_match_numeric_split() {
        assert!(Opcode::Hello.is_request());
        assert!(!Opcode::Hello.is_response());
        assert!(Opcode::Welcome.is_response());
        assert!(!Opcode::Welcome.is_request());
        assert!(Opcode::Bye.is_request()); // 0x1F numerically server-bound
        assert!(Opcode::Error.is_response()); // 0xFF numerically client-bound
    }

    #[test]
    fn admin_range_predicate() {
        assert!(Opcode::AdminStatsReq.is_admin());
        assert!(Opcode::AdminListTombstonedResp.is_admin());
        assert!(Opcode::AdminMoveMemoryReq.is_admin());
        assert!(!Opcode::EncodeReq.is_admin());
        assert!(!Opcode::Ping.is_admin());
        assert!(!Opcode::Error.is_admin());
    }

    proptest! {
        /// `from_u8` is total across the byte range: every byte either
        /// decodes to an opcode whose `as_u8()` equals the input, or
        /// returns `UnknownOpcode(byte)` carrying the same input byte.
        #[test]
        fn from_u8_is_total(b in 0u8..=u8::MAX) {
            match Opcode::from_u8(b) {
                Ok(op) => prop_assert_eq!(op.as_u8(), b),
                Err(ProtocolError::UnknownOpcode(x)) => prop_assert_eq!(x, b),
                Err(other) => prop_assert!(
                    false, "unexpected error variant: {other:?}"
                ),
            }
        }
    }
}
