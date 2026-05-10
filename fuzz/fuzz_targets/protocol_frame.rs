//! Fuzz target for the Brain wire protocol parser.
//!
//! Run with: `cargo fuzz run protocol_frame`
//!
//! Once the parser is implemented in `brain-protocol`, this target should
//! call it on arbitrary bytes and assert it never panics — only returns a
//! structured error for malformed input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // TODO(phase-1): replace with actual parser invocation.
    // For now, exercise the constants and opcode enum so the harness builds.
    let _ = data.len();
    let _ = brain_protocol::MAGIC;
    let _ = brain_protocol::HEADER_SIZE;
});
