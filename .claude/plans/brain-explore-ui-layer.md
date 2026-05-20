# Plan: `brain-explore` — shared terminal UI/UX layer

**Status:** draft — awaiting-confirmation
**Date:** 2026-05-20
**Author:** Claude (autonomous)
**Estimated commits:** 5

---

## 1. Scope

Create `crates/brain-explore` as a **shared rendering library** that owns every piece of terminal UI/UX Brain ships. **Both** consumer binaries — `brain-shell` (the `brain` REPL/CLI; "psql-equivalent" user tool) and `brain-cli` (the `brain-cli` admin/operator tool; "kubectl-equivalent") — migrate to consume `brain-explore` so their output looks and feels identical: same color policy, same OSC 8 handling, same `--output/-o` flag matrix, same table conventions, same NO_COLOR semantics.

Ship `brain-explore` as both a library (the API the two CLIs call) and a binary (the interactive Ratatui TUI — the original brook plan Part 6 explorer), gated behind a `tui` Cargo feature so neither CLI pays the ratatui-full compile cost.

**The layering principle (the answer to "how do brain-shell and brain-cli look the same?"):**

```
                     ┌──────────────────────────────────────┐
                     │           brain-explore              │
                     │                                      │
                     │  PRIMITIVES (used by everyone):      │
                     │    theme · term · format trait       │
                     │    OutputFormat · table builders     │
                     │    truncation · pager · OSC 8        │
                     │                                      │
                     │  USER-DOMAIN RENDERERS               │
                     │  (consumed by brain-shell + TUI):    │
                     │    memory · entity_card · graph_tree │
                     │    recall_with_graph · statement     │
                     │    relation · subscribe · plan       │
                     │                                      │
                     │  TUI (feature = "tui"):              │
                     │    interactive explorer app          │
                     └──────────────────────────────────────┘
                              ▲                    ▲
              uses primitives │                    │ uses primitives
              + user-domain   │                    │ + its own admin renderers
              renderers       │                    │ that impl Render
                              │                    │
                     ┌────────┴────────┐  ┌────────┴────────────────┐
                     │  brain-shell    │  │  brain-cli              │
                     │  (binary: brain)│  │  (binary: brain-cli)    │
                     │                 │  │                         │
                     │  REPL · parser  │  │  admin commands:        │
                     │  session        │  │   shard_health · snap   │
                     │                 │  │   worker · audit · cfg  │
                     │  (no renderers; │  │  (renderers live here   │
                     │  all in         │  │   because they're       │
                     │  brain-explore) │  │   admin-specific; they  │
                     │                 │  │   impl Render)          │
                     └─────────────────┘  └─────────────────────────┘
```

The boundary: `brain-explore` knows Brain's display-relevant *user-domain types* (memory hit, entity, statement, relation, subscription event) but never the wire protocol, the SDK client, REPL state, or command parsing. **Admin-domain renderers live in brain-cli** (shard health, snapshot manifest, worker queue, audit row, config dump) because they wrap admin-specific SDK shapes that don't belong in a "user UX library." brain-cli's renderers implement `brain_explore::Render`, so they look and behave identically — same colors, same flags, same dispatch — even though they live in a different crate.

**Why this asymmetry has a reason:**
- User-domain types (memory/entity/etc.) are consumed in **two** places — `brain-shell` commands AND the interactive TUI in `brain-explore::tui`. Living in `brain-explore` lets both reach them.
- Admin-domain types (shard health/snapshot/etc.) are consumed in **one** place — `brain-cli`. Lifting them into `brain-explore` would force `brain-explore` to depend on admin SDK surfaces for no shared-consumer benefit.

**Out of scope:**
- Web/HTML rendering. Terminal-only.
- Interactive TUI write commands (`:encode`, `:link`, `:forget`) — read-only TUI for v1 per brook plan §6.6.
- Themes beyond a single default. Theme system shape gets designed in (a feature toggle); custom themes ship later.
- Moving brain-cli's admin renderers *into* brain-explore. The asymmetry above is deliberate.

## 2. Spec references

