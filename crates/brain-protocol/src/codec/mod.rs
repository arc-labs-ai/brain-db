//! Bytes-on-the-wire plumbing. Frame header, frame envelope, CRC32C
//! checksums, the rkyv archive pipeline, and the `Opcode` enum live here.
//! No operation semantics — nothing in this module knows what an
//! `EncodeRequest` is, only how to put bytes onto and pull bytes off of
//! the wire.

pub mod crc;
pub mod frame;
pub mod header;
pub mod opcode;
pub mod rkyv;
