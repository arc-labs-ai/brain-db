//! `brain-cli config` integration tests.

mod support;

use brain_cli::cli::OutputFormat;
use brain_cli::commands::config::{get, reload, set};
use support::{not_implemented_body, spawn_mock};

#[test]
fn get_full_config_round_trip() {
    let addr = spawn_mock(|method, path, _b| {
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/config");
        (200, r#"{"server":{"listen_addr":"127.0.0.1:9090"}}"#.into())
    });
    let out = get::run(&addr.to_string(), None, OutputFormat::Json).expect("get");
    assert!(out.contains("listen_addr"));
}

#[test]
fn get_by_key_threads_query() {
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let c2 = captured.clone();
    let addr = spawn_mock(move |_m, path, _b| {
        *c2.lock().unwrap() = Some(path.to_string());
        (200, r#""127.0.0.1:9090""#.into())
    });
    let _ = get::run(
        &addr.to_string(),
        Some("server.listen_addr"),
        OutputFormat::Json,
    )
    .expect("get");
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some("/v1/config?key=server.listen_addr")
    );
}

#[test]
fn reload_surfaces_501() {
    let addr = spawn_mock(|_m, _p, _b| {
        (
            501,
            not_implemented_body("phase-11/live-config-reload", "x"),
        )
    });
    let err = reload::run(&addr.to_string()).expect_err("err");
    assert!(err.to_string().contains("phase-11/live-config-reload"));
}

#[test]
fn set_surfaces_501() {
    let addr = spawn_mock(|_m, _p, _b| {
        (
            501,
            not_implemented_body("phase-11/runtime-config-set", "x"),
        )
    });
    let err = set::run(&addr.to_string(), "workers.decay.interval", "30m").expect_err("err");
    assert!(err.to_string().contains("phase-11/runtime-config-set"));
}
