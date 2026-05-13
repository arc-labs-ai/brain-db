//! `brain-cli worker` integration tests against a mock admin
//! server.

mod support;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::worker::{list, run_now, start, stop};
use support::{not_implemented_body, spawn_mock};

#[test]
fn list_json_round_trip() {
    let addr = spawn_mock(|method, path, _body| {
        assert_eq!(method, "GET");
        assert!(path.starts_with("/v1/workers"));
        let body = r#"{"workers":[{"shard":0,"name":"decay","cycles":3,"processed":12,"errors":0,"last_run_unix":1700000000}]}"#;
        (200, body.into())
    });
    let out = list::run(&addr.to_string(), None, OutputFormat::Json).expect("list");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["workers"][0]["name"], "decay");
    assert_eq!(v["workers"][0]["cycles"], 3);
}

#[test]
fn list_with_shard_query() {
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let cap2 = captured.clone();
    let addr = spawn_mock(move |_m, path, _b| {
        *cap2.lock().unwrap() = Some(path.to_string());
        (200, r#"{"workers":[]}"#.into())
    });
    let _ = list::run(&addr.to_string(), Some(2), OutputFormat::Json).expect("list");
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some("/v1/workers?shard=2")
    );
}

#[test]
fn stop_surfaces_501() {
    let addr = spawn_mock(|_m, _p, _b| {
        (
            501,
            not_implemented_body("phase-11/scheduler-control", "deferred"),
        )
    });
    let err = stop::run(&addr.to_string(), "decay", 0, OutputFormat::Table).expect_err("err");
    let msg = err.to_string();
    assert!(msg.contains("Not yet implemented"));
    assert!(msg.contains("phase-11/scheduler-control"));
}

#[test]
fn start_surfaces_501() {
    let addr =
        spawn_mock(|_m, _p, _b| (501, not_implemented_body("phase-11/scheduler-control", "x")));
    let err = start::run(&addr.to_string(), "decay", 0, OutputFormat::Table).expect_err("err");
    assert!(err.to_string().contains("Not yet implemented"));
}

#[test]
fn run_now_surfaces_501() {
    let addr =
        spawn_mock(|_m, _p, _b| (501, not_implemented_body("phase-11/scheduler-control", "x")));
    let err = run_now::run(&addr.to_string(), "decay", 0, OutputFormat::Table).expect_err("err");
    assert!(err.to_string().contains("Not yet implemented"));
}
