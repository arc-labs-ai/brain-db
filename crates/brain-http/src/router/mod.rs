//! HTTP request router.
//!
//! Match-based dispatch with exact / prefix / fallback. Designed for
//! Brain's ~15-route admin surface; if route count ever climbs past
//! ~30 we revisit with a radix trie. Anything fancier (typed
//! extractors, middleware layers) is over-tooled for the use case.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};

use crate::body::{full, ResponseBody};
use crate::error::status_for_error;

mod matcher;

use matcher::{match_route, ExactSpec, MatchOutcome, PrefixSpec};

/// A boxed async handler stored inside the router's dispatch table.
/// One per route — exact and prefix lists hold separate `Vec`s so
/// match indices line up.
type BoxedAsyncHandler<B> = Box<
    dyn Fn(
            Request<B>,
        ) -> Pin<Box<dyn Future<Output = crate::Result<Response<ResponseBody>>> + Send>>
        + Send
        + Sync,
>;

/// HTTP request router. Generic over the inbound body type so unit
/// tests can drive routing with synthetic bodies (e.g. `Full<Bytes>`)
/// without spinning up a real listener; the production server uses
/// `hyper::body::Incoming` here.
pub struct Router<B>
where
    B: Send + 'static,
{
    exact_specs: Vec<ExactSpec>,
    exact_handlers: Vec<BoxedAsyncHandler<B>>,
    prefix_specs: Vec<PrefixSpec>,
    prefix_handlers: Vec<BoxedAsyncHandler<B>>,
    fallback: Option<BoxedAsyncHandler<B>>,
}

impl<B> Default for Router<B>
where
    B: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<B> Router<B>
where
    B: Send + 'static,
{
    /// Construct an empty router. Routes are added via `get` / `post`
    /// / `delete` / `route` / `route_prefix` / `fallback`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            exact_specs: Vec::new(),
            exact_handlers: Vec::new(),
            prefix_specs: Vec::new(),
            prefix_handlers: Vec::new(),
            fallback: None,
        }
    }

    /// Register an exact-match route. `path` must start with `/`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `path` does not start with `/`.
    #[must_use]
    pub fn route<H, Fut>(mut self, method: Method, path: &'static str, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        debug_assert!(path.starts_with('/'), "exact paths must start with /");
        self.exact_specs.push(ExactSpec { method, path });
        self.exact_handlers.push(wrap(handler));
        self
    }

    /// Register a prefix-match route. `prefix` must start with `/`.
    /// Handlers parse any remaining path segments themselves.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `prefix` does not start with `/`.
    #[must_use]
    pub fn route_prefix<H, Fut>(mut self, method: Method, prefix: &'static str, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        debug_assert!(prefix.starts_with('/'), "prefixes must start with /");
        self.prefix_specs.push(PrefixSpec { method, prefix });
        self.prefix_handlers.push(wrap(handler));
        self
    }

    /// `GET` exact-match shortcut.
    #[must_use]
    pub fn get<H, Fut>(self, path: &'static str, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        self.route(Method::GET, path, handler)
    }

    /// `POST` exact-match shortcut.
    #[must_use]
    pub fn post<H, Fut>(self, path: &'static str, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        self.route(Method::POST, path, handler)
    }

    /// `DELETE` exact-match shortcut.
    #[must_use]
    pub fn delete<H, Fut>(self, path: &'static str, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        self.route(Method::DELETE, path, handler)
    }

    /// Register a fallback handler. Returns 404 when no route matched
    /// (and no method mismatch surfaced 405) and no fallback is set.
    #[must_use]
    pub fn fallback<H, Fut>(mut self, handler: H) -> Self
    where
        H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
    {
        self.fallback = Some(wrap(handler));
        self
    }

    /// Dispatch one request. Returns the handler's response, or a
    /// canned 404 / 405 when no route matches.
    ///
    /// Handler errors are mapped to canonical status-code responses
    /// via [`crate::error::status_for_error`]; this method itself
    /// never returns `Err` — the surrounding server can treat its
    /// result as the wire response unconditionally.
    ///
    /// Returns a boxed `Send` future so the surrounding hyper service
    /// is `Send` (required for `tokio::spawn` of the connection task).
    /// The compiler can't infer `Send` through `BoxedAsyncHandler<B>`'s
    /// trait-object call site reliably, so the bound is explicit here.
    pub fn dispatch<'a>(
        &'a self,
        req: Request<B>,
    ) -> Pin<Box<dyn Future<Output = Response<ResponseBody>> + Send + 'a>> {
        Box::pin(async move {
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            let outcome = match_route(&self.exact_specs, &self.prefix_specs, &method, &path);

            let result = match outcome {
                MatchOutcome::Exact(i) => (self.exact_handlers[i])(req).await,
                MatchOutcome::Prefix(i) => (self.prefix_handlers[i])(req).await,
                MatchOutcome::MethodMismatch => {
                    return canned(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
                }
                MatchOutcome::None => match &self.fallback {
                    Some(h) => h(req).await,
                    None => return canned(StatusCode::NOT_FOUND, "not found\n"),
                },
            };

            match result {
                Ok(resp) => resp,
                // Escape the message so one containing `"`, `\`, or a
                // control char can't produce malformed JSON.
                Err(e) => {
                    let body = format!("{{\"error\":\"{}\"}}\n", json_escape(&e.to_string()));
                    canned_json(status_for_error(&e), &body)
                }
            }
        })
    }
}

