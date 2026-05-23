//! Top-level dispatch envelopes: the `RequestBody` and `ResponseBody`
//! enums that fan out to every op's payload, the per-op conversion
//! between core types and their wire mirrors, and the `ErrorResponse`
//! payload carried by the dedicated ERROR frame. The envelope binds
//! the codec layer to the per-domain ops without leaking either side's
//! internals.

pub mod convert;
pub mod error;
pub mod request;
pub mod response;
