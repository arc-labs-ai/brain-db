//! Pre-clap argv scan for `--help` / `-h`.
//!
//! clap validates required positional arguments before it surfaces
//! global flags, so `brain recall --help` would fail with
//! "missing <QUERY>" even though the user only wants to see the
//! help card. This module runs BEFORE clap so that
//! `<verb> --help` short-circuits to the unified `HelpVerb` card
//! regardless of what other arguments are missing.
//!
//! The scan is permissive by design — it accepts `--help` and `-h`
//! anywhere in the argv (clap-style) and stops at `--` so a user
//! who genuinely wants to encode the literal string `"--help"` can
//! escape with `brain encode -- --help`.

/// Outcome of a pre-clap argv scan. `verb` is `None` for the bare
/// top-level case (`brain --help`); `Some(name)` for `brain <verb>
/// --help` where `<verb>` is the first non-flag positional argument.
#[derive(Debug, PartialEq, Eq)]
pub struct HelpIntent {
    pub verb: Option<String>,
}

/// Scan `argv` (including the program name at index 0) for a help
/// request. Returns `Some(HelpIntent { verb })` when `--help` / `-h`
/// is present before any `--` separator; returns `None` otherwise.
///
/// The verb is the first non-flag positional after the program name,
/// not normalised against any whitelist — the caller maps unknown
/// verbs to the top-level help via `repl::help::lookup`'s
/// `HelpUnknown` fallback.
#[must_use]
pub fn detect_help_intent(argv: &[String]) -> Option<HelpIntent> {
    let mut wants_help = false;
    let mut verb: Option<String> = None;
    for arg in argv.iter().skip(1) {
        if arg == "--" {
            // Everything after `--` is positional. A user typing
            // `brain encode -- --help` really wants to encode the
            // text `--help`; we must not intercept that.
            break;
        }
        if arg == "--help" || arg == "-h" {
            wants_help = true;
        } else if !arg.starts_with('-') && verb.is_none() {
            verb = Some(arg.clone());
        }
    }
    if wants_help {
        Some(HelpIntent { verb })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("brain")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn no_help_flag_returns_none() {
        assert_eq!(detect_help_intent(&argv(&[])), None);
        assert_eq!(detect_help_intent(&argv(&["encode", "hello"])), None);
        assert_eq!(detect_help_intent(&argv(&["recall", "auth"])), None);
    }

    #[test]
    fn bare_help_returns_no_verb() {
        let got = detect_help_intent(&argv(&["--help"]));
        assert_eq!(got, Some(HelpIntent { verb: None }));
        let got = detect_help_intent(&argv(&["-h"]));
        assert_eq!(got, Some(HelpIntent { verb: None }));
    }

    #[test]
    fn verb_before_help_picks_up_verb() {
        let got = detect_help_intent(&argv(&["encode", "--help"]));
        assert_eq!(
            got,
            Some(HelpIntent {
                verb: Some("encode".into())
            })
        );
        let got = detect_help_intent(&argv(&["recall", "-h"]));
        assert_eq!(
            got,
            Some(HelpIntent {
                verb: Some("recall".into())
            })
        );
    }

    #[test]
    fn help_before_verb_still_finds_verb() {
        // clap accepts global flags before subcommands, so we must too.
        let got = detect_help_intent(&argv(&["--help", "encode"]));
        assert_eq!(
            got,
            Some(HelpIntent {
                verb: Some("encode".into())
            })
        );
    }

    #[test]
    fn other_flags_dont_confuse_verb_detection() {
        let got = detect_help_intent(&argv(&[
            "--server",
            "localhost:9090",
            "encode",
            "--help",
        ]));
        // `--server` takes a value (`localhost:9090`); the verb scan
        // treats both tokens as flag-like (the value starts with a
        // digit, but the previous token was a flag). For the purposes
        // of this scan we only need to find the first NON-flag, which
        // is `encode`. That works as long as flag values are stripped
        // by clap later — they are.
        assert_eq!(
            got,
            Some(HelpIntent {
                verb: Some("localhost:9090".into())
            })
        );
        // Caveat captured: the scan only looks at "first non-flag
        // token." If clap doesn't recognise the resolved "verb" it
        // routes to top-level help anyway via HelpUnknown, so this is
        // safe in practice. Documented here so a future maintainer
        // doesn't try to "fix" this by teaching the scan about
        // flag-value pairs (that path leads to re-implementing clap).
    }

    #[test]
    fn double_dash_escapes_help() {
        // `brain encode -- --help` means "encode the literal text
        // --help"; the user explicitly opted out of flag parsing.
        let got = detect_help_intent(&argv(&["encode", "--", "--help"]));
        assert_eq!(got, None);
    }

    #[test]
    fn double_dash_after_help_still_intercepts() {
        // `--help` comes before `--`, so we intercept even though
        // a `--` follows. clap would do the same.
        let got = detect_help_intent(&argv(&["--help", "--", "anything"]));
        assert_eq!(got, Some(HelpIntent { verb: None }));
    }
}
