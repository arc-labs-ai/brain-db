//! Wire-protocol bridge. Re-uses `brain-protocol`'s `Frame`,
//! `Header`, `RequestBody`, `ResponseBody` types over a Tokio
//! `TcpStream`. The handshake FSM (HELLO → WELCOME → AUTH →
//! AUTH_OK per spec §03/06) lives in [`handshake`]. Frame I/O
//! helpers live in [`frames`].

pub mod frames;
pub mod handshake;
