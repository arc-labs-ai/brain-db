//! `GET /healthz` handler.
//!
//! Liveness probe. Always replies `200 OK\nok\n`; the admin server
//! is "healthy" iff the accept loop is running.

use brain_http::body::{full, ResponseBody};
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

pub async fn handle(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(Bytes::from_static(b"ok\n")))
        .expect("static response always builds"))
}
