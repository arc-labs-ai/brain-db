//! `POST /v1/workers/{name}/{action}` — deferred control plane.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::handlers::worker::{KNOWN_ACTIONS, KNOWN_WORKERS};
use crate::admin::util::{not_implemented, text_response};
use crate::admin::AdminState;

pub async fn control(
    req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    if req.method() != Method::POST {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        ));
    }
    let path = req.uri().path();
    let Some(rest) = path.strip_prefix("/v1/workers/") else {
        return Ok(text_response(
            StatusCode::NOT_FOUND,
            "worker route not found\n",
        ));
    };
    let mut parts = rest.splitn(2, '/');
    let name = parts.next().unwrap_or("");
    let action = parts.next().unwrap_or("");

    if !KNOWN_WORKERS.contains(&name) {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown worker `{name}`\n"),
        ));
    }
    if !KNOWN_ACTIONS.contains(&action) {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown worker action `{action}`\n"),
        ));
    }
    Ok(not_implemented(
        "phase-11/scheduler-control",
        "live worker pause/resume/trigger",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_action_set() {
        assert!(KNOWN_ACTIONS.contains(&"stop"));
        assert!(KNOWN_ACTIONS.contains(&"start"));
        assert!(KNOWN_ACTIONS.contains(&"run-now"));
    }
}
