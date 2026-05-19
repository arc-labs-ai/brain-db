//! `should_use_color` / `should_use_hyperlinks` honor env vars + flags.
//! Tests use a tempfile env-mutation helper so they can run in parallel
//! without contention (each test saves + restores the relevant vars).

use std::env;

use brain_shell::output::term::{should_use_color, should_use_hyperlinks};
use brain_shell::parser::{ColorMode, HyperlinkMode};

fn with_env<F: FnOnce()>(set: &[(&str, Option<&str>)], f: F) {
    let saved: Vec<(String, Option<String>)> = set
        .iter()
        .map(|(k, _)| ((*k).to_string(), env::var(k).ok()))
        .collect();
    for (k, v) in set {
        match v {
            Some(value) => env::set_var(k, value),
            None => env::remove_var(k),
        }
    }
    f();
    for (k, v) in saved {
        match v {
            Some(value) => env::set_var(&k, value),
            None => env::remove_var(&k),
        }
    }
}

#[test]
fn color_always_overrides_env() {
    with_env(&[("NO_COLOR", Some("1"))], || {
        assert!(should_use_color(ColorMode::Always, false));
    });
}

#[test]
fn color_never_overrides_clicolor_force() {
    with_env(&[("CLICOLOR_FORCE", Some("1"))], || {
        assert!(!should_use_color(ColorMode::Never, true));
    });
}

#[test]
fn color_auto_off_with_no_color_env() {
    with_env(
        &[
            ("NO_COLOR", Some("1")),
            ("CLICOLOR", None),
            ("CLICOLOR_FORCE", None),
        ],
        || {
            assert!(!should_use_color(ColorMode::Auto, true));
        },
    );
}

#[test]
fn color_auto_off_when_not_tty() {
    with_env(
        &[
            ("NO_COLOR", None),
            ("CLICOLOR", None),
            ("CLICOLOR_FORCE", None),
        ],
        || {
            assert!(!should_use_color(ColorMode::Auto, false));
        },
    );
}

#[test]
fn hyperlinks_never_returns_false_even_on_tty() {
    assert!(!should_use_hyperlinks(HyperlinkMode::Never, true));
}

#[test]
fn hyperlinks_always_returns_true_off_tty() {
    assert!(should_use_hyperlinks(HyperlinkMode::Always, false));
}

#[test]
fn hyperlinks_auto_off_when_not_tty() {
    // Auto mode requires a TTY (and a supporting terminal); off-TTY is
    // always false.
    assert!(!should_use_hyperlinks(HyperlinkMode::Auto, false));
}
