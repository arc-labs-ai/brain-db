//! Service trait surface.
//!
//! brain-http uses [`hyper::service::Service`] directly — it's the
//! same trait `tower` defines, and the version-neutral handler shape
//! the whole Rust HTTP ecosystem agrees on.
//!
//! This module exposes:
//!
//! - A re-export of [`service_fn`] for wrapping closures.
//! - The [`AsyncHandler`] trait — the ergonomic `async fn` shape Brain
//!   handlers will use, with an adapter to `hyper::Service`.
//! - A [`BoxedService`] type alias used by the router.

pub use hyper::service::{service_fn, Service};

mod handler;
pub use handler::AsyncHandler;

use std::future::Future;
use std::pin::Pin;

/// Boxed `Service` for storing heterogeneous handlers in a router
/// dispatch table.
///
/// The router keeps a `Vec<(Method, &str, BoxedService<...>)>`
/// and selects by `(method, path)`.
pub type BoxedService<Req, Res> = Box<
    dyn Service<
            Req,
            Response = Res,
            Error = crate::Error,
            Future = Pin<Box<dyn Future<Output = crate::Result<Res>> + Send>>,
        > + Send
        + Sync,
>;
