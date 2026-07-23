//! Criterion bench for `Router::dispatch` — pure routing throughput.
//!
//! No TCP, no hyper. Measures the dispatch overhead in isolation so
//! later instrumentation changes can be compared against this
//! baseline.

use std::sync::Arc;

use brain_http::body::{full, ResponseBody};
use brain_http::router::Router;
use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use tokio::runtime::Runtime;

async fn ok_handler(_req: Request<Full<Bytes>>) -> brain_http::Result<Response<ResponseBody>> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(full(Bytes::from_static(b"ok")))
        .unwrap())
}

fn build_router() -> Router<Full<Bytes>> {
    // 10 routes — 5 exact, 4 prefix, 1 fallback.
    Router::new()
        .get("/healthz", ok_handler)
        .get("/v1/route/1", ok_handler)
        .get("/v1/route/2", ok_handler)
        .get("/v1/route/3", ok_handler)
        .get("/v1/route/5", ok_handler)
        .route_prefix(Method::POST, "/v1/snapshots/", ok_handler)
        .route_prefix(Method::POST, "/v1/workers/", ok_handler)
        .route_prefix(Method::GET, "/v1/spaces/", ok_handler)
        .route_prefix(Method::DELETE, "/v1/shards/", ok_handler)
        .fallback(ok_handler)
}

fn req(method: Method, uri: &'static str) -> Request<Full<Bytes>> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn bench_router(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio rt");
    let router = Arc::new(build_router());

    let r = router.clone();
    c.bench_function("router_exact_hit", |b| {
        b.to_async(&rt).iter(|| {
            let r = r.clone();
            async move {
                let _ = r.dispatch(req(Method::GET, "/v1/route/5")).await;
            }
        });
    });

    let r = router.clone();
    c.bench_function("router_prefix_hit", |b| {
        b.to_async(&rt).iter(|| {
            let r = r.clone();
            async move {
                let _ = r.dispatch(req(Method::POST, "/v1/snapshots/42")).await;
            }
        });
    });

    let r = router.clone();
    c.bench_function("router_miss_fallback", |b| {
        b.to_async(&rt).iter(|| {
            let r = r.clone();
            async move {
                let _ = r.dispatch(req(Method::GET, "/nope/not/here")).await;
            }
        });
    });
}

criterion_group!(benches, bench_router);
criterion_main!(benches);
