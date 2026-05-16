//! Wire-protocol opcodes (u16, per spec §03/05 + §28/00).
//!
//! ## Namespaces (high byte)
//!
//! - `0x00xx` — substrate ops (cognitive primitives + connection mgmt +
//!   admin), spec §03/05.
//! - `0x01xx` — knowledge layer (schema / entities / statements /
//!   relations / queries / extractors), spec §28/00.
//! - `0x02xx`–`0xFFxx` — reserved for future namespaces.
//!
//! ## Direction (low byte's high bit)
//!
//! Within a namespace, low byte `< 0x80` is server-bound (C→S, request);
//! low byte `>= 0x80` is client-bound (S→C, response). Substrate's
//! existing `0x2N → 0xAN` (encode→encode_resp) convention is preserved
//! by promoting it to `0x002N → 0x00AN`. Knowledge follows the same
//! convention: e.g. `0x0130 ENTITY_CREATE` (req) ↔ `0x01B0 ENTITY_CREATE_RESP`.
//!
//! ## Reserved ranges
//!
//! Substrate reserved (low byte): `0x70–0x7F` (server-bound, mostly used
//! by knowledge gateway in prior designs — no longer; the knowledge layer
//! now lives in its own namespace and 0x70–0x7F are open for future
//! substrate ops) and `0xF0–0xFE` (client-bound, reserved future).

use crate::error::ProtocolError;

