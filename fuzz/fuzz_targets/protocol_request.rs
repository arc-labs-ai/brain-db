//! Fuzz target: `RequestBody::decode`.
//!
//! Run with:
//!
//! ```
//! cargo +nightly fuzz run protocol_request -- -max_total_time=60
//! ```
//!
//! Invariant (spec §03/11):
//!
//! - `RequestBody::decode` MUST NOT panic for any opcode/byte combo.
//!   Either an `Ok(...)` or a structured `ProtocolError`.
//!
//! The harness uses the first input byte (mod len) to pick a
//! server-bound opcode, then feeds the remaining bytes to the decoder.
//! Cycling through opcodes lets libFuzzer's coverage tracking explore
//! every variant's rkyv-validation path; if we used `Opcode::from_u8`
//! the random byte would skip ~80% of inputs as `UnknownOpcode`.

#![no_main]

use brain_protocol::{Opcode, RequestBody};
use libfuzzer_sys::fuzz_target;

const OPCODES: &[Opcode] = &[
    Opcode::Hello,
    Opcode::Auth,
    Opcode::EncodeReq,
    Opcode::EncodeVectorDirectReq,
    Opcode::RecallReq,
    Opcode::PlanReq,
    Opcode::ReasonReq,
    Opcode::ForgetReq,
    Opcode::SubscribeReq,
    Opcode::UnsubscribeReq,
    Opcode::TxnBegin,
    Opcode::TxnCommit,
    Opcode::TxnAbort,
    Opcode::CancelStream,
    Opcode::Ping,
    Opcode::ClientPong,
    Opcode::Bye,
    Opcode::AdminStatsReq,
    Opcode::AdminSnapshotReq,
    Opcode::AdminRestoreReq,
    Opcode::AdminIntegrityCheckReq,
    Opcode::AdminMigrateEmbeddingsReq,
    Opcode::AdminCreateContextReq,
    Opcode::AdminRenameContextReq,
    Opcode::AdminMoveMemoryReq,
    Opcode::AdminReclassifyReq,
    Opcode::AdminListTombstonedReq,
];

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let opcode = OPCODES[(data[0] as usize) % OPCODES.len()];
    let _ = RequestBody::decode(opcode, &data[1..]);
});
