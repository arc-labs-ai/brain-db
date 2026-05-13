//! `brain-cli agent` integration tests. All actions deferred.

mod support;

use brain_cli::commands::agent::{delete, list, stats};
use support::{not_implemented_body, spawn_mock};

#[test]
fn list_surfaces_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert!(path.starts_with("/v1/agents"));
        (501, not_implemented_body("phase-11/agent-index", "x"))
    });
    let err = list::run(&addr.to_string(), None).expect_err("err");
    assert!(err.to_string().contains("phase-11/agent-index"));
}

#[test]
fn stats_surfaces_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/agents/abc");
        (501, not_implemented_body("phase-11/agent-index", "x"))
    });
    let err = stats::run(&addr.to_string(), "abc").expect_err("err");
    assert!(err.to_string().contains("phase-11/agent-index"));
}

#[test]
fn delete_requires_confirm() {
    // No HTTP request made — guard rejects before hitting the wire.
    let addr = spawn_mock(|_m, _p, _b| (200, "{}".into()));
    let err = delete::run(&addr.to_string(), "abc", false).expect_err("err");
    assert!(err.to_string().contains("--confirm"));
}

#[test]
fn delete_with_confirm_hits_server_then_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/v1/agents/abc");
        (
            501,
            not_implemented_body("phase-11/agent-cascade-delete", "x"),
        )
    });
    let err = delete::run(&addr.to_string(), "abc", true).expect_err("err");
    assert!(err.to_string().contains("phase-11/agent-cascade-delete"));
}
