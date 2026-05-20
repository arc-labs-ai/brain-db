# Plan: agent config redesign — name + id, default, active, auto-mint, \info

**Status:** draft — awaiting-confirmation
**Date:** 2026-05-20
**Author:** Claude (autonomous)
**Estimated commits:** 4

---

## 1. Scope

Restructure brain-shell's agent identity story around three new behaviors:

1. **Every persisted agent has both a name AND a UUID**, stored together in `~/.config/brain/config.toml`.
2. **Bare `brain` (no `--agent` / no `BRAIN_AGENT`) auto-mints + persists** a fresh named agent the first time it runs, instead of producing a non-persisted ephemeral UUIDv7 (the current K1 behavior).
3. **Two new agent-state flags** — `default` (factory-default, sticky across the user's whole config lifetime) and `active` (currently-selected, mutates on `\agent use`).
4. **`\info` meta command** that dumps connection + server + agent + session state for support / debugging.
5. **`\agent use <name>` persists the switch** to the config file so the next bare `brain` lands on the same agent.
6. **`\agent create` already exists and stays create-only** (doesn't auto-switch); `\agent use` is the explicit switch.

**Out of scope:**
- Cross-machine config sync. The file stays local.
- Encrypted credentials. v1 stores plaintext UUIDs (per spec §03/09 auth is `none` in v1).
- Multi-server profiles. One config file, one set of agents, all addressed at the configured server. (kubectl-style multi-cluster + multi-agent matrix is a follow-up.)
- Migrating substrate-side data when switching agents. Agent IDs are routing keys; existing memories under a different agent stay under that agent.

## 2. Spec references

The shell config is a brain-shell concern, not substrate. Spec touchpoints:

- `spec/06_handshake/05_auth.md` — auth-time agent_id binding. The connection's `caller_agent` (per `cffcc0d`) flows from whatever the shell sends in the AUTH frame; this plan changes how the shell decides what to send.
- `spec/03_wire_protocol/06_handshake.md` — WELCOME response carries `server_id`, capabilities, `server_time_unix_nanos`. `\info` reads these.

Project memory rules that apply (`~/.claude/projects/-Users-dodo-Desktop-brain/memory/`):
- **`agent_id` is first-class** ([[feedback_brain_agent_is_first_class]]) — surface it directly via `\agent`, not behind a generic command. ✓ existing design already does this.
- **No DB/wire versioning** ([[feedback_no_db_wire_versioning]]) — the on-disk config schema changes (`default`, `active` added to `AgentEntry`); load path migrates legacy entries on first read, but there are no schema-version-stamp shims.
- **Folder layout under src/** ([[feedback_src_folder_layout]]) — each concern in its own folder. `cli/agent/` becomes a folder with `config.rs`, `resolve.rs`, `commands.rs`.
- **Comments describe WHY** ([[feedback_comments_no_spec_refs]]) — no `// Spec §X/Y` citations in code; spec numbers rot, business reasons don't.

## 3. External validation

How three mature tools handle the same problem.

### AWS CLI named profiles ([docs](https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-files.html))

- Profiles named via `[default]` and `[profile prod]` sections in `~/.aws/config`.
- The `[default]` section is implicit and selected when no `--profile` is passed.
- No "active" concept — selection is *strictly* per-command (`--profile prod`) or per-session (`AWS_PROFILE=prod`).
- **Lesson:** "default" is a sticky disk-level concept, not "the one currently in use."

### kubectl contexts ([docs](https://kubernetes.io/docs/reference/kubectl/generated/kubectl_config/kubectl_config_use-context/))

- `contexts:` list in kubeconfig + a top-level `current-context: <name>` field.
- `kubectl config use-context <name>` mutates `current-context` and rewrites the file.
- `kubectl config current-context` prints the active name.
- `--context <name>` flag overrides per command.
- **Lesson:** exactly ONE pointer ("current-context") holds the selection. No `default` separate from current. Race: two parallel kubectl shells, one runs `use-context`, the other's next command sees the new value.

### gh CLI multi-account switch ([docs](https://github.com/cli/cli/blob/trunk/docs/multiple-accounts.md))

- `gh auth login` is additive — never overwrites an existing account.
- One account per host is `active`; `gh auth switch` rotates it.
- "Active account is global, not per-directory or per-repo."
- **Lesson:** `active` is the only sticky pointer needed for the common case.

### Synthesis for Brain

The user explicitly asked for **both** `default` and `active`. Reading that with the three references in mind:

- `default` = factory-default. Set on first-ever auto-mint or via `\agent set-default <name>`. **Used as a fallback** when there's no active agent (config was just edited by hand to remove `active`, or the active agent was deleted).
- `active` = current selection. Updated on `\agent use <name>`. **The primary resolution source** when no flag/env passed.

This gives Brain a clean two-tier story: `--agent` flag > env > active > default > auto-mint. Matches the user's stated points 3 + 5 + 2 verbatim.

Alternative considered: collapse to just `active` (kubectl/gh model). Rejected because the user explicitly named `default` as a separate field. The two-tier design also makes "I want to come back to my main agent" a single command (`\agent use default-name`) without scanning the file.

## 4. Architecture

### 4.1 On-disk schema

```toml
# ~/.config/brain/config.toml

[settings]
output = "table"
timing = false
server = "127.0.0.1:9090"

[agents.work]
id = "01927a8b-4c2f-7000-8000-deadbeeffeed"
created_at = "2026-05-20T01:23:01Z"
note = "primary work account"
default = true                              # ← new
active = false                              # ← new

[agents.demo]
id = "01927c01-..."
created_at = "2026-05-20T02:00:00Z"
note = ""
default = false
active = true                               # ← new
```

### 4.2 Schema invariants (enforced on every save)

1. **At most one** agent has `default = true`. If multiple, save returns an error.
2. **At most one** agent has `active = true`. Same.
3. **At least one** agent has `default = true` whenever `agents` is non-empty (new on first-create; resurrects via auto-promote if the default agent is deleted).
4. The `active` agent (if any) MUST exist in `[agents.*]`. Renaming or deleting an agent that's active is allowed but flips `active` to the default agent in the same write.

These are enforced in `Config::save` via a `validate()` step that runs before the file is rewritten. Validation failure is fail-stop — the save aborts and the in-memory state stays consistent. No half-written files.

### 4.3 Resolution precedence

Current (after K1 + cffcc0d):
```
--agent name → file lookup
--agent-id   → raw uuid
BRAIN_AGENT  → file lookup
BRAIN_AGENT_ID → raw uuid
(else)       → mint ephemeral UUIDv7, don't persist
```

New:
```
--agent name → file lookup
--agent-id   → raw uuid
BRAIN_AGENT  → file lookup
BRAIN_AGENT_ID → raw uuid
active (config) → use it
default (config) → use it
(else)       → auto-mint + persist + mark default + mark active
```

The auto-mint case is the user's point #2 — first-ever `brain` run with no config does the right thing automatically.

### 4.4 Auto-mint behavior

When no flag, no env, no active, no default:

1. Mint a UUIDv7.
2. Build a name: `agent-<first 8 hex chars>` (e.g. `agent-01927a8b`). Memorable enough to type; deterministically derived from the UUID so the same machine reliably regenerates the same name if the file is deleted (it won't, but the property is nice).
3. Load (or create) the config file.
4. Create the `[agents.agent-<hex>]` section with `default = true, active = true, created_at = <now>`.
5. Save and proceed with this agent.

Output to the user:
```
note: first run — created and selected agent `agent-01927a8b`
note: stored at ~/.config/brain/config.toml. Mark another as default with `brain agent set-default <name>`.
```

If the config file already has agents but none is `active`/`default` (manual edit, deletion, etc.), auto-mint does NOT fire — instead, the resolver picks the first agent by `created_at` and marks it `default` + `active`, prompts via stderr that it did so. This is the "self-healing" behavior; never crash, never lose work.

### 4.5 `\info` meta command

```
brain> \info

Server
  address           127.0.0.1:9090
  server_id         brain-server/1.0.0
  wire_version      2
  server_time       2026-05-20T01:23:01Z (clock skew: +12 ms)
  shards            4

Agent (active)
  name              demo
  id                01927c01-...-...-...-...
  source            config: active
  default           no
  note              ""
  created_at        2026-05-20T02:00:00Z

Connection
  state             authenticated
  bound_shard       2
  connected_at      2026-05-20T03:15:00Z
  recent_ops        47

Session
  output            wide
  sticky_context    7
  active_txn        none
```

Source is the `WELCOME` frame for the server block + the resolver's `AgentIdSource` for the agent block + the SDK's `Client` state for connection + the shell's `Session` for the bottom block. No new wire op needed — the WELCOME already carries everything.

### 4.6 Module layout

Current single file `crates/brain-shell/src/cli/agent_id.rs` (464 lines) is doing resolver + types + tests. Split per the folder-layout rule:

```
crates/brain-shell/src/cli/agent/
├── mod.rs                  # re-exports
├── config.rs               # AgentEntry struct, default/active invariants, save validation
├── resolve.rs              # resolve() / resolve_with() + ResolveInputs / ResolveError
├── auto_mint.rs            # the new auto-mint logic
└── commands.rs             # \agent {list, show, use, create, set-default} handlers
```

Old `crates/brain-shell/src/cli/agent_id.rs` is deleted; existing callers `use brain_shell::cli::agent::*`. Existing `crates/brain-shell/src/cli/config.rs` keeps the `[settings]` half and re-exports the agent half from the new module.

### 4.7 `\agent use <name>` persists the switch

Today's behavior (current `\agent use`):
- Sets `session.sticky_agent = Some(name)` in memory.
- Forces the SDK client to reconnect with the new auth.
- Doesn't touch the config file.

New behavior:
- Same two steps.
- Additionally: load config, flip the matching entry's `active = true` (and others to `active = false`), save.
- On next bare `brain` start, the persisted `active` flag selects the same agent.

This is the AWS-CLI-style "session env" + kubectl's "use-context writes to file" combined. Persistence is the user's explicit point #5.

### 4.8 `\agent set-default <name>` — new meta

User-facing escape hatch to change the default without leaving the REPL:

```
brain> \agent set-default work
default agent → work
```

Persists immediately. Doesn't touch `active`.

### 4.9 `brain agent set-default <name>` — one-shot CLI subcommand

For non-REPL flows. Same behavior. Drops into the existing `agent` subcommand tree alongside `create`, `list`, `show`, `delete`.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| **A. Two fields: `default` + `active`** (per user spec) | Matches user's explicit ask; "factory default" survives session mutation; simple mental model | Two invariants to maintain; small risk of file ending up with both fields off (mitigated by auto-promote at load) | ✓ chosen |
| B. kubectl-style: only `current-context` (== `active`) | One field, one invariant; matches the most-popular reference | Loses "factory default" concept; user has to scan the file to find their main agent | rejected — user explicitly asked for both |
| C. AWS-style: implicit `[default]` profile is special-named | Familiar to AWS users | Conflates name with role; can't rename the default; doesn't address "active" at all | rejected — name + role should be separate |
| D. gh-style: `active` per-host, but Brain has one host | Cleanest for a single-host product | Still doesn't give the user a "factory default" | rejected — same as B |
| E. Auto-mint without persisting (status quo with K1) | No file mutation; ephemeral | The user's #2 ask is exactly the opposite — they want persistence | rejected — that's the current behavior that prompted this plan |

## 6. Risks / open questions

1. **Race: two parallel shells both call `\agent use`.** Last writer wins on the file; the other shell's in-memory `sticky_agent` doesn't notice. Mitigation: file lock during save (advisory `flock` on the config dir). Documented in code; if the lock is contended, save retries 3× with backoff then errors clearly.

2. **Migration of existing configs.** The current `AgentEntry { id, created_at, note }` shape needs `default: bool` and `active: bool` added. serde `#[serde(default)]` deserializes missing fields as `false`. Migration logic at load: if the loaded config has agents but no `default = true`, promote the oldest (by `created_at`) to default; if no `active = true`, promote the default to active too. Persist the migration on the next save — but NOT eagerly at load (load is read-only by convention; users running `brain --help` shouldn't get a silent file rewrite).

3. **What name format for auto-mint?** Options: (a) `agent-<8 hex>` derived from UUID — terse, deterministic. (b) Humanish like `fox-river-37` — requires a names crate. (c) just the UUID short form. Recommendation: (a). No new dep; still typeable for `\agent use`.

4. **What does `\info` show when not connected yet?** Server block prints `(not connected)` for everything; agent block prints the resolved (but unsent) name + id; connection block prints `state: disconnected`. Lets the user debug "I think I'm pointed at the wrong agent" without forcing a connect.

5. **Should the `\info` server section make a fresh round-trip, or just echo the cached WELCOME?** Recommendation: echo cached WELCOME. The connection layer caches it; surfacing the live time would require a wire op (`HEALTH_REQ`-style) that doesn't exist today. Add a `--refresh` flag later if anyone asks.

6. **`brain agent delete <name>`'s interaction with `default` / `active`.** Already covered by invariant 4 (deleting an active agent flips active to default, deleting a default agent promotes the oldest remaining). Need to document the prompt behavior: if the agent being deleted is `default`, ask "are you sure? this will promote <oldest>" unless `--yes` is passed.

7. **brain-cli vs brain-shell.** brain-cli is the admin/operator tool; it doesn't issue ENCODE today (per the brain-explore migration plan §1). Does it need access to the same agent config? Probably yes for `brain-cli agent list` / `brain-cli agent create` (admin-side mutations). But the resolution precedence for brain-cli's own work (calling admin endpoints) is **different** — admin ops use a server-side admin token, not an agent. Keep the agent CRUD in brain-shell for v1; brain-cli stays out of it.

8. **What if the user runs `brain --agent-id <uuid>` where the UUID isn't in the config?** Today: works fine, source is `IdFlag`, no file touched. New plan keeps this: `--agent-id` is the "raw escape hatch" — uses the UUID directly without resolving against the config. No auto-create. This matches AWS's `AWS_ACCESS_KEY_ID` env semantics (raw cred, not a profile name).

9. **Where does the `WELCOME` data live so `\info` can read it?** `crates/brain-sdk-rust/src/client/` already caches the handshake state. Need a public accessor like `Client::welcome() -> Option<&WelcomePayload>`. If absent, add one.

## 7. Test plan

Map each "Done when" item to tests.

### Resolver
- [ ] `resolve_with_active_set_returns_active`
- [ ] `resolve_with_default_only_returns_default`
- [ ] `resolve_with_no_agents_auto_mints_and_persists` (using `tempdir`)
- [ ] `resolve_flag_id_bypasses_config_lookup` (existing test, keep)
- [ ] `resolve_env_id_bypasses_config_lookup` (existing test, keep)
- [ ] `resolve_precedence_flag_beats_env_beats_active_beats_default`
- [ ] `resolve_with_orphaned_active_field_falls_back_to_default`

### Config invariants
- [ ] `save_rejects_two_defaults`
- [ ] `save_rejects_two_actives`
- [ ] `save_rejects_active_naming_nonexistent_agent`
- [ ] `load_promotes_oldest_to_default_when_none_marked`
- [ ] `load_promotes_default_to_active_when_no_active`
- [ ] `legacy_file_without_default_or_active_loads_clean`

### Auto-mint
- [ ] `auto_mint_creates_named_agent_with_both_flags`
- [ ] `auto_mint_format_is_agent_dash_8hex`
- [ ] `auto_mint_writes_atomically` (write to tmp, rename — kill mid-write should leave old file intact)

### `\agent use <name>` persistence
- [ ] `agent_use_flips_active_in_config`
- [ ] `agent_use_followed_by_restart_picks_new_active`
- [ ] `agent_use_unknown_name_errors_no_file_mutation`

### `\agent set-default <name>`
- [ ] `set_default_flips_default_in_config`
- [ ] `set_default_does_not_change_active`

### `\info`
- [ ] `info_renders_server_block_from_cached_welcome`
- [ ] `info_renders_disconnected_state_cleanly`
- [ ] `info_renders_agent_source` (each `AgentIdSource` variant)

### Existing behavior preserved
- [ ] `cli::agent_id::tests::bare_resolution_returns_ephemeral` — this test (fixed in K1) needs to be REWRITTEN. After this plan, bare resolution returns either the active/default config agent OR auto-mints. The K1 ephemeral path no longer fires unless something pathological happened (no HOME, can't write config). New test name: `bare_resolution_creates_persisted_agent_on_first_run`.

## 8. Commit shape

Four commits:

1. **`refactor(shell): split cli/agent_id.rs into cli/agent/ module`** (~100 LOC moved). Pure module re-layout; no behavior change. Existing tests pass unchanged. Makes the rest of the work scrutable.

2. **`feat(shell): persist default + active on AgentEntry; enforce invariants`** (~250 LOC). Adds the two bool fields, the validate() step on save, the load-time auto-promote, all the new config tests. No resolver change yet — file format gets richer but resolver still picks via flags/env only.

3. **`feat(shell): auto-mint + active/default resolution; persist \agent use`** (~300 LOC). The behavior-change commit. Updates `resolve_with` to consult `active`/`default`, adds `auto_mint.rs`, updates `\agent use` to write the active flag. New `\agent set-default` meta + matching `brain agent set-default` CLI subcommand. Existing K1 ephemeral-mint behavior moves to a `--no-persist` flag (rarely used: scripts that want a fresh per-invocation identity).

4. **`feat(shell): \info meta command + WELCOME accessor on Client`** (~200 LOC). Adds `Client::welcome() -> Option<&WelcomePayload>` to brain-sdk-rust if missing. Implements `\info` rendering through brain-explore (new `render/info.rs` impl Render). Tests for connected + disconnected states.

Commits 1 + 2 are sequential; 3 depends on 2; 4 can run in parallel with 3 (touches different files).

## 9. Confirmation

Three judgment calls to sign off on:

1. **Two fields, both persisted: `default` AND `active`.** §4 + §5 row A. The user explicitly asked for both — confirming the semantic split: default = "factory default I want to come back to," active = "currently picked, updated by `\agent use`." Rejected the simpler kubectl-style single-pointer model because the user named both.

2. **Auto-mint name format = `agent-<first 8 hex of UUID>`.** §4.4 + §6 Q3. No new deps, deterministic, typeable. The alternative (`fox-river-37` style) needs a names crate; preferable to keep the dep set tight. Confirm OK.

3. **`\agent use` persists to the config file.** §4.7. This is the user's point #5. Side effect: two parallel shells that both run `\agent use` will see last-writer-wins on the file but each remembers its own in-memory state. Acceptable per the file-lock mitigation in §6 Q1. Confirm OK.

After sign-off, I'll convert §8 into tasks and start with Commit 1 (the pure module refactor). The behavioral commits 2-4 land after that base is stable.

## 10. Cross-tool comparison (summary)

| Tool | Default | Active | Switch persists? | Auto-mint on first run? |
|---|---|---|---|---|
| AWS CLI | `[default]` section name | implicit (per-command/session) | no | no |
| kubectl | none — only `current-context` | yes (`current-context`) | yes (`use-context`) | no |
| gh CLI | none — only "active" per host | yes | yes (`auth switch`) | no |
| **Brain (this plan)** | `default: bool` field | `active: bool` field | yes (`\agent use`) | yes (point #2) |

The auto-mint-on-first-run is the unique Brain choice. Justified by Brain's deployment posture: brain-shell is the primary user-facing surface, and "fail to start because config is empty" is a bad first-run experience for a substrate that wants to feel as approachable as `redis-cli` (which doesn't have agents at all).

Sources:
- [AWS CLI named profiles](https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-files.html)
- [kubectl config use-context](https://kubernetes.io/docs/reference/kubectl/generated/kubectl_config/kubectl_config_use-context/)
- [gh CLI multiple accounts](https://github.com/cli/cli/blob/trunk/docs/multiple-accounts.md)
