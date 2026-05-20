# Unified help routing — one card, three entry points

**Status**: approved, in flight
**Scope**: `crates/brain-shell/src/parser/command.rs`, dispatch wiring in
`crates/brain-shell/src/cli/` + `repl/loop.rs`, drift tests
**Goal**: `brain <verb> --help`, `brain> <verb> --help`, and
`brain> help <verb>` all output the same `HelpVerb` card.

---

## 1 · Why

Today two help systems coexist:

- **clap auto-help** (`encode --help`): flat list of every flag,
  globals interleaved with verb-specifics, gated flags surfaced as
  if working, multi-line descriptions that wrap awkwardly.
- **HelpVerb card** (`help encode`): grouped Flags / Sources / Notes /
  Example / Reference, polished after H1-H7.

Users hit both and the inconsistency is jarring. Both systems claim to
be authoritative — neither is. Unified routing makes the card the only
help surface and clap the parser only.

## 2 · The design

Three integration points:

| Entry point | Today | After |
|---|---|---|
| `brain encode --help` | clap auto-help | `HelpVerb` card via interception |
| `brain> encode --help` | clap auto-help (REPL re-uses Cli) | `HelpVerb` card via interception |
| `brain> help encode` | `HelpVerb` card | unchanged |

The card is `repl::help::lookup(Some("encode"))` — already a
`Box<dyn Render>`, already drives the card layout. We need to make
clap not print its own help and instead set a flag we read after
parsing.

## 3 · Implementation

### L1 — Disable clap's `--help`; add custom flag on `GlobalOpts`

```rust
#[derive(Debug, Parser, Clone)]
#[command(
    name = "brain",
    bin_name = "brain",
    version,
    about = "Interactive shell for the Brain cognitive substrate.",
    disable_help_subcommand = true,
    disable_help_flag = true,   // ← NEW
)]
pub struct Cli { … }

#[derive(Debug, Args, Clone)]
pub struct GlobalOpts {
    // … existing fields …

    /// Print help (overrides everything else).
    #[arg(long, short = 'h', global = true, action = clap::ArgAction::SetTrue)]
    pub help: bool,
}
```

`disable_help_flag = true` propagates to subcommands via clap's
inheritance, but we make the custom `help` field `global = true` so
every subcommand inherits it without per-verb annotation.

### L2 — Intercept in the dispatcher

Two entry points dispatch the parsed Cli:
- One-shot: `crates/brain-shell/src/cli/run.rs` (top-level
  `run_one_shot` / `run_one`)
- REPL: `crates/brain-shell/src/repl/loop.rs` (per-line dispatch
  after `parse_repl_line`)

Add a helper in `repl::help`:

```rust
pub fn render_help_for(verb: Option<&str>, ctx: &RenderCtx,
                      w: &mut dyn Write) -> io::Result<()> {
    let card = lookup(verb);
    dispatch(card.as_ref(), ctx, w)
}
```

Each dispatch point checks `cli.global.help` before dispatch:

```rust
if cli.global.help {
    let verb = cli.subcommand.as_ref().map(Command::verb_name);
    repl::help::render_help_for(verb, &ctx, &mut stdout)?;
    return Ok(ExitCode::SUCCESS);
}
```

`Command::verb_name(&self) -> &'static str` needs adding (small match
on the subcommand enum variants → their canonical names).

### L3 — Drift tests

Per-verb test using clap introspection:

```rust
fn assert_all_clap_flags_in_card<A: clap::CommandFactory>(
    fixture: &HelpVerb,
) {
    let clap_cmd = A::command();
    let clap_flags: Vec<String> = clap_cmd
        .get_arguments()
        .filter(|a| !a.is_global_set() || a.get_id() == "help")
        // globals carry their own card or are caught at top level;
        // the per-verb fixture only needs to surface non-global flags
        // unless the verb chooses to document them.
        .filter_map(|a| a.get_long().map(|s| format!("--{s}")))
        .collect();
    let card = render_card_table(fixture);
    for flag in clap_flags {
        assert!(card.contains(&flag),
            "clap defines {flag} on this verb but the card doesn't list it");
    }
}
```

One test per verb. Tests pin the contract: every clap-parseable flag
must appear in its `HelpVerb` card.

### L4 — Docs touch-up

- `README.md`: command examples that show `--help` continue to work;
  add a one-line note that `--help` and `help <verb>` are equivalent.
- `docs/reference/shell/meta/help.md`: document the unified
  behaviour.

### L5 — Verify gate

```bash
cargo fmt -p brain-shell
cargo clippy -p brain-shell --lib --tests -- -D warnings
cargo test  -p brain-shell -p brain-explore
```

## 4 · Edge cases I'll handle

1. **`brain --help` (no subcommand)** → render `top_level()`.
2. **`brain encode --help --bad-flag`** → clap errors on `--bad-flag`
   first; our intercept never runs; standard behaviour.
3. **`brain encode -h`** → routes through the same `cli.global.help`
   path (short alias on the same field).
4. **`brain --help encode`** → clap parses `--help` as a global flag
   and `encode` as the subcommand; our intercept fires with
   `verb = Some("encode")`. Equivalent to `brain encode --help`.
5. **Piped output** → `TermPolicy::detect_for(stdout)` already gives
   us `plain()` when stdout isn't a TTY; box-drawing characters
   render as muted lines that still parse.
6. **Color / hyperlink overrides on the help render** → honoured via
   the existing `--color` / `--hyperlinks` global flags; the
   `RenderCtx` we build for help uses the same policy as the rest of
   the command.

## 5 · What this kills

- **K1-K6 (clap help standardization)** plan. Moot — clap's help
  surface goes away.
- The `encode --help` mess the user pasted. Replaced wholesale.

## 6 · Risks I'd accept pushback on

- **Losing clap's automatic `[possible values: …]` and `[env: …]`
  display.** Our cards encode the same info manually (`--kind K:
  episodic | semantic | consolidated`). Drift risk mitigated by L3
  tests.
- **`--help` no longer Unix-conventional (pure stdout, easy to grep
  for `--<flag>`).** Our card has `Flags` rows that `grep '\-\-'` can
  still extract; the box-drawing rules are line-stable so `grep -v
  '^─'` cleans them out. Better-than-clap for human reading,
  comparable-to-clap for scripting.
- **A user discovering a new flag in code before the help card knows
  about it.** L3 drift tests fail in CI before a PR can land, so this
  failure mode is structural, not procedural.

## 7 · Commit boundaries

| # | Commit | LoC | Touches |
|---|---|---|---|
| L1 | Disable clap auto-help; add `help: bool` on `GlobalOpts` | ~30 | parser/command.rs |
| L2 | Dispatch interception (one-shot + REPL) | ~60 | cli/run.rs, repl/loop.rs, repl/help.rs |
| L3 | Drift tests per verb | ~120 | tests/help_drift.rs (new) |
| L4 | Docs: README + meta/help.md | ~30 | docs/ |
| L5 | fmt + clippy + test | — | — |

Single branch, single PR.

## 8 · Done when

- `brain encode --help`, `brain> encode --help`, `brain> help encode`
  produce byte-identical card output for the same terminal policy.
- Same for every other verb (`recall`, `forget`, `link`, `unlink`,
  `txn`, `subscribe`, `plan`, `reason`).
- `brain --help` (no verb) renders the top-level directory.
- Drift tests pass for every verb.
- `cargo fmt && cargo clippy -D warnings && cargo test` green.
- Docs updated.
