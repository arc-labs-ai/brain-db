//! Connection-lifecycle wire ops: the handshake exchange (HELLO /
//! WELCOME / AUTH / AUTH_OK plus `negotiate()`) and the small
//! stream-control surface (PING / PONG / SERVER_PING / CANCEL_STREAM /
//! BYE). Operations that belong to a domain noun (memory, entity,
//! statement, …) live under `crate::ops`, not here.

pub mod handshake;
pub mod stream;
