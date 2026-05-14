//! WebSocket upgrade-request validation + `101 Switching Protocols`
//! response builder.
//!
//! Per RFC 6455 §4.2.1 the request must satisfy:
//!
//! - Method = `GET`
//! - HTTP version = `HTTP/1.1` (HTTP/2 has RFC 8441; not supported)
//! - `Upgrade: websocket`
//! - `Connection: Upgrade` (may be one token in a comma-separated list)
//! - `Sec-WebSocket-Version: 13`
//! - `Sec-WebSocket-Key: <base64>`

use http::{HeaderMap, Method, Request, Response, StatusCode, Version};

use crate::body::{empty, ResponseBody};
use crate::ws::accept_key;

/// Validate a request's WebSocket upgrade headers and build the
/// matching `101 Switching Protocols` response.
///
/// Returns the 101 response. The caller pairs this with
/// `hyper::upgrade::on(req)` to drive the WS protocol — see
/// [`crate::ws::server::accept`].
///
/// # Errors
///
/// Returns [`crate::Error::Upgrade`] if any required header is
/// missing or malformed.
pub fn validate_and_respond<B>(req: &Request<B>) -> crate::Result<Response<ResponseBody>> {
    if req.method() != Method::GET {
        return Err(crate::Error::Upgrade("method must be GET".into()));
    }
    if req.version() != Version::HTTP_11 {
        return Err(crate::Error::Upgrade("HTTP/1.1 required".into()));
    }
    let headers = req.headers();
    check_header(headers, "upgrade", b"websocket")?;
    check_connection_upgrade(headers)?;
    check_header(headers, "sec-websocket-version", b"13")?;
    let key = headers
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| crate::Error::Upgrade("missing sec-websocket-key".into()))?;
    if key.is_empty() {
        return Err(crate::Error::Upgrade("empty sec-websocket-key".into()));
    }

    let accept = accept_key::derive(key);
    let resp = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", accept)
        .body(empty())
        .map_err(crate::Error::Http)?;
    Ok(resp)
}

fn check_header(h: &HeaderMap, name: &str, expected: &[u8]) -> crate::Result<()> {
    let actual = h
        .get(name)
        .ok_or_else(|| crate::Error::Upgrade(format!("missing {name}")))?
        .as_bytes();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(crate::Error::Upgrade(format!("invalid {name}")))
    }
}

/// `Connection` may be a comma-separated list (e.g.
/// `keep-alive, Upgrade`). Accept if any token equals `upgrade`
/// case-insensitively.
fn check_connection_upgrade(h: &HeaderMap) -> crate::Result<()> {
    let v = h
        .get("connection")
        .ok_or_else(|| crate::Error::Upgrade("missing connection".into()))?
        .to_str()
        .map_err(|_| crate::Error::Upgrade("non-ascii connection header".into()))?;
    if v.split(',')
        .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    {
        Ok(())
    } else {
        Err(crate::Error::Upgrade(
            "connection must include `upgrade`".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Empty;

    fn valid_request() -> http::request::Builder {
        Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("host", "localhost")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
    }

    #[test]
    fn valid_request_yields_101() {
        let req = valid_request().body(Empty::<bytes::Bytes>::new()).unwrap();
        let resp = validate_and_respond(&req).expect("validate");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            resp.headers().get("sec-websocket-accept").unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn wrong_method_rejected() {
        let req = valid_request()
            .method(Method::POST)
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let err = validate_and_respond(&req).expect_err("err");
        assert!(matches!(err, crate::Error::Upgrade(_)));
    }

    #[test]
    fn missing_key_rejected() {
        let req = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("host", "localhost")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-version", "13")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let err = validate_and_respond(&req).expect_err("err");
        assert!(matches!(err, crate::Error::Upgrade(_)));
    }

    #[test]
    fn wrong_version_rejected() {
        let req = valid_request()
            .header("sec-websocket-version", "12")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        // Note: setting the header again ADDS a value; we need to
        // override. The header crate appends duplicates. Build a
        // request without the matching `13` value instead.
        let req2 = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("host", "localhost")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-version", "12")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let _ = req;
        let err = validate_and_respond(&req2).expect_err("err");
        assert!(matches!(err, crate::Error::Upgrade(_)));
    }

    #[test]
    fn connection_with_upgrade_in_list_accepted() {
        let req = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("host", "localhost")
            .header("upgrade", "websocket")
            .header("connection", "keep-alive, Upgrade")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp = validate_and_respond(&req).expect("validate");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    }

    #[test]
    fn missing_upgrade_rejected() {
        let req = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("host", "localhost")
            .header("connection", "Upgrade")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let err = validate_and_respond(&req).expect_err("err");
        assert!(matches!(err, crate::Error::Upgrade(_)));
    }
}
