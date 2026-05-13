//! `brain-cli shard` integration tests. `list` is backed;
//! create/delete return 501.

mod support;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::shard::{create, delete, list};
use support::{not_implemented_body, spawn_mock};

#[test]
fn list_json_round_trip() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/shards");
        (
            200,
            r#"{"shards":[{"index":0,"shard_id":0},{"index":1,"shard_id":1}]}"#.into(),
        )
    });
    let out = list::run(&addr.to_string(), OutputFormat::Json).expect("list");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["shards"][1]["shard_id"], 1);
}

#[test]
fn list_table_output() {
    let addr = spawn_mock(|_m, _p, _b| (200, r#"{"shards":[{"index":0,"shard_id":0}]}"#.into()));
    let out = list::run(&addr.to_string(), OutputFormat::Table).expect("list");
    assert!(out.contains("index 0"));
    assert!(out.contains("shard_id=0"));
}

#[test]
fn create_surfaces_501() {
    let addr = spawn_mock(|method, path, body| {
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/shards");
        assert!(body.contains("logical_id"));
        (501, not_implemented_body("phase-12/shard-create", "x"))
    });
    let err = create::run(&addr.to_string(), 16).expect_err("err");
    assert!(err.to_string().contains("phase-12/shard-create"));
}

#[test]
fn delete_requires_confirm() {
    let addr = spawn_mock(|_m, _p, _b| (200, "{}".into()));
    let err = delete::run(&addr.to_string(), "abc", false).expect_err("err");
    assert!(err.to_string().contains("--confirm"));
}

#[test]
fn delete_with_confirm_hits_server_then_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "DELETE");
        assert_eq!(path, "/v1/shards/abc");
        (501, not_implemented_body("phase-12/shard-delete", "x"))
    });
    let err = delete::run(&addr.to_string(), "abc", true).expect_err("err");
    assert!(err.to_string().contains("phase-12/shard-delete"));
}
