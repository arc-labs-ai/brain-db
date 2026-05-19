# `brain` shell — REPL meta-commands

Backslash-prefixed commands and a few un-prefixed aliases. These
are intercepted *before* `clap` parses, so they never round-trip
to the server. Companion to [`commands.md`](commands.md) (server
verbs) and [`configuration.md`](configuration.md) (persistent
state).

## Session control

| Command | What it does |
|---|---|
| `quit`, `exit`, `\q`, `\quit`, Ctrl-D | Exit the shell cleanly. |
| `help [VERB]`, `? [VERB]`, `\?`, `\help` | In-REPL help (psql aliases). With `<VERB>`, shows that verb's flags. |
| `\connect <host:port>` | Reconnect to a different server. Drops the current session (incl. any active txn locally; the server-side txn is left to time out). |

## Settings — live session

These mutate the **live session only**. They do not touch
`~/.config/brain/config.toml`. For persistence, use
`\config set …` (see below) or the one-shot `brain config set …`.

| Command | What it does |
|---|---|
| `\set output json\|table` | Toggle output format for this session. |
| `\set context <N>` | Sticky `--context` default — auto-applied to subsequent `encode`/`recall` unless overridden. Prompt shows `brain[ctx=7]> `. |
| `\unset context` | Clear sticky context. |
| `\unset txn` | Clear sticky txn (does **not** abort it on the server — the txn ages out on its own timeout). |
| `\timing on\|off` | Show per-op wall time after each command. |

## Settings — persistent

`\config` and `\agent` mirror the one-shot `brain config` / `brain
agent` subcommands, but `\config set` *also* mutates the live
session (mongosh-style "set + persist").

| Command | What it does |
|---|---|
| `\config list` | Print effective merged settings (file + defaults). |
| `\config get <KEY>` | Print one value. |
| `\config set <KEY> <VALUE>` | Validate, write `~/.config/brain/config.toml`, AND mutate the live session. |
| `\config path` | Print the config file path. |
| `\config edit` | Open the file in `$EDITOR` (→ `$VISUAL` → `vi`). |

## Named agents

| Command | What it does |
|---|---|
| `\agent` | Print the **current binding** — agent id + source (named / id-flag / env / ephemeral). |
| `\agent list` | Table of all configured agents with `*` on the current one. |
| `\agent show [<NAME>]` | Full record (name, id, created_at, note). Omit `<NAME>` for the current binding. |
| `\agent create <NAME> [--note <TEXT>]` | Mint a fresh ULID, write `[agents.<name>]` in the config file. |
| `\agent rename <OLD> <NEW>` | Atomic rename. Refuses on name collision. |
| `\agent delete <NAME>` | Remove the entry. Blocks if `<NAME>` is the agent the current session is bound to. |
| `\agent import <NAME> <ULID>` | For sharing — colleague gives you a ULID, you give it a local name. |
| `\agent use <NAME>` | **Rebind the live session** to `<NAME>`. Refuses if `active_txn.is_some()` (commit/abort first). Does **not** write to the config file — use `BRAIN_AGENT=<name>` in your shell rc for sticky cross-session selection. |

See [`configuration.md`](configuration.md) for the named-agent
file shape and the resolution-precedence rules.

## Prompt encoding

The REPL prompt reflects state at a glance:

| Prompt | Meaning |
|---|---|
| `brain> ` | No active txn, no sticky context. |
| `brain*> ` | Active transaction (sticky txn_id). |
| `brain[ctx=7]> ` | Sticky context = 7. |
| `brain*[ctx=7]> ` | Both. |

The connection banner at REPL entry also shows the bound agent:

```
brain shell — connected to 127.0.0.1:9090 as 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e (via --agent demo).
Type `help` for commands, `quit` to exit.
```

The "via …" suffix indicates the **resolution source**:

| Suffix | Meaning |
|---|---|
| `via --agent <name>` | `--agent` flag. |
| `via --agent-id <ulid>` | `--agent-id` flag. |
| `via BRAIN_AGENT` env | Env variable. |
| `via BRAIN_AGENT_ID` env | Env variable (raw id). |
| `ephemeral` | No selection; minted a fresh UUID for this session. |

## History

Persistent history at:

```
$XDG_DATA_HOME/brain/history          # or
~/.local/share/brain/history          # XDG default
~/.brain_history                      # fallback
```

Loaded on REPL start; appended after each entered line.

## Completion (REPL)

Tab cycles through:

1. **Subcommands** at the start of a line — `enc<TAB>` → `encode`.
2. **Flag names** after a subcommand — `encode "x" --c<TAB>` → `--context`.
3. **Enum values** after an enum-flag — `encode "x" --kind <TAB>` →
   `episodic | semantic | consolidated`.

For tab completion in **non-REPL** shells (bash/zsh/fish), use
`brain generate-completion <SHELL>` — see
[`commands.md#generate-completion`](commands.md#generate-completion).

## See also

- [`commands.md`](commands.md) — server verbs
- [`configuration.md`](configuration.md) — config file + agent CRUD
- [`output-formats.md`](output-formats.md) — table vs JSON
- [`../../guides/shell/named-agents.md`](../../guides/shell/named-agents.md) — task-oriented walkthrough
