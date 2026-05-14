//! WebSocket client.
//!
//! Thin wrapper around [`tokio_tungstenite::connect_async`]. Builder
//! shape so we can grow custom headers, connect timeout, and a TLS
//! config later without breaking call sites.
//!
//! Symmetric with [`crate::ws::accept`] on the server side: same
//! crate, same module, same vocabulary.

use std::time::Duration;

use http::{HeaderName, HeaderValue, Response as HttpResponse};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

/// Result of a successful connect.
///
/// Carries the WS stream plus the raw 101 response so callers can
/// inspect server-side headers (e.g. `Sec-WebSocket-Protocol` once
/// subprotocol negotiation lands).
///
/// **`MaybeTlsStream<TcpStream>` note:** tungstenite returns this
/// type unconditionally — TLS support is gated by its own feature
/// flags (`native-tls` / `rustls-tls-*`). brain-http doesn't enable
/// any of those today, so the stream is always the `Plain` variant;
/// the type alias just preserves the option for the future.
pub struct Connected {
    /// The WebSocket frame stream — ready to `send`/`next`.
    pub stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// The raw 101 Switching Protocols response from the server.
    pub response: HttpResponse<Option<Vec<u8>>>,
}

/// Connect to a WebSocket server with default settings (no custom
/// headers, no timeout).
///
/// For custom headers or a connect timeout, use
/// [`ConnectBuilder`].
///
/// # Errors
///
/// - [`crate::Error::Upgrade`] on handshake failure (bad status,
///   malformed `Sec-WebSocket-Accept`, malformed URL).
/// - [`crate::Error::Io`] on socket-level failure.
///
/// # Caveat: reconnect storms
///
/// If you loop `connect` on failure with no backoff, you'll hammer
/// the server. brain-http does not provide a `connect_with_backoff`
/// helper because that's per-consumer policy — pick a policy and
/// apply it at the call site.
pub async fn connect(url: &str) -> crate::Result<Connected> {
    ConnectBuilder::new(url).connect().await
}

/// Builder for a WebSocket client connection.
///
/// All knobs default to "off"; the builder only matters when you
/// want a custom header or a connect timeout. Otherwise call
/// [`connect`] directly.
pub struct ConnectBuilder<'a> {
    url: &'a str,
    headers: Vec<(HeaderName, HeaderValue)>,
    connect_timeout: Option<Duration>,
}

impl<'a> ConnectBuilder<'a> {
    /// Start a connection to `url`. URL scheme must be `ws://` or
    /// (when TLS is wired) `wss://`.
    #[must_use]
    pub fn new(url: &'a str) -> Self {
        Self {
            url,
            headers: Vec::new(),
            connect_timeout: None,
        }
    }

    /// Add a header to the HTTP/1.1 Upgrade request. Common uses:
    /// `Authorization: Bearer …`, `User-Agent: my-app/1.0`,
    /// `X-Request-ID: <uuid>`.
    ///
    /// Don't override `Host`, `Upgrade`, `Connection`, or any
    /// `Sec-WebSocket-*` header — tungstenite owns those and a
    /// custom value produces a malformed handshake.
    #[must_use]
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }

    /// Wall-clock cap on connect + handshake. Defaults to no timeout.
    #[must_use]
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = Some(d);
        self
    }

    /// Drive the connection.
    ///
    /// # Errors
    ///
    /// See [`connect`] for the error taxonomy. Adds
    /// [`crate::Error::Timeout`] when the builder-configured timeout
    /// fires.
    pub async fn connect(self) -> crate::Result<Connected> {
        let mut req = self
            .url
            .into_client_request()
            .map_err(|e| crate::Error::Upgrade(format!("bad url: {e}")))?;
        for (name, value) in self.headers {
            req.headers_mut().append(name, value);
        }
        let fut = tokio_tungstenite::connect_async(req);
        let (stream, response) = match self.connect_timeout {
            Some(d) => tokio::time::timeout(d, fut)
                .await
                .map_err(|_| crate::Error::Timeout(d))?
                .map_err(map_ws_error)?,
            None => fut.await.map_err(map_ws_error)?,
        };
        Ok(Connected { stream, response })
    }
}

fn map_ws_error(e: tokio_tungstenite::tungstenite::Error) -> crate::Error {
    use tokio_tungstenite::tungstenite::Error as Te;
    match e {
        Te::Io(io) => crate::Error::Io(io),
        other => crate::Error::Upgrade(other.to_string()),
    }
}
