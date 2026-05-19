# `brain` — interactive shell (overview)

The `brain` binary is the `psql` / `redis-cli` / `mongosh` equivalent
for the Brain cognitive substrate. It speaks the binary wire protocol
on `listen_addr` (default `127.0.0.1:9090`) via the Rust SDK. Two
modes:

- **REPL** — `brain` with no subcommand drops you into a prompt.
- **One-shot** — `brain <verb> [args]` runs a single op and exits.

Source: `crates/brain-shell/`. Both modes share one `clap` command
tree, so flag names, defaults, and `--help` are identical.

> For the **admin** CLI (HTTP `/v1/*` routes — snapshots, audit,
> worker control), see [`cli.md`](cli.md) (`brain-cli`).

This page is the **overview**; the deep references live under
[`shell/`](shell/).

| You want | Read |
|---|---|
| The full command list and every flag | [`shell/commands.md`](shell/commands.md) |
| REPL meta-commands (`\agent`, `\config`, `\set`, …) | [`shell/repl-meta.md`](shell/repl-meta.md) |
| Output shapes (table layout, JSON schema per verb) | [`shell/output-formats.md`](shell/output-formats.md) |
| `~/.config/brain/config.toml` + named agent management | [`shell/configuration.md`](shell/configuration.md) |
| Error frames → exit codes + common gotchas | [`shell/errors.md`](shell/errors.md) |

For **task-oriented** how-tos (multi-agent workflows, `recall →
subscribe` chaining, bulk encode, jq pipelines, troubleshooting),
see [`../guides/shell/`](../guides/shell/).

For a **20-minute guided tour**, see
[`../tutorials/03-shell-deep-dive.md`](../tutorials/03-shell-deep-dive.md).

---

## Quick start

```bash
# One-shot — table output by default when stdout is a TTY,
# JSON by default when piped.
brain encode "Alice merged the auth-rewrite branch" --context 7 --salience 0.7
# ok  s2/m1/v1  lsn=1
#     agent=00000000… · ctx=7 · episodic · sal=0.700 · fp=00000000…

brain recall "auth rewrite" --top-k 5 --include-text
# #1  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0164
#     Alice merged the auth-rewrite branch
#
# 1 result

# REPL
brain
brain> encode "hello" --context 1
brain> recall "hello" --top-k 3 --include-text
brain> \agent
brain> quit
```

## Invocation

```
brain [OPTIONS] [<COMMAND> [ARGS]]
```

With no `<COMMAND>` (or with `shell`), enter the REPL. Otherwise
run the one-shot.

## Global options

| Option | Default | Notes |
|---|---|---|
| `--server <host:port>` | `127.0.0.1:9090` | Wire-protocol endpoint. Match `[server] listen_addr` in your TOML. Also reads `BRAIN_SERVER` env. |
| `--agent <name>` | — | **Named agent** lookup against `~/.config/brain/config.toml`. Error if name unknown (with did-you-mean hint). Reads `BRAIN_AGENT` env. |
| `--agent-id <UUID>` | random UUIDv7 (ephemeral) | Raw agent id, no config-file touch. Reads `BRAIN_AGENT_ID` env. |
| `--output <table\|json>` | `table` (TTY) / `json` (piped) | Output format. JSON is one line per command, wrapped as `{ "op": <verb>, "result": <body> }`. |
| `--timeout <SECS>` | `30` | Per-op wall-clock budget. |
| `--token <VALUE>` | — | Reserved for v2 auth (parsed and ignored in v1). |
| `--help`, `-h` | — | Print help (also works on each subcommand). |
| `--version`, `-V` | — | Print version. |

**Conflicts**: `--agent` and `--agent-id` together → error;
`BRAIN_AGENT` and `BRAIN_AGENT_ID` together → error. Pick one.

## Verb summary

The cognitive primitives plus session/admin commands. Each links
to its deep reference in [`shell/commands.md`](shell/commands.md).