/// Wire-protocol opcode. See spec §03/05 §1 + §28/00 for the full table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum Opcode {
    // ============================================================
    // Substrate namespace (high byte = 0x00).
    // ============================================================

    // §03/05 §1.1 Connection management
    Hello = 0x0001,
    Welcome = 0x0081,
    Auth = 0x0002,
    AuthOk = 0x0082,
    Ping = 0x0010,
    Pong = 0x0090,
    ServerPing = 0x0091,
    ClientPong = 0x0011,
    Bye = 0x001F,

    // §03/05 §1.2 Cognitive operations
    EncodeReq = 0x0020,
    EncodeResp = 0x00A0,
    RecallReq = 0x0021,
    RecallResp = 0x00A1,
    PlanReq = 0x0022,
    PlanResp = 0x00A2,
    ReasonReq = 0x0023,
    ReasonResp = 0x00A3,
    ForgetReq = 0x0024,
    ForgetResp = 0x00A4,
    LinkReq = 0x0025,
    LinkResp = 0x00A5,
    UnlinkReq = 0x0026,
    UnlinkResp = 0x00A6,
    EncodeVectorDirectReq = 0x002A,
    EncodeVectorDirectResp = 0x00AA,

    // §03/05 §1.3 Subscription
    SubscribeReq = 0x0030,
    SubscribeEvent = 0x00B0,
    UnsubscribeReq = 0x0031,
    UnsubscribeResp = 0x00B1,

    // §03/05 §1.4 Transactions
    TxnBegin = 0x0040,
    TxnBeginResp = 0x00C0,
    TxnCommit = 0x0041,
    TxnCommitResp = 0x00C1,
    TxnAbort = 0x0042,
    TxnAbortResp = 0x00C2,

    // §03/05 §1.5 Stream control
    CancelStream = 0x0050,
    CancelStreamAck = 0x00D0,

    // §03/05 §1.6 Admin operations
    AdminStatsReq = 0x0060,
    AdminStatsResp = 0x00E0,
    AdminSnapshotReq = 0x0061,
    AdminSnapshotResp = 0x00E1,
    AdminRestoreReq = 0x0062,
    AdminRestoreResp = 0x00E2,
    AdminIntegrityCheckReq = 0x0063,
    AdminIntegrityCheckResp = 0x00E3,
    AdminMigrateEmbeddingsReq = 0x0064,
    AdminMigrateEmbeddingsResp = 0x00E4,
    AdminCreateContextReq = 0x0065,
    AdminCreateContextResp = 0x00E5,
    AdminRenameContextReq = 0x0066,
    AdminRenameContextResp = 0x00E6,
    AdminMoveMemoryReq = 0x0067,
    AdminMoveMemoryResp = 0x00E7,
    AdminReclassifyReq = 0x0068,
    AdminReclassifyResp = 0x00E8,
    AdminListTombstonedReq = 0x0069,
    AdminListTombstonedResp = 0x00E9,

    // §03/05 §1.7 Errors
    Error = 0x00FF,

    // ============================================================
    // Knowledge namespace (high byte = 0x01). Spec §28/00.
    // Phase 16.6c adds entity ops 0x0130-0x0133 + their responses.
    // Other §28 opcodes land in phases 17-24.
    // ============================================================

    // §28 schema operations (0x0120-0x0123 low-byte range) — phase 19.6.
    SchemaUploadReq = 0x0120,
    SchemaUploadResp = 0x01A0,
    SchemaGetReq = 0x0121,
    SchemaGetResp = 0x01A1,
    SchemaListReq = 0x0122,
    SchemaListResp = 0x01A2,
    SchemaValidateReq = 0x0123,
    SchemaValidateResp = 0x01A3,

    // §28 entity operations (0x0130-0x013F low-byte range)
    EntityCreateReq = 0x0130,
    EntityCreateResp = 0x01B0,
    EntityGetReq = 0x0131,
    EntityGetResp = 0x01B1,
    EntityUpdateReq = 0x0132,
    EntityUpdateResp = 0x01B2,
    EntityRenameReq = 0x0133,
    EntityRenameResp = 0x01B3,
    EntityMergeReq = 0x0134,
    EntityMergeResp = 0x01B4,
    EntityUnmergeReq = 0x0135,
    EntityUnmergeResp = 0x01B5,
    EntityResolveReq = 0x0136,
    EntityResolveResp = 0x01B6,
    EntityListReq = 0x0137,
    EntityListResp = 0x01B7,
    EntityTombstoneReq = 0x0138,
    EntityTombstoneResp = 0x01B8,

    // §28 statement operations (0x0140-0x014F low-byte range) — phase 17.6.
    StatementCreateReq = 0x0140,
    StatementCreateResp = 0x01C0,
    StatementGetReq = 0x0141,
    StatementGetResp = 0x01C1,
    StatementSupersedeReq = 0x0142,
    StatementSupersedeResp = 0x01C2,
    StatementTombstoneReq = 0x0143,
    StatementTombstoneResp = 0x01C3,
    StatementRetractReq = 0x0144,
    StatementRetractResp = 0x01C4,
    StatementHistoryReq = 0x0145,
    StatementHistoryResp = 0x01C5,
    StatementListReq = 0x0146,
    StatementListResp = 0x01C6,

    // §28 relation operations (0x0150-0x015F low-byte range) — phase 18.6.
    RelationCreateReq = 0x0150,
    RelationCreateResp = 0x01D0,
    RelationGetReq = 0x0151,
    RelationGetResp = 0x01D1,
    RelationSupersedeReq = 0x0152,
    RelationSupersedeResp = 0x01D2,
    RelationTombstoneReq = 0x0153,
    RelationTombstoneResp = 0x01D3,
    RelationListFromReq = 0x0154,
    RelationListFromResp = 0x01D4,
    RelationListToReq = 0x0155,
    RelationListToResp = 0x01D5,
    RelationTraverseReq = 0x0156,
    RelationTraverseResp = 0x01D6,
}