fn wrap<B, H, Fut>(handler: H) -> BoxedAsyncHandler<B>
where
    H: Fn(Request<B>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::Result<Response<ResponseBody>>> + Send + 'static,
{
    Box::new(move |req| Box::pin(handler(req)))
}

fn canned(status: StatusCode, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(Bytes::copy_from_slice(body.as_bytes())))
        .expect("static response always builds")
}

fn canned_json(status: StatusCode, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(full(Bytes::copy_from_slice(body.as_bytes())))
        .expect("static response always builds")
}

/// Escape a string for embedding inside a JSON string literal. Handles the
/// two structural characters (`"`, `\`) plus the control characters JSON
/// forbids unescaped, so an arbitrary error `Display` can never break the
/// surrounding document.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    async fn ok_handler(_req: Request<Full<Bytes>>) -> crate::Result<Response<ResponseBody>> {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(full(Bytes::from_static(b"ok")))
            .unwrap())
    }

    async fn echo_path(req: Request<Full<Bytes>>) -> crate::Result<Response<ResponseBody>> {
        let path = req.uri().path().to_owned();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(full(Bytes::from(path)))
            .unwrap())
    }

    fn req(method: Method, path: &'static str) -> Request<Full<Bytes>> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn collect(resp: Response<ResponseBody>) -> (StatusCode, Bytes) {
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes();
        (status, body)
    }

    #[tokio::test]
    async fn exact_route_dispatches_to_its_handler() {
        let r = Router::<Full<Bytes>>::new().get("/healthz", ok_handler);
        let (status, body) = collect(r.dispatch(req(Method::GET, "/healthz")).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"ok");
    }

    #[tokio::test]
    async fn prefix_dispatch_passes_full_path() {
        let r =
            Router::<Full<Bytes>>::new().route_prefix(Method::POST, "/v1/snapshots/", echo_path);
        let (status, body) = collect(
            r.dispatch(req(Method::POST, "/v1/snapshots/abc/delete"))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"/v1/snapshots/abc/delete");
    }

    #[tokio::test]
    async fn no_route_returns_404() {
        let r = Router::<Full<Bytes>>::new();
        let (status, _) = collect(r.dispatch(req(Method::GET, "/missing")).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn method_mismatch_returns_405() {
        let r = Router::<Full<Bytes>>::new().post("/v1/snap", ok_handler);
        let (status, _) = collect(r.dispatch(req(Method::GET, "/v1/snap")).await).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn fallback_fires_when_no_match() {
        let r = Router::<Full<Bytes>>::new().fallback(ok_handler);
        let (status, body) = collect(r.dispatch(req(Method::GET, "/missing")).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"ok");
    }

    #[tokio::test]
    async fn exact_wins_over_prefix() {
        let r = Router::<Full<Bytes>>::new()
            .get("/v1/snap", ok_handler)
            .route_prefix(Method::GET, "/v1/", echo_path);
        let (status, body) = collect(r.dispatch(req(Method::GET, "/v1/snap")).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"ok"); // exact handler, not echo
    }

    #[tokio::test]
    async fn handler_error_becomes_status_response() {
        async fn explode(_req: Request<Full<Bytes>>) -> crate::Result<Response<ResponseBody>> {
            Err(crate::Error::Server(StatusCode::SERVICE_UNAVAILABLE))
        }
        let r = Router::<Full<Bytes>>::new().get("/v1/down", explode);
        let (status, body) = collect(r.dispatch(req(Method::GET, "/v1/down")).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.starts_with(b"{\"error\":"));
    }

    #[test]
    fn json_escape_handles_structural_chars() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("line1\nline2\t"), "line1\\nline2\\t");
        assert_eq!(json_escape("\u{01}"), "\\u0001");
        assert_eq!(json_escape("plain"), "plain");
    }

    #[tokio::test]
    async fn handler_error_with_quote_produces_valid_json() {
        // An error Display containing `"` / `\` / newline must not break
        // out of the JSON error document. (brain-http has no JSON parser
        // dependency, so we assert well-formedness structurally.)
        async fn explode(_req: Request<Full<Bytes>>) -> crate::Result<Response<ResponseBody>> {
            Err(crate::Error::Upgrade("say \"hi\"\nboom".to_string()))
        }
        let r = Router::<Full<Bytes>>::new().get("/v1/boom", explode);
        let (_status, body) = collect(r.dispatch(req(Method::GET, "/v1/boom")).await).await;
        let text = std::str::from_utf8(&body).expect("utf8");

        // Exactly the escaped document the dispatcher promises to emit.
        let display = crate::Error::Upgrade("say \"hi\"\nboom".to_string()).to_string();
        let expected = format!("{{\"error\":\"{}\"}}\n", json_escape(&display));
        assert_eq!(text, expected);

        // The value portion carries no raw newline and no bare quote —
        // both would corrupt the JSON.
        let inner = text
            .trim_end()
            .strip_prefix("{\"error\":\"")
            .and_then(|s| s.strip_suffix("\"}"))
            .expect("well-formed error envelope");
        assert!(!inner.contains('\n'), "raw newline leaked: {text}");
        // Every quote inside the value is backslash-escaped.
        let bytes = inner.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                assert!(i > 0 && bytes[i - 1] == b'\\', "bare quote at {i}: {text}");
            }
        }
    }
}
