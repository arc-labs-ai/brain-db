//! Listener bind + per-stream socket-option helpers.
//!
//! requires `SO_REUSEADDR` (graceful restart),
//! `TCP_NODELAY` (low per-request latency), and `SO_KEEPALIVE` on
//! both the listener and each accepted stream. The existing
//! `brain-server::network::connection` applies these via
//! [`tokio::net::TcpSocket`] for the listener and [`socket2::SockRef`]
//! for the per-stream keepalive knobs. We do the same here.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

/// Knobs applied at bind and per-accepted-stream.
#[derive(Debug, Clone)]
pub struct BindConfig {
    /// Set `SO_REUSEADDR` on the listener. Default `true` —
    /// enables graceful restart by allowing the new process to bind
    /// before the old socket's TIME_WAIT expires.
    pub reuse_addr: bool,

    /// Set `TCP_NODELAY` on each accepted stream. Default `true` —
    /// HTTP/1.1 request/response is small; Nagle's algorithm only
    /// hurts us.
    pub tcp_nodelay: bool,

    /// TCP keepalive configuration applied to each accepted stream.
    /// `None` keeps the OS default (typically no keepalive). The
    /// existing brain-server data plane uses
    /// `(idle=75s, interval=15s, retries=9)`.
    pub keepalive: Option<KeepAlive>,

    /// Listen backlog. Default 1024 (matches brain-server data plane;
    /// well below default `somaxconn` of 4096 on stock Linux).
    pub backlog: u32,
}

/// TCP keepalive parameters.
#[derive(Debug, Clone)]
pub struct KeepAlive {
    /// Idle time before keepalive probes start. Maps to `TCP_KEEPIDLE`.
    pub time: Duration,
    /// Interval between keepalive probes. Maps to `TCP_KEEPINTVL`.
    pub interval: Duration,
    /// Number of probes before the connection is dropped. Maps to
    /// `TCP_KEEPCNT`.
    pub retries: u32,
}

impl Default for BindConfig {
    fn default() -> Self {
        Self {
            reuse_addr: true,
            tcp_nodelay: true,
            keepalive: Some(KeepAlive {
                time: Duration::from_secs(75),
                interval: Duration::from_secs(15),
                retries: 9,
            }),
            backlog: 1024,
        }
    }
}

/// Bind a TCP listener at `addr` with Brain's standard socket options.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if `socket()`, `bind()`, or
/// `listen()` fails.
pub fn bind(addr: SocketAddr, cfg: &BindConfig) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    if cfg.reuse_addr {
        socket.set_reuseaddr(true)?;
    }
    socket.bind(addr)?;
    socket.listen(cfg.backlog)
}

/// Apply per-stream options after `accept()`.
///
/// Some kernels inherit `TCP_NODELAY` from the listener; others don't.
/// Apply explicitly so behaviour is the same across distros.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] from any `setsockopt` call.
pub fn apply_stream_opts(stream: &TcpStream, cfg: &BindConfig) -> io::Result<()> {
    if cfg.tcp_nodelay {
        stream.set_nodelay(true)?;
    }
    if let Some(ka) = &cfg.keepalive {
        let sock = SockRef::from(stream);
        let keepalive = TcpKeepalive::new()
            .with_time(ka.time)
            .with_interval(ka.interval)
            .with_retries(ka.retries);
        sock.set_tcp_keepalive(&keepalive)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_loopback_succeeds() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind(addr, &BindConfig::default()).expect("bind");
        let bound = listener.local_addr().expect("local_addr");
        assert_eq!(bound.ip(), addr.ip());
        assert_ne!(bound.port(), 0);
    }
}
