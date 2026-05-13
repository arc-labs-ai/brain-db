//! Startup orchestration: TLS provider build, signal handling, and
//! graceful shard drain. The pieces here are wired together by
//! `main::run`.

#[cfg(target_os = "linux")]
pub mod shutdown;
#[cfg(target_os = "linux")]
pub mod tls;
