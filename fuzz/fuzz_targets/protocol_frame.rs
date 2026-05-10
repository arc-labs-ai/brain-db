//! Fuzz target: `Frame::decode`.
//!
//! Run with:
//!
//! ```
//! cargo +nightly fuzz run protocol_frame -- -max_total_time=60
//! ```
//!
//! Invariants enforced (spec §03/11):
//!
//! 1. `Frame::decode` MUST NOT panic on arbitrary input. It returns
//!    either a structured `ProtocolError` or a successfully parsed
//!    frame.
//! 2. If a frame parses successfully, the consumed prefix MUST
//!    re-encode to itself — the decoder accepted only canonical bytes.
//!
//! libFuzzer's coverage-guided exploration complements the existing
//! `frame::tests::decode_arbitrary_bytes_is_total` proptest, which
//! covers the same property on 1024 quickcheck-style inputs.

#![no_main]

use brain_protocol::Frame;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((frame, rest)) = Frame::decode(data) {
        let consumed = data.len() - rest.len();
        let reencoded = frame.encode();
        assert_eq!(
            reencoded.as_slice(),
            &data[..consumed],
            "decoded frame must re-encode to the consumed prefix"
        );
    }
});
