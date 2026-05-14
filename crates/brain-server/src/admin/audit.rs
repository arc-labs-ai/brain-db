//! Admin HTTP handlers for `audit` (spec §14/06 §8; sub-task 10.11).
//!
//! Both deferred — no audit-log primitive exists yet.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response};
use hyper::body::Incoming;

use crate::admin::util::not_implemented;
use crate::admin::AdminState;

/// `GET /v1/audit?...` handler — deferred.
pub async fn query(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/audit-log",
        "audit-log query and export pathway",
    ))
}

/// `GET /v1/audit/export` handler — deferred.
pub async fn export(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/audit-log",
        "audit-log query and export pathway",
    ))
}