impl Opcode {
    /// Decode a u16 opcode value. Returns [`ProtocolError::UnknownOpcode`]
    /// for values not assigned in the spec table.
    pub fn from_u16(v: u16) -> Result<Self, ProtocolError> {
        Ok(match v {
            // Substrate namespace
            0x0001 => Self::Hello,
            0x0081 => Self::Welcome,
            0x0002 => Self::Auth,
            0x0082 => Self::AuthOk,
            0x0010 => Self::Ping,
            0x0090 => Self::Pong,
            0x0091 => Self::ServerPing,
            0x0011 => Self::ClientPong,
            0x001F => Self::Bye,

            0x0020 => Self::EncodeReq,
            0x00A0 => Self::EncodeResp,
            0x0021 => Self::RecallReq,
            0x00A1 => Self::RecallResp,
            0x0022 => Self::PlanReq,
            0x00A2 => Self::PlanResp,
            0x0023 => Self::ReasonReq,
            0x00A3 => Self::ReasonResp,
            0x0024 => Self::ForgetReq,
            0x00A4 => Self::ForgetResp,
            0x0025 => Self::LinkReq,
            0x00A5 => Self::LinkResp,
            0x0026 => Self::UnlinkReq,
            0x00A6 => Self::UnlinkResp,
            0x002A => Self::EncodeVectorDirectReq,
            0x00AA => Self::EncodeVectorDirectResp,

            0x0030 => Self::SubscribeReq,
            0x00B0 => Self::SubscribeEvent,
            0x0031 => Self::UnsubscribeReq,
            0x00B1 => Self::UnsubscribeResp,

            0x0040 => Self::TxnBegin,
            0x00C0 => Self::TxnBeginResp,
            0x0041 => Self::TxnCommit,
            0x00C1 => Self::TxnCommitResp,
            0x0042 => Self::TxnAbort,
            0x00C2 => Self::TxnAbortResp,

            0x0050 => Self::CancelStream,
            0x00D0 => Self::CancelStreamAck,

            0x0060 => Self::AdminStatsReq,
            0x00E0 => Self::AdminStatsResp,
            0x0061 => Self::AdminSnapshotReq,
            0x00E1 => Self::AdminSnapshotResp,
            0x0062 => Self::AdminRestoreReq,
            0x00E2 => Self::AdminRestoreResp,
            0x0063 => Self::AdminIntegrityCheckReq,
            0x00E3 => Self::AdminIntegrityCheckResp,
            0x0064 => Self::AdminMigrateEmbeddingsReq,
            0x00E4 => Self::AdminMigrateEmbeddingsResp,
            0x0065 => Self::AdminCreateContextReq,
            0x00E5 => Self::AdminCreateContextResp,
            0x0066 => Self::AdminRenameContextReq,
            0x00E6 => Self::AdminRenameContextResp,
            0x0067 => Self::AdminMoveMemoryReq,
            0x00E7 => Self::AdminMoveMemoryResp,
            0x0068 => Self::AdminReclassifyReq,
            0x00E8 => Self::AdminReclassifyResp,
            0x0069 => Self::AdminListTombstonedReq,
            0x00E9 => Self::AdminListTombstonedResp,

            0x00FF => Self::Error,

            // Knowledge namespace
            0x0130 => Self::EntityCreateReq,
            0x01B0 => Self::EntityCreateResp,
            0x0131 => Self::EntityGetReq,
            0x01B1 => Self::EntityGetResp,
            0x0132 => Self::EntityUpdateReq,
            0x01B2 => Self::EntityUpdateResp,
            0x0133 => Self::EntityRenameReq,
            0x01B3 => Self::EntityRenameResp,
            0x0134 => Self::EntityMergeReq,
            0x01B4 => Self::EntityMergeResp,
            0x0135 => Self::EntityUnmergeReq,
            0x01B5 => Self::EntityUnmergeResp,
            0x0136 => Self::EntityResolveReq,
            0x01B6 => Self::EntityResolveResp,
            0x0137 => Self::EntityListReq,
            0x01B7 => Self::EntityListResp,
            0x0138 => Self::EntityTombstoneReq,
            0x01B8 => Self::EntityTombstoneResp,

            // §28 statement operations (phase 17.6).
            0x0140 => Self::StatementCreateReq,
            0x01C0 => Self::StatementCreateResp,
            0x0141 => Self::StatementGetReq,
            0x01C1 => Self::StatementGetResp,
            0x0142 => Self::StatementSupersedeReq,
            0x01C2 => Self::StatementSupersedeResp,
            0x0143 => Self::StatementTombstoneReq,
            0x01C3 => Self::StatementTombstoneResp,
            0x0144 => Self::StatementRetractReq,
            0x01C4 => Self::StatementRetractResp,
            0x0145 => Self::StatementHistoryReq,
            0x01C5 => Self::StatementHistoryResp,
            0x0146 => Self::StatementListReq,
            0x01C6 => Self::StatementListResp,

            // §28 relation operations (phase 18.6).
            0x0150 => Self::RelationCreateReq,
            0x01D0 => Self::RelationCreateResp,
            0x0151 => Self::RelationGetReq,
            0x01D1 => Self::RelationGetResp,
            0x0152 => Self::RelationSupersedeReq,
            0x01D2 => Self::RelationSupersedeResp,
            0x0153 => Self::RelationTombstoneReq,
            0x01D3 => Self::RelationTombstoneResp,
            0x0154 => Self::RelationListFromReq,
            0x01D4 => Self::RelationListFromResp,
            0x0155 => Self::RelationListToReq,
            0x01D5 => Self::RelationListToResp,
            0x0156 => Self::RelationTraverseReq,
            0x01D6 => Self::RelationTraverseResp,

            0x0120 => Self::SchemaUploadReq,
            0x01A0 => Self::SchemaUploadResp,
            0x0121 => Self::SchemaGetReq,
            0x01A1 => Self::SchemaGetResp,
            0x0122 => Self::SchemaListReq,
            0x01A2 => Self::SchemaListResp,
            0x0123 => Self::SchemaValidateReq,
            0x01A3 => Self::SchemaValidateResp,

            other => return Err(ProtocolError::UnknownOpcode(other)),
        })
    }

