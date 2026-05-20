//! Drift tests for the unified help routing.
//!
//! `brain <verb> --help` renders the same `HelpVerb` card as
//! `brain> help <verb>`. The card's `Flags` block is hand-curated; if
//! someone adds a new flag to the clap-derive struct without updating
//! the matching `repl::help::help_<verb>()` fixture, the card lies to
//! users about what flags exist. These tests pin the contract:
//!
//!   * every verb-specific long flag declared in clap MUST appear in
//!     the corresponding HelpVerb's rendered card
//!   * the Reference block's `<verb> --help` pointer MUST resolve
//!     (i.e. the verb is one our lookup function recognises)
//!
//! Globals (`--server`, `--agent`, `-o`, `--color`, etc.) are
//! intentionally not enumerated per-verb on the card — they're shared
//! across all verbs and a future "Globals" card or top-level help can
//! own them.

use brain_explore::{dispatch, OutputFormat, RenderCtx, TermPolicy, Theme};
use brain_shell::parser::{
    EncodeArgs, ForgetArgs, LinkArgs, PlanArgs, ReasonArgs, RecallArgs, SubscribeArgs, UnlinkArgs,
};
use brain_shell::repl::help::lookup;
use clap::{Args, Command};

/// Render a help card for `verb` to a plain-policy table. The same
/// path the binary follows on `brain <verb> --help`.
fn render_card(verb: &str) -> String {
    let payload = lookup(Some(verb));
    let ctx = RenderCtx {
        policy: TermPolicy::plain(),
        theme: Theme::default(),
        format: OutputFormat::Table,
    };
    let mut buf = Vec::new();
    dispatch(payload.as_ref(), &ctx, &mut buf).expect("dispatch");
    String::from_utf8(buf).expect("utf8")
}

/// Walk clap's argument list for `A` and return every long flag name
/// declared by the per-verb struct (not by the flattened `GlobalOpts`).
/// Returns flags WITH the `--` prefix so they're string-searchable
/// directly against the card text.
///
/// Per-verb structs derive `Args` (not `Parser`), so `CommandFactory`
/// isn't available. `Args::augment_args` is the equivalent — it
/// attaches the struct's `#[arg(...)]` fields to a synthetic Command
/// from which we can enumerate.
fn verb_specific_long_flags<A: Args>() -> Vec<String> {
    let cmd = A::augment_args(Command::new("drift-probe"));
    cmd.get_arguments()
        // `--help` is always global on the top-level Cli (via
        // GlobalOpts); the per-verb argument enumeration sometimes
        // surfaces it as inherited. Skip so we only assert against
        // verb-specific flags.
        .filter(|a| a.get_id() != "help")
        // Globals are flagged via `global = true`. Per-verb cards
        // intentionally don't list them.
        .filter(|a| !a.is_global_set())
        .filter_map(|a| a.get_long().map(|s| format!("--{s}")))
        .collect()
}

/// Per-verb assertion: every clap-defined long flag must appear in
/// the rendered card. Panics with a helpful message naming the verb
/// and the missing flag if drift is detected.
fn assert_no_flag_drift<A: Args>(verb: &str) {
    let card = render_card(verb);
    for flag in verb_specific_long_flags::<A>() {
        assert!(
            card.contains(&flag),
            "clap defines {flag} on `{verb}` but the help card doesn't list it. \
             Update crates/brain-shell/src/repl/help.rs::help_{verb}() to include it."
        );
    }
}

// ── per-verb tests ──────────────────────────────────────────────────

#[test]
fn encode_card_lists_every_clap_flag() {
    assert_no_flag_drift::<EncodeArgs>("encode");
}

#[test]
fn recall_card_lists_every_clap_flag() {
    assert_no_flag_drift::<RecallArgs>("recall");
}

#[test]
fn plan_card_lists_every_clap_flag() {
    assert_no_flag_drift::<PlanArgs>("plan");
}

#[test]
fn reason_card_lists_every_clap_flag() {
    assert_no_flag_drift::<ReasonArgs>("reason");
}

#[test]
fn forget_card_lists_every_clap_flag() {
    assert_no_flag_drift::<ForgetArgs>("forget");
}

#[test]
fn link_card_lists_every_clap_flag() {
    assert_no_flag_drift::<LinkArgs>("link");
}

#[test]
fn unlink_card_lists_every_clap_flag() {
    assert_no_flag_drift::<UnlinkArgs>("unlink");
}

#[test]
fn subscribe_card_lists_every_clap_flag() {
    assert_no_flag_drift::<SubscribeArgs>("subscribe");
}

// `txn` is a subcommand enum (`begin / commit / abort`), not a flag-
// bearing struct — no per-flag drift to test. Usage lines on its
// card are pinned by the existing repl::help::tests::help_txn test.
