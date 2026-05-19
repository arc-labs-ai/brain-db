//! Color, pager, and hyperlink primitives.
//!
//! These exist so the rest of the shell never reaches for env vars
//! directly. One place to honor `NO_COLOR`, `CLICOLOR`, `--color`,
//! `supports-hyperlinks`, and `$PAGER` semantics — every renderer
//! consults the same policy.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::{Child, Command, Stdio};

use crate::parser::{ColorMode, HyperlinkMode};

/// Policy bag the renderers consult. Built once at command dispatch,
/// passed by value into the renderers so they don't reach into
/// process state on the hot path.
#[derive(Debug, Clone, Copy)]
pub struct TermPolicy {
    pub color: bool,
    pub hyperlinks: bool,
    pub width: usize,
    pub height: usize,
    pub stdout_is_tty: bool,
}

impl TermPolicy {
    /// Probe the current process and reconcile against the user's
    /// `--color` / `--hyperlinks` global flags.
    #[must_use]
    pub fn detect(color: ColorMode, hyperlinks: HyperlinkMode) -> Self {
        let stdout_is_tty = io::stdout().is_terminal();
        let (width, height) = detect_terminal_size();
        Self {
            color: should_use_color(color, stdout_is_tty),
            hyperlinks: should_use_hyperlinks(hyperlinks, stdout_is_tty),
            width,
            height,
            stdout_is_tty,
        }
    }

    /// Convenience for tests: a deterministic policy that asks for no
    /// color, no hyperlinks, 80×24, not-a-TTY.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            color: false,
            hyperlinks: false,
            width: 80,
            height: 24,
            stdout_is_tty: false,
        }
    }
}

/// Resolve the `--color` flag against env vars + isatty.
///
/// Precedence (highest first):
///   1. `--color=always` / `--color=never`
///   2. `NO_COLOR` set (any value) → off (per <https://no-color.org>)
///   3. `CLICOLOR=0` → off; `CLICOLOR_FORCE` non-zero → on
///   4. isatty(stdout) — color iff stdout is a TTY
#[must_use]
pub fn should_use_color(mode: ColorMode, stdout_is_tty: bool) -> bool {
    match mode {
        ColorMode::Always => return true,
        ColorMode::Never => return false,
        ColorMode::Auto => {}
    }
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(v) = env::var("CLICOLOR_FORCE") {
        if v != "0" {
            return true;
        }
    }
    if let Ok(v) = env::var("CLICOLOR") {
        if v == "0" {
            return false;
        }
    }
    stdout_is_tty
}

/// Resolve the `--hyperlinks` flag.
///
/// `auto` consults the `supports-hyperlinks` probe — it knows the
/// terminals (iTerm2, kitty, WezTerm, modern VTE, …) that handle OSC 8
/// cleanly and bails on the ones that print the escape sequence as
/// noise.
#[must_use]
pub fn should_use_hyperlinks(mode: HyperlinkMode, stdout_is_tty: bool) -> bool {
    match mode {
        HyperlinkMode::Always => true,
        HyperlinkMode::Never => false,
        HyperlinkMode::Auto => {
            stdout_is_tty && supports_hyperlinks::on(supports_hyperlinks::Stream::Stdout)
        }
    }
}