    /// Numeric value as it appears in the frame header (big-endian u16).
    #[inline]
    #[must_use]
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    /// Namespace byte (high byte): 0x00 substrate, 0x01 knowledge.
    #[inline]
    #[must_use]
    pub fn namespace(self) -> u8 {
        ((self as u16) >> 8) as u8
    }

    /// Low byte (operation index within the namespace).
    #[inline]
    #[must_use]
    pub fn low_byte(self) -> u8 {
        (self as u16) as u8
    }

    /// True if this opcode is server-bound (C→S, request) — low byte's
    /// high bit is clear. Mirrors spec §03/05 §4 dispatch rule, applied
    /// per-namespace.
    #[inline]
    #[must_use]
    pub fn is_request(self) -> bool {
        self.low_byte() < 0x80
    }

    /// True if this opcode is client-bound (S→C, response).
    #[inline]
    #[must_use]
    pub fn is_response(self) -> bool {
        !self.is_request()
    }

    /// True if this opcode is in the substrate's admin range
    /// (spec §03/05 §1.6): low byte `0x60..=0x69` (req) or `0xE0..=0xE9`
    /// (resp), namespace 0x00.
    #[inline]
    #[must_use]
    pub fn is_admin(self) -> bool {
        self.namespace() == 0x00 && matches!(self.low_byte(), 0x60..=0x69 | 0xE0..=0xE9)
    }

    /// True if this opcode is in the knowledge namespace (spec §28).
    #[inline]
    #[must_use]
    pub fn is_knowledge(self) -> bool {
        self.namespace() == 0x01
    }

