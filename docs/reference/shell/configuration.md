# `brain` shell — configuration

The shell keeps two pieces of state on disk:

```
~/.config/brain/config.toml          # named agents + settings
~/.local/share/brain/history         # REPL history (XDG-honouring)
```

Both paths respect `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME`. The
config file is created on first run with permissions `0o600`.

This page documents the config file schema, the agent-resolution
precedence, and the `agent` / `config` command surfaces. For
*workflow* (multi-agent flows, sharing IDs with a colleague), see
[`../../guides/shell/named-agents.md`](../../guides/shell/named-agents.md).

---

## File schema

```toml
# ~/.config/brain/config.toml
#
# Edit by hand or via `brain config set` / `brain agent create`.
# Delete the file to reset everything.

[settings]
output = "table"            # "json" | "table"
timing = false              # bool
sticky_context = 5          # u64 (optional)
server = "127.0.0.1:7878"   # last `config set server` value

# Named agents. Add via `brain agent create <name>` or by hand.
# Names are TOML keys: O(1) lookup, duplicate names are a parse error.

[agents.work]
id = "01HMK…ULID"
created_at = "2026-05-19T10:00:00Z"
note = "prod work notebook"

[agents.demo]
id = "01HMK…ULID"
created_at = "2026-05-19T11:30:00Z"
```

### `[settings]` keys

| Key | Type | Default | Notes |
|---|---|---|---|
| `output` | `"table"` \| `"json"` | (auto by TTY) | Default output format. Overridden by `--output`. |
| `timing` | bool | `false` | Show per-op wall time. Overridden by `\timing on/off`. |
| `sticky_context` | `u64` (optional) | unset | Default `--context` for `encode`/`recall`. Overridden by `--context`. |
| `server` | string | `127.0.0.1:9090` | Endpoint. Overridden by `--server`, `BRAIN_SERVER`. |

**Unknown keys are rejected** with a "did you mean…" hint based on
Levenshtein distance. The schema is closed by design so typos
don't silently no-op.

### `[agents.<name>]` keys

| Key | Type | Notes |
|---|---|---|
| `id` | ULID string | Stable agent identifier. Must be valid UUIDv7 / ULID. |
| `created_at` | ISO-8601 timestamp | Set by `agent create`. |
| `note` | string (optional) | Human-readable label, shown in `agent list`. |

---

## Agent resolution precedence

Per invocation, evaluated **highest → lowest**:

1. `--agent <name>` flag → look up `[agents.<name>]`; **error if missing**.
2. `--agent-id <ulid>` flag → raw id, no file touched.
3. `BRAIN_AGENT=<name>` env → same lookup as (1); **error if missing**.
4. `BRAIN_AGENT_ID=<ulid>` env → raw id.
5. **Fresh ephemeral ULID**, minted at connect, discarded at quit.

**Conflicts:**

- `--agent` AND `--agent-id` together → error.
- `BRAIN_AGENT` AND `BRAIN_AGENT_ID` together → error.

The error messages name both sources explicitly so you don't
have to guess which one wins.

---

## CLI: `brain agent …`

All `agent` subcommands act on the config file at the path printed
by `brain config path`. Writes are atomic (`tempfile + rename`,
preserving `0o600`).

```
brain agent list
brain agent show [<NAME>]
brain agent create <NAME> [--note <TEXT>]
brain agent rename <OLD> <NEW>
brain agent delete <NAME>
brain agent import <NAME> <ULID>
```

| Command | Output |
|---|---|
| `agent list` | Table: `NAME · ID · CREATED · NOTE`. The agent the current invocation would use is marked with `*`; ephemeral runs show `<ephemeral>`. |
| `agent show [<NAME>]` | Full record. `<NAME>` omitted → show whatever the next connect would bind to. |
| `agent create <NAME> [--note <TEXT>]` | Mints a fresh ULID, writes `[agents.<name>]`, echoes the new id. Name collision → error. |
| `agent rename <OLD> <NEW>` | Atomic in one write txn. |
| `agent delete <NAME>` | Removes the entry. Blocks if the *current invocation* uses that agent — protects the live session's writes. |
| `agent import <NAME> <ULID>` | For sharing — a colleague passes you a ULID, you give it a local name. |

The REPL has additionally `\agent use <NAME>` to **rebind the live
session** to a named agent — see [`repl-meta.md`](repl-meta.md).
This does **not** write to the config file; for sticky cross-
session selection use `BRAIN_AGENT=<name>` in your shell rc.

---

## CLI: `brain config …`

```
brain config list                                # effective merged settings
brain config get <KEY>
brain config set <KEY> <VALUE>
brain config path                                # print the config path
brain config edit                                # $EDITOR → $VISUAL → vi
```

`brain config set` validates against the closed schema. Invalid
values surface as:

```
$ brain config set output yaml
error: invalid value 'yaml' for 'output' (allowed: "table", "json")
```

Inside the REPL, `\config set <KEY> <VALUE>` mutates the live
session *and* persists.

---

## Migration of pre-named-agents config

The previously-shipped file shape was `agent_id = "<ulid>"`. On
first run after upgrading:

1. Backup created: `config.toml.bak-YYYYMMDDHHMMSS`.
2. File rewritten as `[agents.default]\nid = "<that ulid>"\nnote = "migrated from legacy singleton"\ncreated_at = <now>`.
3. Stderr line: `note: migrated legacy config.toml to named-agent schema; backup at <path>`.
4. The migrated `default` agent is **not** auto-selected (ephemeral remains the default). To reach yesterday's memories, run `brain --agent default` or set `BRAIN_AGENT=default`.

A malformed config file refuses to start — the message points at
`brain config path` and the backup convention.

---

## Sharing an agent id across machines / colleagues

The id is opaque bytes; ULIDs are safe to share. The pattern:

```bash
# Machine A
brain --agent work agent show work
# → id = 01HMK_PROD_AGENT_ULID

# Machine B
brain agent import work 01HMK_PROD_AGENT_ULID
brain --agent work recall "shared memory"
```

Both machines now bind to the same agent on the substrate; encodes
from either show up in recalls from either.

---

## See also

- [`../brain-shell.md`](../brain-shell.md) — overview
- [`commands.md`](commands.md) — full command list
- [`repl-meta.md`](repl-meta.md) — `\agent`, `\config` meta-commands
- [`../../guides/shell/named-agents.md`](../../guides/shell/named-agents.md) — walkthrough