/// Wrap `text` in an OSC 8 hyperlink pointing at `target`. Falls back
/// to plain `text` when the policy says hyperlinks are off — so call
/// sites can always render through this helper.
#[must_use]
pub fn link(policy: TermPolicy, text: &str, target: &str) -> String {
    if !policy.hyperlinks {
        return text.to_owned();
    }
    // OSC 8 ; ; <uri> ST <text> OSC 8 ; ; ST
    format!("\x1b]8;;{target}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Auto-pager wrapper. Spawns `$PAGER` (defaulting to `less -R` so ANSI
/// colors survive) when the rendered output overflows the terminal
/// height and stdout is a TTY. Otherwise writes straight to stdout.
///
/// Use via [`Pager::page`]: build the body into a `String`, then hand
/// it over. Direct streaming through a paged process is more efficient
/// but harder to get right; the shell's outputs are always small enough
/// for buffering.
pub struct Pager {
    policy: TermPolicy,
}

impl Pager {
    #[must_use]
    pub fn new(policy: TermPolicy) -> Self {
        Self { policy }
    }

    /// Write `body` to stdout, paging through `$PAGER` if it overflows.
    pub fn page(&self, body: &str) -> io::Result<()> {
        let line_count = body.lines().count();
        let should_page = self.policy.stdout_is_tty && line_count > self.policy.height;
        if !should_page {
            let mut stdout = io::stdout().lock();
            stdout.write_all(body.as_bytes())?;
            return stdout.flush();
        }
        match spawn_pager() {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    if let Err(e) = stdin.write_all(body.as_bytes()) {
                        // Pager process closed early (q before EOF) is
                        // a SIGPIPE — common and not an error.
                        if e.kind() != io::ErrorKind::BrokenPipe {
                            return Err(e);
                        }
                    }
                }
                let _ = child.wait();
                Ok(())
            }
            Err(_) => {
                // Falling back to stdout is more useful than failing
                // hard — the user gets the output, just unpaginated.
                let mut stdout = io::stdout().lock();
                stdout.write_all(body.as_bytes())?;
                stdout.flush()
            }
        }
    }
}

fn spawn_pager() -> io::Result<Child> {
    let pager = env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let mut parts = pager.split_whitespace();
    let prog = parts.next().unwrap_or("less");
    let args: Vec<&str> = parts.collect();
    Command::new(prog).args(args).stdin(Stdio::piped()).spawn()
}

fn detect_terminal_size() -> (usize, usize) {
    // Honor $COLUMNS / $LINES first — they're how a user overrides
    // the system probe (test harnesses, recorded sessions, …).
    let env_cols = env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    let env_lines = env::var("LINES").ok().and_then(|s| s.parse::<usize>().ok());

    let (probed_w, probed_h) = terminal_size::terminal_size()
        .map(|(w, h)| (w.0 as usize, h.0 as usize))
        .unwrap_or((100, 30));

    let width = env_cols.unwrap_or(probed_w);
    let height = env_lines.unwrap_or(probed_h);
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn color_always_overrides_no_color() {
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
    fn color_auto_off_when_no_color_set() {
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
    fn color_auto_off_when_clicolor_zero() {
        with_env(
            &[
                ("NO_COLOR", None),
                ("CLICOLOR", Some("0")),
                ("CLICOLOR_FORCE", None),
            ],
            || {
                assert!(!should_use_color(ColorMode::Auto, true));
            },
        );
    }

    #[test]
    fn color_auto_follows_isatty() {
        with_env(
            &[
                ("NO_COLOR", None),
                ("CLICOLOR", None),
                ("CLICOLOR_FORCE", None),
            ],
            || {
                assert!(should_use_color(ColorMode::Auto, true));
                assert!(!should_use_color(ColorMode::Auto, false));
            },
        );
    }

    #[test]
    fn hyperlinks_always_returns_true_even_off_tty() {
        assert!(should_use_hyperlinks(HyperlinkMode::Always, false));
    }

    #[test]
    fn hyperlinks_never_returns_false_on_tty() {
        assert!(!should_use_hyperlinks(HyperlinkMode::Never, true));
    }

    #[test]
    fn link_with_hyperlinks_off_passes_text_through() {
        let p = TermPolicy::plain();
        assert_eq!(link(p, "m17", "brain://recall/m17"), "m17");
    }

    #[test]
    fn link_with_hyperlinks_on_wraps_in_osc8() {
        let mut p = TermPolicy::plain();
        p.hyperlinks = true;
        let out = link(p, "m17", "brain://recall/m17");
        assert!(out.contains("brain://recall/m17"));
        assert!(out.contains("m17"));
        assert!(out.contains('\x1b'));
    }
}
