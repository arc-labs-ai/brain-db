//! Fuzz target: `schema::parse_schema`.
//!
//! Run with:
//!
//! ```
//! cargo +nightly fuzz run schema_parser -- -max_total_time=60
//! ```
//!
//! Invariant:
//!
//! - `parse_schema` MUST be total on arbitrary UTF-8 input: it returns
//!   either an `Ok(Schema)` or a structured `ParseError`. It must never
//!   panic, never overflow the native stack, and never hang.
//!
//! This surface is reachable from any low-privilege caller via the
//! `SCHEMA_VALIDATE` / `SCHEMA_UPLOAD` wire ops (spec §04), which feed the
//! untrusted document straight into `parse_schema`. The parser hands its
//! input to a recursive-descent pest grammar, so a deeply-nested-bracket
//! document could overflow the stack and abort the whole process
//! (all shards/tenants) — an availability DoS. A `MAX_NESTING_DEPTH=64`
//! byte-scan guard in front of pest is meant to reject those before they
//! recurse; a regex-skip bypass of that guard (`/{{{...`) was fixed, and
//! the corpus seeds carry it as a regression witness.
//!
//! The fuzzer only feeds valid UTF-8 — `parse_schema` takes `&str`, and
//! the wire layer decodes CBOR text before calling it, so non-UTF-8 bytes
//! never reach this function.

#![no_main]

use brain_protocol::schema::parse_schema;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Only valid UTF-8 reaches parse_schema on the wire; mirror that here.
    if let Ok(s) = core::str::from_utf8(data) {
        let _ = parse_schema(s);
    }
});