There is no spec for the rendering layer — this is a "implementation organization" concern, not a wire-protocol or storage concern. The spec's binding constraints that **do** touch this work:

- `spec/03_wire_protocol/*` — `brain-explore` consumes `brain_protocol::response::*` types directly (MemoryResult, etc.). Field changes there ripple into `brain-explore`'s domain renderers.
- `spec/17_knowledge_model/*` and `spec/18_entities/*` — entity, statement, relation shapes that `brain-explore` renders.

Project memory rules that apply (all from `~/.claude/projects/-Users-dodo-Desktop-brain/memory/`):
- **No DB/wire versioning** ([[feedback_no_db_wire_versioning]]) — no compat shims for the existing brain-shell renderer; rewrite, don't dual-path.
- **Folder layout under src/** ([[feedback_src_folder_layout]]) — every concern in its own folder; only `lib.rs` at root.
- **Brain agent_id is first-class** ([[feedback_brain_agent_is_first_class]]) — rendering layer surfaces `agent_id` directly in stacked cards / status lines / interactive TUI top bar, not buried behind generic "connection info."
- **Comments describe WHY** ([[feedback_comments_no_spec_refs]]) — no `// Spec §X/Y` citations in `brain-explore` code.

## 3. External validation

Web-research findings (May 2026):

### Ratatui modularized for exactly this case

Ratatui 0.30.0 split into `ratatui-core` (core traits + types) and `ratatui` (the full framework with widgets, backends, layout). The official guidance: **"widget library authors should depend on `ratatui-core` to avoid pulling in unnecessary built-in widgets and reduce compilation time."** ([ratatui-core](https://crates.io/crates/ratatui-core))

This is the textbook pattern for `brain-explore` — depend on `ratatui-core` for the `Widget` trait + types; pull in full `ratatui` only behind the `tui` feature for the interactive binary.

### `tui-widgets` is the canonical shared-widget collection

Ratatui org maintains `tui-widgets` ([tui-widgets](https://github.com/ratatui/tui-widgets)) — a meta-crate that bundles standalone widget crates (`tui-big-text`, `tui-scrollview`, `tui-tree-widget`, etc.) into one. Two lessons:

1. Widgets are first-class library citizens; they don't have to live inside an app.
2. The pattern of "small focused widget crates + one umbrella" suggests we *could* split brain-explore into sub-crates later (`brain-explore-theme`, `brain-explore-widgets`, `brain-explore-app`) if the API surface justifies it. Not at v1.

### Backend abstraction

Ratatui defines a `Backend` trait; `crossterm` and `termion` implement it. ([Starlog: Ratatui rendering](https://starlog.is/articles/developer-tools/ratatui-ratatui/)). Brain picks **crossterm** (current `brain-shell` already uses `comfy-table` which composes cleanly with crossterm; matches gitui's choice).

### gitui as architectural reference

gitui ([extrawurst/gitui](https://github.com/extrawurst/gitui)) is the closest in shape to our interactive explorer: crossterm + ratatui, immediate-mode, component pattern. ~25k LOC. It does *not* split UI primitives into a library — it's a single binary. Reason: it has no companion CLI tool that needs to share renderers. Brain *does* (brain-shell + brain-explore-the-binary), so the library split is justified.

### Non-TUI primitives

`comfy-table` ([comfy-table](https://crates.io/crates/comfy-table)) and `owo-colors` ([owo-colors](https://crates.io/crates/owo-colors)) are already in `brain-shell`; they keep their roles in `brain-explore`. Both are stable and zero-allocation-on-the-hot-path-friendly.

## 4. Architecture

### 4.1 Crate shape

```
crates/brain-explore/
├── Cargo.toml
├── src/
│   ├── lib.rs                      # re-exports + crate doc
│   │
│   ├── theme/                      # semantic color tokens
│   │   ├── mod.rs                  # pub use Token, Palette, Theme
│   │   ├── token.rs                # enum Token { Label, Value, Muted, Accent, Error, Warn, Success, Confidence, Score, Predicate, EntityId, MemoryId }
│   │   └── palette.rs              # default Palette (dark-mode-first); light variant
│   │
│   ├── term/                       # capability detection + lifecycle
│   │   ├── mod.rs
│   │   ├── policy.rs               # TermPolicy { color, hyperlinks, width, height, stdout_is_tty }
│   │   ├── detect.rs               # NO_COLOR / CLICOLOR / supports-hyperlinks / isatty / terminal_size
│   │   ├── pager.rs                # Pager wrapper (spawns $PAGER if needed; passthrough otherwise)
│   │   └── hyperlink.rs            # OSC 8 link() helper
│   │
│   ├── format/                     # output format dispatch
│   │   ├── mod.rs
│   │   ├── output_format.rs        # enum OutputFormat { Auto, Table, Wide, Json, Ndjson, Yaml, JsonPath(String) }
│   │   └── render_trait.rs         # trait Render { fn render(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()>; ... per-format methods }
│   │
│   ├── table/                      # comfy-table wrappers
│   │   ├── mod.rs
│   │   ├── builder.rs              # build_table(policy) -> comfy_table::Table with project conventions
│   │   ├── truncate.rs             # middle_truncate(s, max_width)
│   │   └── cells.rs                # cell helpers: confidence_cell, score_cell, short_id_cell, …
│   │
│   ├── render/                     # domain renderers (one file per concept)
│   │   ├── mod.rs
│   │   ├── memory.rs               # RecallResults, MemoryDetail
│   │   ├── encode.rs               # EncodeResponse renderer
│   │   ├── forget.rs               # ForgetResponse renderer
│   │   ├── plan.rs                 # PlanStep, PlanStatus
│   │   ├── reason.rs               # InferenceStep
│   │   ├── txn.rs                  # TxnBegin/Commit/Abort
│   │   ├── link.rs                 # Link/Unlink
│   │   ├── subscribe.rs            # SubscriptionEvent + streaming render
│   │   ├── entity_card.rs          # stacked card (current entity_card.rs ported)
│   │   ├── graph_tree.rs           # termtree-based neighborhood (current ported)
│   │   ├── recall_with_graph.rs    # composed stacked output (current ported)
│   │   ├── statement_card.rs       # statement detail card
│   │   ├── relation_card.rs        # relation detail card
│   │   ├── audit_card.rs           # extract status / audit row
│   │   └── error.rs                # ErrorResponse → user-facing error card
│   │
│   ├── stream/                     # incremental render helpers
│   │   ├── mod.rs
│   │   └── append.rs               # row-at-a-time table append for SUBSCRIBE
│   │
│   ├── tui/                        # GATED behind feature "tui"
│   │   ├── mod.rs
│   │   ├── app.rs                  # pub fn explore() -> the interactive TUI entry point
│   │   ├── event.rs                # crossterm event loop
│   │   ├── layout.rs               # three-panel layout
│   │   ├── panels/
│   │   │   ├── mod.rs
│   │   │   ├── browser.rs          # left panel (entity/statement list)
│   │   │   ├── detail.rs           # center panel (focused card)
│   │   │   └── neighborhood.rs     # right panel (graph tree)
│   │   ├── widgets/
│   │   │   ├── mod.rs
│   │   │   ├── expandable.rs       # jless-style expand-on-Enter
│   │   │   └── help_overlay.rs     # ? cheatsheet
│   │   └── state.rs                # app state machine
│   │
│   └── util/
│       ├── mod.rs
│       └── short_id.rs             # MemoryId::short_form(), EntityId::short_form()
│
└── bin/
    └── brain-explore.rs            # tiny: parses --connect; calls brain_explore::tui::explore()
                                    # gated behind feature "tui"
```

`lib.rs` re-exports the public API:

```rust
pub mod theme;
pub mod term;
pub mod format;
pub mod table;
pub mod render;
pub mod stream;
pub mod util;
#[cfg(feature = "tui")]
pub mod tui;

pub use format::{OutputFormat, Render, RenderCtx};
pub use term::TermPolicy;
pub use theme::{Theme, Token};
```

### 4.2 Cargo.toml

```toml
[package]
name = "brain-explore"
description = "Terminal UI/UX layer for Brain — rendering primitives and the interactive explorer TUI."

[lib]
name = "brain_explore"

[[bin]]
name = "brain-explore"
path = "src/bin/brain-explore.rs"
required-features = ["tui"]

[features]
default = ["tui"]
tui = ["dep:ratatui", "dep:crossterm-event"]   # or similar; pull in full ratatui only here

[dependencies]
brain-core      = { path = "../brain-core" }      # MemoryId, EntityId, …
brain-protocol  = { path = "../brain-protocol" }  # response::* types we render

ratatui-core    = "0.30"                          # always; we depend on Widget trait + types
ratatui         = { version = "0.30", optional = true }   # interactive TUI only
crossterm       = "0.28"                          # terminal capabilities + key events

comfy-table     = "7"
owo-colors      = "4"
termtree        = "0.5"
supports-hyperlinks = "3"
terminal_size   = "0.4"

serde           = { workspace = true, features = ["derive"] }
serde_json      = { workspace = true }
serde_yaml      = "0.9"                           # for Yaml output format
jsonpath_lib    = "0.3"                           # for JsonPath output format

anyhow          = { workspace = true }
thiserror       = { workspace = true }
tracing         = { workspace = true }
```

`brain-shell`'s Cargo.toml stops pulling in `comfy-table`, `owo-colors`, `termtree`, `supports-hyperlinks`, `terminal_size` directly — it gets them transitively through `brain-explore`. It depends on `brain-explore` with `default-features = false` so the ratatui-full + tui module compile cost is skipped for the CLI/REPL.

### 4.3 Module boundaries — what `brain-shell` keeps

`brain-shell` remains the CLI + REPL. After migration:

```
crates/brain-shell/src/
├── lib.rs
├── main.rs                         # `brain` binary entry
├── cli/                            # clap args, top-level subcommands
├── parser/                         # command grammar, OutputFormatArg parsing
├── commands/                       # one module per subcommand; each builds a Render-impl and prints it
├── repl/                           # rustyline loop, completion, help, sticky agent
├── session/                        # connection state, recent_ids ring, sticky_agent
└── config/                         # ~/.config/brain/config.toml load/save
```

`brain-shell` never imports `comfy_table` / `owo_colors` / `termtree` / `supports_hyperlinks` directly after the migration — it goes through `brain_explore`.

### 4.3.1 `brain-cli` after migration

Today `brain-cli`'s entire rendering layer is `crates/brain-cli/src/output/{json.rs, table.rs}` — the `table.rs` is a 17-line hand-rolled `render_kv` that pads keys to a uniform width. No color, no comfy-table, no `--output` flag, no NO_COLOR honoring, no OSC 8. brain-cli also has zero dependency on `comfy-table` / `owo-colors` / `termtree` / `supports-hyperlinks` today.

After migration:

```
crates/brain-cli/src/
├── lib.rs
├── main.rs                         # adds --output / -o flag plumbing
├── cli/                            # clap parser; OutputFormat parsed via brain-explore
├── commands/
│   ├── agent/, audit/, config/, diagnostics/, extract/, shard/, snapshot/, worker/
│   ├── health.rs, rebuild.rs, stats.rs
│   └── … (unchanged structure)
└── output/
    ├── mod.rs
    └── render/                     # admin-domain render impls
        ├── mod.rs
        ├── shard_health.rs         # impl Render for ShardHealthResponse
        ├── shard_stats.rs          # impl Render for ShardStatsResponse
        ├── snapshot.rs             # impl Render for SnapshotManifest
        ├── worker_status.rs        # impl Render for WorkerStatusResponse
        ├── audit_row.rs            # impl Render for AuditLogEntry
        ├── config_dump.rs          # impl Render for ConfigDumpResponse
        ├── agent_record.rs         # impl Render for AgentRecord (list/get/stats)
        └── extract_status.rs       # impl Render for ExtractStatusResponse
```

Note that brain-cli's old `output/{json.rs, table.rs}` are deleted — the JSON path now goes through `brain_explore::format::dispatch` with `OutputFormat::Json`, and the kv-pair "table" becomes a proper comfy-table layout via `brain_explore::table::build_table()`. brain-cli's `output/render/*` files contain *only* the `impl Render for X` blocks plus any cell-formatting helpers specific to admin types.

brain-cli grows three deps it didn't have:
- `brain-explore = { path = "../brain-explore", default-features = false }` — pulls in primitives (no ratatui-full)
- (transitively) comfy-table, owo-colors, etc. — already in the workspace lockfile via brain-shell

brain-cli's `main.rs` `print!("{out}")` pattern collapses into:

```rust
let policy = brain_explore::TermPolicy::detect(args.color, args.hyperlinks);
let ctx = brain_explore::RenderCtx { policy, theme: brain_explore::Theme::default(), format: args.output };
brain_explore::dispatch(&response, &ctx, &mut io::stdout())?;
```

Same idiom that brain-shell's command modules use. Two binaries, one rendering protocol.

### 4.4 The `Render` trait shape

```rust
pub trait Render {
    /// Required: render as table (the default human format).
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()>;

    /// Optional: render with extra columns / sections.
    /// Default impl delegates to `render_table`.
    fn render_wide(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        self.render_table(ctx, w)
    }

    /// Required: render as a single JSON value.
    fn render_json(&self, ctx: &RenderCtx) -> serde_json::Value;

    /// Optional: render as newline-delimited JSON (one record per line).
    /// Default impl wraps `render_json` in a single-line dump.
    fn render_ndjson(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> { … }

    /// Optional: YAML; default impl serdes from `render_json`.
    fn render_yaml(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> { … }

    /// Optional: apply a jq-like JsonPath; default impl uses `render_json`.
    fn render_jsonpath(&self, ctx: &RenderCtx, path: &str, w: &mut dyn Write) -> io::Result<()> { … }
}

pub struct RenderCtx {
    pub policy: TermPolicy,
    pub theme: Theme,
    pub format: OutputFormat,
}

pub fn dispatch(item: &dyn Render, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
    match &ctx.format {
        OutputFormat::Auto => {
            if ctx.policy.stdout_is_tty { item.render_table(ctx, w) }
            else { item.render_ndjson(ctx, w) }
        }
        OutputFormat::Table   => item.render_table(ctx, w),
        OutputFormat::Wide    => item.render_wide(ctx, w),
        OutputFormat::Json    => writeln!(w, "{}", item.render_json(ctx)),
        OutputFormat::Ndjson  => item.render_ndjson(ctx, w),
        OutputFormat::Yaml    => item.render_yaml(ctx, w),
        OutputFormat::JsonPath(p) => item.render_jsonpath(ctx, p, w),
    }
}
```

Why a trait with default impls instead of a "render(format)" enum-dispatch: every domain has a custom table render, but JSON/YAML/NDJSON are mostly mechanical from `render_json` — the trait composes cleanly and minimizes per-renderer code.

### 4.5 Theme tokens

Semantic, not chromatic. `Token::Confidence` returns one color today, may return a confidence-gradient later — caller doesn't care:

```rust
pub enum Token {
    Label, Value, Muted, Accent,
    Error, Warn, Success, Info,
    Confidence, Score, Predicate,
    EntityId, MemoryId, StatementId,
}

pub struct Theme {
    palette: Palette,
}

impl Theme {
    pub fn paint<'a>(&self, token: Token, text: &'a str, policy: TermPolicy) -> Cow<'a, str> {
        if !policy.color { return Cow::Borrowed(text); }
        // owo_colors application based on palette
    }
}
```

`policy.color = false` (NO_COLOR, piped, etc.) → all `paint()` calls become identity. One place to gate.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| **A. One crate (lib + bin), tui feature-gated** | Single dep boundary; brain-shell pays no ratatui-full cost; explorer ships from the same crate | Slightly more `#[cfg(feature = "tui")]` noise; one crate has two roles | ✓ chosen |
| B. Two crates (`brain-tui` library + `brain-explore` binary) | Cleaner separation; library could be published standalone | More crate proliferation; binary's interactive-only widgets get duplicated unless `brain-explore` depends on `brain-tui` (so we end up with both anyway); v1 doesn't need it | rejected — premature split |
| C. Three crates (`brain-explore-theme` + `brain-explore-widgets` + `brain-explore-app`) | Mirrors `tui-widgets` pattern; smallest dep surface for consumers | Way too much ceremony for a layer brain-shell + one binary consume. Revisit if we publish to crates.io | rejected — not v1 |
| D. Keep rendering in brain-shell; brain-explore is just the standalone TUI binary (brook plan §6 original) | Smaller change; no migration | The user's stated goal — "standardize UI/UX" — needs the *library* extraction; without it, brain-explore-the-binary and brain-shell drift in look-and-feel | rejected — defeats the user's goal |

## 6. Risks / open questions

1. **Risk: migrating brain-shell's ~1700 lines of `output/` to brain-explore is a lot of code motion.** Mitigation: do it in three sub-commits (theme+term → format+table → domain renderers + tui), each independently green. Don't bundle with rewrites; this is a *move* commit. Style stays identical for the move; improvements come after.

2. **Risk: `brain-explore` ends up importing `brain-protocol` for response types, which is a heavier dep than `brain-core` alone.** Mitigation: that's unavoidable — the rendering layer renders wire responses. We accept the dependency; `brain-protocol` is already in the brain-shell dep graph today.

3. **Open question: ratatui-core vs ratatui-full for the default brain-explore lib build.** Recommendation: `ratatui-core` always, full `ratatui` only when `tui` feature is on. That keeps brain-shell's compile fast. Confirm whether `ratatui-core` 0.30 exposes everything domain renderers need (likely yes — they don't need the `Backend` trait or the high-level layout, only `Widget`, `Buffer`, `Rect`, `Style`).

4. **Open question: should `OutputFormat::JsonPath` actually live here, or in brain-shell as a "client transformation"?** Recommendation: in brain-explore. It's a format, not a query; treating it as one of the OutputFormat variants keeps `dispatch()` symmetric. Same for `Yaml`.

5. **Resolved: brain-cli is in scope as a first-class consumer.** It migrates in Commit 6 below. Admin-domain renderers (ShardHealth, SnapshotManifest, WorkerStatus, AuditRow, ConfigDump, AgentRecord, ExtractStatus) get `impl brain_explore::Render` blocks and live in `crates/brain-cli/src/output/render/`. brain-cli's current 17-line hand-rolled kv table renderer is deleted; comfy-table-backed rendering arrives via `brain_explore::table::build_table`. Both binaries become indistinguishable in look and behavior — same flags, same colors, same OSC 8, same `--output` matrix. See §4.3.1 for the post-migration brain-cli layout.

6. **Open question: does the TUI (Part 6 of the brook plan) ship in *this* plan, or is this plan just the *library extraction* and the TUI lands in a follow-up plan?** Recommendation: **library extraction in this plan; TUI app deferred to a follow-up.** Reason: the library extraction is a discrete, mostly mechanical migration; the TUI is a green-field app with real design surface. Bundling them stalls the migration on TUI design decisions. The crate scaffold reserves `src/tui/` and the `tui` feature so the TUI lands cleanly later.

   **This means brain-explore-the-binary doesn't ship in v1 of this plan.** `bin/brain-explore.rs` is created as a stub that prints "explorer ships in a follow-up plan; this binary is a placeholder" and exits non-zero. The `[[bin]]` declaration sits behind `required-features = ["tui"]` so a default build never even compiles it.

7. **Risk: the `Render` trait's default impls for json/yaml/jsonpath rely on `render_json` producing a faithful representation.** Some current renderers compose text in `render_table` that isn't reachable from a struct serialization (e.g. middle-truncated text). Mitigation: `render_json` must serialize the *un-truncated* logical form; truncation is a table-only concern. Audit during migration.

## 7. Test plan

Map each "Done when" to tests:

- [ ] `brain-explore` library compiles standalone (`cargo check -p brain-explore`).
- [ ] `brain-explore` library compiles without `tui` feature (`cargo check -p brain-explore --no-default-features`).
- [ ] `brain-shell` compiles using `brain-explore` (`cargo check -p brain-shell`).
- [ ] `brain-cli` compiles using `brain-explore` (`cargo check -p brain-cli`).
- [ ] **No direct deps** in brain-shell or brain-cli on `comfy-table`, `owo-colors`, `termtree`, `supports-hyperlinks`, `terminal_size` (audit: `grep` Cargo.toml).
- [ ] **All current brain-shell tests pass** unchanged after migration (`cargo test -p brain-shell`).
- [ ] **All current brain-cli tests pass** unchanged after migration (`cargo test -p brain-cli`).
- [ ] **Consistency check**: `brain recall <cue> -o json` and `brain-cli shard health -o json` produce JSON with the same envelope shape (top-level `{ data: …, meta: { ts, version, … } }` or equivalent — pick a convention in §4.4 and assert it for both).
- [ ] **Color-policy parity**: both `brain` and `brain-cli` honor `NO_COLOR`, `CLICOLOR=0`, `--color={auto,always,never}` identically. Spot-checked with a snapshot.
- [ ] New unit tests in brain-explore:
  - `theme::token_paint_is_identity_when_color_disabled` — every `Token` returns input unchanged under `policy.color = false`.
  - `term::policy_detect_honours_no_color_env`
  - `term::policy_detect_honours_clicolor_force`
  - `term::link_round_trips_through_osc8_strip`
  - `table::middle_truncate_preserves_unicode_grapheme_boundaries`
  - `format::dispatch_routes_each_variant` — exhaustive match over `OutputFormat` variants → correct method called (use a test double `Render` impl that records the call).
  - `format::auto_picks_ndjson_when_not_tty`
  - `render::memory::recall_results_table_under_narrow_width_truncates` — set `policy.width = 40`, assert table fits.
  - `render::entity_card::renders_all_sections`
  - `render::entity_card::omits_empty_sections`
  - `render::graph_tree::tree_uses_supports_hyperlinks_gate`
- [ ] Snapshot tests using `insta` (or a hand-rolled equivalent) for each major renderer — catches accidental output drift.
- [ ] `cargo doc -p brain-explore -- -D warnings`.

`brain-shell` tests survive unchanged. If any depend on the *exact* layout of comfy-table output, those tests get reviewed during migration but the strong assumption is they assert on logical content not exact bytes.

## 8. Commit shape

Six commits, each compiling green:

1. **`feat(explore): scaffold brain-explore crate (theme + term)`** — new `crates/brain-explore/`, `Cargo.toml`, `lib.rs`, theme module + term module + util module. Workspace `Cargo.toml` adds the member. No callers yet. ~500 LOC, mostly moved from `brain-shell/src/output/term.rs`.

2. **`feat(explore): add format dispatch + table builders`** — `format/`, `table/` modules. Defines `Render` trait, `RenderCtx`, `OutputFormat`, table builder helpers, `middle_truncate`, cell helpers. ~600 LOC, mostly from `brain-shell/src/output/table.rs` (the non-domain parts).

3. **`feat(explore): port user-domain renderers from brain-shell`** — `render/` module: memory, encode, forget, plan, reason, txn, link, subscribe, entity_card, graph_tree, recall_with_graph, statement_card, relation_card, audit_card, error. ~1200 LOC moved from `brain-shell/src/output/render/*` and the per-type impls in `brain-shell/src/output/table.rs`. Each user-domain renderer impls `Render`. Admin-domain renderers do NOT live here.

4. **`refactor(shell): migrate brain-shell to brain-explore`** — add `brain-explore` dep to `brain-shell/Cargo.toml`; remove the direct deps on `comfy-table`/`owo-colors`/etc.; delete `brain-shell/src/output/` entirely; update every caller in `brain-shell/src/commands/**` to import from `brain_explore::*` and call `dispatch(&item, &ctx, &mut stdout)`. brain-shell shrinks meaningfully. ~800 LOC net negative (deletions).

5. **`refactor(cli): migrate brain-cli to brain-explore`** — add `brain-explore` dep to `brain-cli/Cargo.toml`; delete `crates/brain-cli/src/output/{json.rs, table.rs}` (the 17-line kv renderer + the json shim); add `crates/brain-cli/src/output/render/{shard_health, shard_stats, snapshot, worker_status, audit_row, config_dump, agent_record, extract_status}.rs` — each one a focused `impl brain_explore::Render` for the admin-domain response type. Add `--output / -o` flag parsing to brain-cli's clap surface so it gets the same OutputFormat matrix as brain-shell. Add `--color={auto,always,never}` and `--hyperlinks={auto,always,never}` flags so the policy story matches. Update `main.rs`'s `print!("{out}")` sites to call `brain_explore::dispatch(&response, &ctx, &mut io::stdout())`. ~400 LOC net (some new admin render code, but the JSON / kv paths get smaller). The 8 admin command families touch their leaf modules to use the new flow.

6. **`feat(explore): scaffold tui module + binary stub`** — empty `tui/` module gated behind `tui` feature; `bin/brain-explore.rs` placeholder; `required-features = ["tui"]`. Reserves the structure for the follow-up TUI plan. ~50 LOC.

Each commit ships its own tests. Commits 4 and 5 are the riskiest — each deletes a meaningful chunk of consumer-side rendering code in exchange for `brain_explore::*` imports. Verify with `cargo test -p brain-shell` and `cargo test -p brain-cli` respectively before landing each.

Commits 1→2→3 are strictly sequential (each depends on the previous). Commits 4 and 5 can run **in parallel** after 3 lands — they touch disjoint consumer crates with no shared files. Commit 6 is independent and can fan out with 4 or 5.

## 9. Confirmation

Sign-off needed on six judgment calls before I start:

1. **Crate shape:** one crate (`brain-explore`) with library + binary, tui feature-gated. Or do you want two crates (`brain-tui` library + `brain-explore` binary)? Plan recommends the single-crate shape (§5 row A).

2. **TUI app scope:** library extraction + brain-cli/brain-shell migration in *this* plan; the interactive TUI app deferred to a follow-up plan. Or do you want this plan to ship the TUI app too? Plan recommends defer (§6 Q6).

3. **Domain dependency:** `brain-explore` depends on `brain-core` AND `brain-protocol` for the response types it renders. Confirm OK. If you'd rather keep `brain-protocol` out of `brain-explore`, we add a thin adapter layer in brain-shell that re-shapes protocol responses into `brain-explore`-owned display structs — more code, cleaner dep graph. Plan recommends direct dep (§6 Q2).

4. **Theme scope:** ship a single hard-coded default theme (dark-mode-first) in v1. User-configurable themes via `~/.config/brain/theme.toml` deferred. Confirm OK. Plan recommends single default (§1).

5. **brain-cli migration scope:** brain-cli is fully in scope; both binaries (`brain` + `brain-cli`) end up consuming `brain-explore` and the two render identically. Admin-domain renderers live in `brain-cli/src/output/render/` and impl `brain_explore::Render`; primitives + user-domain renderers live in `brain-explore`. Confirm the asymmetry sketch in §1 / §4.3.1 reads right.

6. **Migration cleanliness:** six commits as drafted in §8, each green; commits 4 and 5 (brain-shell migration + brain-cli migration) run in parallel after commit 3 lands. Or do you want a different cut?

After sign-off, I'll convert §8 into tasks and launch parallel `rust-implementer` subagents along the dependency chain:

```
1 (scaffold)
  ↓
2 (format + table)
  ↓
3 (user-domain renderers)
  ↓
  ├──── 4 (brain-shell migration) ──┐
  │                                  ├── done
  └──── 5 (brain-cli migration)  ────┤
                                     │
                  6 (tui stub) ──────┘
```

Commits 4, 5, 6 fan out parallel; that's where the multi-agent speedup lives.