    /// True if this opcode rides on the connection-level stream
    /// (stream_id MUST be 0 per spec §03/11 §2.5): HELLO, WELCOME,
    /// AUTH, AUTH_OK, PING, PONG, ServerPing, ClientPong, BYE.
    ///
    /// ERROR (0x00FF) is deliberately excluded — the server can emit
    /// an error frame on either stream 0 (handshake error) or on
    /// the offending op's stream (per-op error).
    #[inline]
    #[must_use]
    pub fn is_connection_level(self) -> bool {
        matches!(
            self,
            Opcode::Hello
                | Opcode::Welcome
                | Opcode::Auth
                | Opcode::AuthOk
                | Opcode::Ping
                | Opcode::Pong
                | Opcode::ServerPing
                | Opcode::ClientPong
                | Opcode::Bye
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every spec-assigned opcode. If a row is added/changed in spec
    /// §03/05 or §28/00, this list is the single update site.
    const ALL: &[(u16, Opcode)] = &[
        // Substrate
        (0x0001, Opcode::Hello),
        (0x0081, Opcode::Welcome),
        (0x0002, Opcode::Auth),
        (0x0082, Opcode::AuthOk),
        (0x0010, Opcode::Ping),
        (0x0090, Opcode::Pong),
        (0x0091, Opcode::ServerPing),
        (0x0011, Opcode::ClientPong),
        (0x001F, Opcode::Bye),
        (0x0020, Opcode::EncodeReq),
        (0x00A0, Opcode::EncodeResp),
        (0x0021, Opcode::RecallReq),
        (0x00A1, Opcode::RecallResp),
        (0x0022, Opcode::PlanReq),
        (0x00A2, Opcode::PlanResp),
        (0x0023, Opcode::ReasonReq),
        (0x00A3, Opcode::ReasonResp),
        (0x0024, Opcode::ForgetReq),
        (0x00A4, Opcode::ForgetResp),
        (0x0025, Opcode::LinkReq),
        (0x00A5, Opcode::LinkResp),
        (0x0026, Opcode::UnlinkReq),
        (0x00A6, Opcode::UnlinkResp),
        (0x002A, Opcode::EncodeVectorDirectReq),
        (0x00AA, Opcode::EncodeVectorDirectResp),
        (0x0030, Opcode::SubscribeReq),
        (0x00B0, Opcode::SubscribeEvent),
        (0x0031, Opcode::UnsubscribeReq),
        (0x00B1, Opcode::UnsubscribeResp),
        (0x0040, Opcode::TxnBegin),
        (0x00C0, Opcode::TxnBeginResp),
        (0x0041, Opcode::TxnCommit),
        (0x00C1, Opcode::TxnCommitResp),
        (0x0042, Opcode::TxnAbort),
        (0x00C2, Opcode::TxnAbortResp),
        (0x0050, Opcode::CancelStream),
        (0x00D0, Opcode::CancelStreamAck),
        (0x0060, Opcode::AdminStatsReq),
        (0x00E0, Opcode::AdminStatsResp),
        (0x0061, Opcode::AdminSnapshotReq),
        (0x00E1, Opcode::AdminSnapshotResp),
        (0x0062, Opcode::AdminRestoreReq),
        (0x00E2, Opcode::AdminRestoreResp),
        (0x0063, Opcode::AdminIntegrityCheckReq),
        (0x00E3, Opcode::AdminIntegrityCheckResp),
        (0x0064, Opcode::AdminMigrateEmbeddingsReq),
        (0x00E4, Opcode::AdminMigrateEmbeddingsResp),
        (0x0065, Opcode::AdminCreateContextReq),
        (0x00E5, Opcode::AdminCreateContextResp),
        (0x0066, Opcode::AdminRenameContextReq),
        (0x00E6, Opcode::AdminRenameContextResp),
        (0x0067, Opcode::AdminMoveMemoryReq),
        (0x00E7, Opcode::AdminMoveMemoryResp),
        (0x0068, Opcode::AdminReclassifyReq),
        (0x00E8, Opcode::AdminReclassifyResp),
        (0x0069, Opcode::AdminListTombstonedReq),
        (0x00E9, Opcode::AdminListTombstonedResp),
        (0x00FF, Opcode::Error),
        // Knowledge
        (0x0130, Opcode::EntityCreateReq),
        (0x01B0, Opcode::EntityCreateResp),
        (0x0131, Opcode::EntityGetReq),
        (0x01B1, Opcode::EntityGetResp),
        (0x0132, Opcode::EntityUpdateReq),
        (0x01B2, Opcode::EntityUpdateResp),
        (0x0133, Opcode::EntityRenameReq),
        (0x01B3, Opcode::EntityRenameResp),
        (0x0134, Opcode::EntityMergeReq),
        (0x01B4, Opcode::EntityMergeResp),
        (0x0135, Opcode::EntityUnmergeReq),
        (0x01B5, Opcode::EntityUnmergeResp),
        (0x0136, Opcode::EntityResolveReq),
        (0x01B6, Opcode::EntityResolveResp),
        (0x0137, Opcode::EntityListReq),
        (0x01B7, Opcode::EntityListResp),
        (0x0138, Opcode::EntityTombstoneReq),
        (0x01B8, Opcode::EntityTombstoneResp),
    ];

    #[test]
    fn every_opcode_round_trips_through_u16() {
        for &(value, op) in ALL {
            assert_eq!(op.as_u16(), value, "as_u16 for {op:?}");
            assert_eq!(
                Opcode::from_u16(value).unwrap(),
                op,
                "from_u16(0x{value:04X})"
            );
        }
    }

    #[test]
    fn unknown_opcode_returns_error() {
        // 0x0000 is unassigned (HELLO is 0x0001).
        assert!(matches!(
            Opcode::from_u16(0x0000),
            Err(ProtocolError::UnknownOpcode(0x0000))
        ));
        // 0x0070 is in the reserved substrate server-bound range.
        assert!(matches!(
            Opcode::from_u16(0x0070),
            Err(ProtocolError::UnknownOpcode(0x0070))
        ));
        // 0x0139 is a not-yet-assigned knowledge entity opcode.
        assert!(matches!(
            Opcode::from_u16(0x0139),
            Err(ProtocolError::UnknownOpcode(0x0139))
        ));
        // 0x0200 is in the reserved future namespace.
        assert!(matches!(
            Opcode::from_u16(0x0200),
            Err(ProtocolError::UnknownOpcode(0x0200))
        ));
    }

    #[test]
    fn predicates_match_split() {
        assert!(Opcode::Hello.is_request());
        assert!(!Opcode::Hello.is_response());
        assert!(Opcode::Welcome.is_response());
        assert!(!Opcode::Welcome.is_request());
        assert!(Opcode::Bye.is_request());
        assert!(Opcode::Error.is_response());
        // Knowledge ops follow the same low-byte rule.
        assert!(Opcode::EntityCreateReq.is_request());
        assert!(Opcode::EntityCreateResp.is_response());
    }

    #[test]
    fn admin_range_predicate() {
        assert!(Opcode::AdminStatsReq.is_admin());
        assert!(Opcode::AdminListTombstonedResp.is_admin());
        assert!(!Opcode::EncodeReq.is_admin());
        assert!(!Opcode::Ping.is_admin());
        assert!(!Opcode::Error.is_admin());
        // Knowledge ops are never admin.
        assert!(!Opcode::EntityCreateReq.is_admin());
    }

    #[test]
    fn knowledge_predicate_split() {
        assert!(!Opcode::EncodeReq.is_knowledge());
        assert!(!Opcode::AdminStatsReq.is_knowledge());
        assert!(Opcode::EntityCreateReq.is_knowledge());
        assert!(Opcode::EntityRenameResp.is_knowledge());
    }

    #[test]
    fn namespace_byte_split() {
        assert_eq!(Opcode::Hello.namespace(), 0x00);
        assert_eq!(Opcode::EncodeReq.namespace(), 0x00);
        assert_eq!(Opcode::EntityCreateReq.namespace(), 0x01);
        assert_eq!(Opcode::EntityRenameResp.namespace(), 0x01);
    }

    proptest! {
        /// `from_u16` is total across the u16 space: every value either
        /// decodes to an opcode whose `as_u16()` equals the input, or
        /// returns `UnknownOpcode(v)` carrying the same input.
        #[test]
        fn from_u16_is_total(v in 0u16..=u16::MAX) {
            match Opcode::from_u16(v) {
                Ok(op) => prop_assert_eq!(op.as_u16(), v),
                Err(ProtocolError::UnknownOpcode(x)) => prop_assert_eq!(x, v),
                Err(other) => prop_assert!(
                    false, "unexpected error variant: {other:?}"
                ),
            }
        }
    }
}
