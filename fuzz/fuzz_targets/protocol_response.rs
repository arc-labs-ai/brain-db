//! Fuzz target: `ResponseBody::decode`.
//!
//! Run with:
//!
//! ```
//! cargo +nightly fuzz run protocol_response -- -max_total_time=60
//! ```
//!
//! Invariant (spec §03/11):
//!
//! - `ResponseBody::decode` MUST NOT panic for any opcode/byte combo.
//!   Either an `Ok(...)` or a structured `ProtocolError`.
//!
//! Same harness shape as `protocol_request`: first byte (mod len)
//! picks a client-bound opcode; remainder is the rkyv payload.

#![no_main]

use brain_protocol::{Opcode, ResponseBody};
use libfuzzer_sys::fuzz_target;

const OPCODES: &[Opcode] = &[
    Opcode::Welcome,
    Opcode::AuthOk,
    Opcode::EncodeResp,
    Opcode::EncodeVectorDirectResp,
    Opcode::RecallResp,
    Opcode::PlanResp,
    Opcode::ReasonResp,
    Opcode::ForgetResp,
    Opcode::SubscribeEvent,
    Opcode::UnsubscribeResp,
    Opcode::TxnBeginResp,
    Opcode::TxnCommitResp,
    Opcode::TxnAbortResp,
    Opcode::CancelStreamAck,
    Opcode::Pong,
    Opcode::ServerPing,
    Opcode::AdminStatsResp,
    Opcode::AdminSnapshotResp,
    Opcode::AdminRestoreResp,
    Opcode::AdminIntegrityCheckResp,
    Opcode::AdminMigrateEmbeddingsResp,
    Opcode::AdminCreateContextResp,
    Opcode::AdminRenameContextResp,
    Opcode::AdminMoveMemoryResp,
    Opcode::AdminReclassifyResp,
    Opcode::AdminListTombstonedResp,
    Opcode::Error,
];

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let opcode = OPCODES[(data[0] as usize) % OPCODES.len()];
    let _ = ResponseBody::decode(opcode, &data[1..]);
});
