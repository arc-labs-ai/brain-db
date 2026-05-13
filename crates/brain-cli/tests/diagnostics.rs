//! `brain-cli profile` + `debug-snapshot` integration tests.

mod support;

use std::fs;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::diagnostics::{debug_snapshot, profile};
use support::{not_implemented_body, spawn_mock};
use tempfile::TempDir;

const DEBUG_BODY: &str = r#"{"shard":0,"captured_at_unix":1700000000,"partial":true,"deferred":["active_tasks","pending_requests","recent_errors","in_memory_state_summary"],"workers":[{"name":"decay","cycles":3,"processed":12,"errors":0,"last_run_unix":1699999000}]}"#;

#[test]
fn debug_snapshot_json_round_trip() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert!(path.starts_with("/v1/diagnostics/debug-snapshot"));
        (200, DEBUG_BODY.into())
    });
    let out = debug_snapshot::run(&addr.to_string(), 0, None, OutputFormat::Json).expect("snap");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["partial"], true);
    assert_eq!(v["workers"][0]["name"], "decay");
    assert_eq!(v["deferred"][0], "active_tasks");
}

#[test]
fn debug_snapshot_table_lists_deferred() {
    let addr = spawn_mock(|_m, _p, _b| (200, DEBUG_BODY.into()));
    let out = debug_snapshot::run(&addr.to_string(), 0, None, OutputFormat::Table).expect("snap");
    assert!(out.contains("partial"));
    assert!(out.contains("active_tasks"));
    assert!(out.contains("pending_requests"));
    assert!(out.contains("worker decay"));
}

#[test]
fn debug_snapshot_writes_to_path() {
    let addr = spawn_mock(|_m, _p, _b| (200, DEBUG_BODY.into()));
    let tmp = TempDir::new().expect("tmp");
    let out_path = tmp.path().join("snap.json");
    let out_str = out_path.to_string_lossy().to_string();
    let _ = debug_snapshot::run(&addr.to_string(), 0, Some(&out_str), OutputFormat::Json)
        .expect("snap");
    let on_disk = fs::read_to_string(&out_path).expect("read");
    let v: serde_json::Value = serde_json::from_str(on_disk.trim()).unwrap();
    assert_eq!(v["shard"], 0);
    assert_eq!(v["workers"][0]["cycles"], 3);
}

#[test]
fn debug_snapshot_threads_shard_in_query() {
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let c2 = captured.clone();
    let addr = spawn_mock(move |_m, path, _b| {
        *c2.lock().unwrap() = Some(path.to_string());
        (200, DEBUG_BODY.into())
    });
    let _ = debug_snapshot::run(&addr.to_string(), 3, None, OutputFormat::Json).expect("snap");
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some("/v1/diagnostics/debug-snapshot?shard=3")
    );
}

#[test]
fn profile_surfaces_501() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "POST");
        assert!(path.starts_with("/v1/diagnostics/profile"));
        (
            501,
            not_implemented_body("phase-11/glommio-profiler", "deferred"),
        )
    });
    let err = profile::run(&addr.to_string(), 0, 1, None).expect_err("err");
    let msg = err.to_string();
    assert!(msg.contains("Not yet implemented"));
    assert!(msg.contains("phase-11/glommio-profiler"));
}

#[test]
fn profile_passes_duration_in_query() {
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let c2 = captured.clone();
    let addr = spawn_mock(move |_m, path, _b| {
        *c2.lock().unwrap() = Some(path.to_string());
        (501, not_implemented_body("phase-11/glommio-profiler", "x"))
    });
    let _ = profile::run(&addr.to_string(), 2, 7, None);
    let got = captured.lock().unwrap().clone().unwrap_or_default();
    assert!(got.contains("shard=2"));
    assert!(got.contains("duration_secs=7"));
}