| Verb | What it does | Read |
|---|---|---|
| `encode <TEXT>` | Write a memory + return id + WAL `lsn` | [commands.md#encode](shell/commands.md#encode) |
| `recall <QUERY>` | Vector-similarity search; returns ranked memories | [commands.md#recall](shell/commands.md#recall) |
| `plan <FROM> <TO>` | Stepwise path from one state to another | [commands.md#plan](shell/commands.md#plan) |
| `reason <OBSERVATION>` | Inference chain from observed evidence | [commands.md#reason](shell/commands.md#reason) |
| `forget <ID>` | Tombstone a memory (soft or hard) | [commands.md#forget](shell/commands.md#forget) |
| `link <SRC> <KIND> <TGT>` | Add a typed edge | [commands.md#link](shell/commands.md#link) |
| `unlink <SRC> <KIND> <TGT>` | Remove an edge | [commands.md#unlink](shell/commands.md#unlink) |
| `txn begin\|commit\|abort` | Multi-op atomic batch | [commands.md#txn](shell/commands.md#txn) |
| `subscribe [--start-lsn N]` | Live + replay event stream | [commands.md#subscribe](shell/commands.md#subscribe) |
| `agent create\|list\|show\|use\|rename\|delete\|import` | Named-agent CRUD | [shell/configuration.md](shell/configuration.md) |
| `config list\|get\|set\|path\|edit` | Persistent shell settings | [shell/configuration.md](shell/configuration.md) |
| `shell` | Explicit REPL entry (same as bare `brain`) | — |
| `generate-completion <SHELL>` | Emit bash/zsh/fish/powershell completion | — |

## Output formats at a glance

`brain` auto-picks based on stdout:

| Stdout | Default | Reason |
|---|---|---|
| Interactive TTY | `table` | Human reading the REPL or one-shot at a terminal |
| Piped / redirected | `json` | Script consumer; safe for `jq`, `python -m json.tool`, etc. |

Override with `--output <FORMAT>`. Per-verb schemas are in
[`shell/output-formats.md`](shell/output-formats.md).

### Table — `encode` (two-line)

```
ok  s2/m1/v1  lsn=1
    agent=00000000… · ctx=7 · episodic · sal=0.700 · fp=00000000…
```

### Table — `recall` (two-line per result + footer)

```
#1  s2/m1/v1  episodic  ctx=7  sal=0.700  score=0.0164
    Alice merged the auth-rewrite branch

#2  s2/m2/v1  semantic  ctx=7  sal=0.900  score=0.0161
    auth tokens now use BLAKE3 instead of SHA-1

2 results  ·  scores tightly clustered (Δ<0.001) — ranking may not be meaningful
```

The **memory-id short form** `s{shard}/m{slot}/v{version}` is the
same id you can pass back to `forget`, `link`, etc. — see
[shell/output-formats.md](shell/output-formats.md#memory-ids).

The **cluster warning** fires when every top-K score is within
`Δ<0.001` of the highest — typically means your embedder isn't
discriminating (or you're on the `NopDispatcher` test path).

## Prompt

The REPL prompt encodes session state:

| Prompt | Meaning |
|---|---|
| `brain> ` | No active txn, no sticky context. |
| `brain*> ` | Active transaction. |
| `brain[ctx=7]> ` | Sticky context = 7. |
| `brain*[ctx=7]> ` | Active txn + sticky context. |

## Persistent state

```
~/.config/brain/config.toml          # named agents + settings
~/.local/share/brain/history         # REPL history (XDG-honouring)
```

See [`shell/configuration.md`](shell/configuration.md) for the
config file schema and the `agent create/use/list` workflow.

## See also

- [`shell/commands.md`](shell/commands.md) — per-verb reference
- [`shell/repl-meta.md`](shell/repl-meta.md) — `\agent`, `\config`, `\set`, `\timing`, `\connect`
- [`shell/output-formats.md`](shell/output-formats.md) — table layout + JSON schemas
- [`shell/configuration.md`](shell/configuration.md) — config.toml + named agents
- [`shell/errors.md`](shell/errors.md) — error codes + exit codes
- [`../guides/shell/`](../guides/shell/) — task-oriented playbooks
- [`../tutorials/03-shell-deep-dive.md`](../tutorials/03-shell-deep-dive.md) — guided tour
- [`cli.md`](cli.md) — admin CLI (`brain-cli`, HTTP `/v1/*` routes)
- [`sdk-rust.md`](sdk-rust.md) — programmatic SDK that `brain` uses under the hood
- [`wire-protocol/`](wire-protocol/) — the binary protocol the shell speaks

**Spec:** §13 (SDK design). The shell is a thin wrapper over the
spec'd SDK surface.
**Source:** `crates/brain-shell/`.
