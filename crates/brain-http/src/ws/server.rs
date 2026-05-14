//! Upgrade plumbing: hyper `Upgraded` → tokio-tungstenite
//! `WebSocketStream`.

use http::{Request, Response};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::WebSocketStream;

use crate::body::ResponseBody;
use crate::ws::upgrade;

/// Future-like wrapper that yields a fully-upgraded
/// [`WebSocketStream`] once the peer is on the WebSocket side of the
/// protocol switch.
///
/// **Drive this inside `tokio::spawn`.** Hyper requires the 101
/// response to have been written before the upgrade future
/// resolves. If you `.await` it inline in the handler, hyper deadlocks
/// — there's no one to send the 101.
pub struct OnUpgrade {
    inner: hyper::upgrade::OnUpgrade,
}

impl OnUpgrade {
    /// Drive the upgrade. Returns the ready-to-use
    /// `WebSocketStream`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Upgrade`] if hyper's upgrade future
    /// fails (typically: peer closed before the 101 was acknowledged).
    pub async fn await_upgrade(
        self,
    ) -> crate::Result<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>> {
        let upgraded = self
            .inner
            .await
            .map_err(|e| crate::Error::Upgrade(format!("hyper upgrade: {e}")))?;
        let stream = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            None, // default WebSocketConfig: max_msg=64 MiB, max_frame=16 MiB, mask-strict
        )
        .await;
        Ok(stream)
    }
}

/// Accept a WebSocket upgrade request.
///
/// Returns `(response, on_upgrade)`:
/// - `response` is the `101 Switching Protocols` the handler returns
///   to brain-http's router immediately.
/// - `on_upgrade` is the future to drive **inside a spawned task**
///   to obtain the `WebSocketStream`.
///
/// Typical usage:
///
/// ```ignore
/// async fn handler(
///     req: http::Request<hyper::body::Incoming>,
/// ) -> brain_http::Result<http::Response<brain_http::body::ResponseBody>> {
///     let (response, on_upgrade) = brain_http::ws::accept(req)?;
///     tokio::spawn(async move {
///         match on_upgrade.await_upgrade().await {
///             Ok(mut ws) => {
///                 use futures_util::{SinkExt, StreamExt};
///                 while let Some(Ok(msg)) = ws.next().await {
///                     if msg.is_text() || msg.is_binary() {
///                         let _ = ws.send(msg).await;
///                     }
///                 }
///             }
///             Err(e) => tracing::warn!(error = %e, "ws upgrade failed"),
///         }
///     });
///     Ok(response)
/// }
/// ```
///
/// # Errors
///
/// Returns [`crate::Error::Upgrade`] if the request headers don't
/// satisfy RFC 6455's upgrade requirements.
pub fn accept(req: Request<Incoming>) -> crate::Result<(Response<ResponseBody>, OnUpgrade)> {
    let response = upgrade::validate_and_respond(&req)?;
    let on_upgrade = OnUpgrade {
        inner: hyper::upgrade::on(req),
    };
    Ok((response, on_upgrade))
}
