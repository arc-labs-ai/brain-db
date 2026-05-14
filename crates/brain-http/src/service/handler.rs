//! The `AsyncHandler` trait — Brain's ergonomic handler shape.
//!
//! Generic over the inbound body type so handlers can be tested
//! against synthetic bodies (e.g. `Full<Bytes>`) without needing a
//! real `hyper::body::Incoming`. This was the M1 plan §8.1 open
//! question; the recommendation was generic-over-Body and that's
//! what's implemented here.

use std::future::Future;

use http::{Request, Response};

use crate::body::ResponseBody;

/// What every Brain HTTP handler implements.
///
/// The signature differs from [`hyper::service::Service::call`] only
/// in that it's a plain `async fn`. M2's router adapts any
/// `AsyncHandler` impl to a `hyper::Service` so it can be wired into
/// `hyper::server::conn::http1::Builder::serve_connection`.
///
/// `B` is the inbound body type — typically [`hyper::body::Incoming`]
/// at runtime, [`http_body_util::Full<bytes::Bytes>`] in unit tests.
pub trait AsyncHandler<B>: Send + Sync + 'static
where
    B: Send + 'static,
{
    /// Future returned by [`call`](AsyncHandler::call).
    type Future: Future<Output = crate::Result<Response<ResponseBody>>> + Send;

    /// Invoke the handler on a request.
    fn call(&self, req: Request<B>) -> Self::Future;
}
