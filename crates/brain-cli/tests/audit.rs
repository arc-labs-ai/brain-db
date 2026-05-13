//! `brain-cli audit` integration tests. Both actions deferred.

mod support;

use brain_cli::commands::audit::{export, query};
use support::{not_implemented_body, spawn_mock};

#[test]
fn query_surfaces_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert!(path.starts_with("/v1/audit"));
        (501, not_implemented_body("phase-11/audit-log", "x"))
    });
    let err = query::run(
        &addr.to_string(),
        Some("2026-05-01"),
        Some("2026-05-07"),
        Some("agent-001"),
    )
    .expect_err("err");
    assert!(err.to_string().contains("phase-11/audit-log"));
}

#[test]
fn query_threads_filters() {
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let c2 = captured.clone();
    let addr = spawn_mock(move |_m, path, _b| {
        *c2.lock().unwrap() = Some(path.to_string());
        (501, not_implemented_body("phase-11/audit-log", "x"))
    });
    let _ = query::run(
        &addr.to_string(),
        Some("2026-05-01"),
        None,
        Some("agent-001"),
    );
    let got = captured.lock().unwrap().clone().unwrap_or_default();
    assert!(got.contains("since=2026-05-01"));
    assert!(got.contains("agent=agent-001"));
}

#[test]
fn export_surfaces_501() {
    let addr = spawn_mock(|_m, path, _b| {
        assert_eq!(path, "/v1/audit/export");
        (501, not_implemented_body("phase-11/audit-log", "x"))
    });
    let err = export::run(&addr.to_string(), None).expect_err("err");
    assert!(err.to_string().contains("phase-11/audit-log"));
}
