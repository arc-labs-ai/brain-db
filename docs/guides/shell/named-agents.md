# Multi-agent workflows with `brain agent`

Brain treats `agent_id` as a first-class noun — it's stamped on
every memory, every event, every subscribe envelope. Named agents
in the `brain` shell are the ergonomic layer: instead of typing
ULIDs, you give each identity a short local name and let the shell
look it up.

For the reference (file schema, resolution precedence), see
[`../../reference/shell/configuration.md`](../../reference/shell/configuration.md).

This guide covers the **workflows**:

1. Creating your first named agent
2. Switching between agents inside one REPL
3. Sharing an agent id with a colleague
4. Subscribing to *only* your agent's events on a shared shard
5. The conflict cases (and how to avoid them)

---

## 1. Create your first named agent

```bash
brain agent create work --note "prod work notebook"
# created agent 'work' (01HMK_NEW_ULID)

brain agent list
# NAME      ID                                       CREATED              NOTE
# work      01HMK_NEW_ULID                           2026-05-19T20:00:00Z prod work notebook
```

Now bind to it per-invocation:

```bash
brain --agent work encode "first work memory"
# ok  s2/m1/v1  lsn=1
#     agent=01hm… · ctx=0 · episodic · sal=0.500 · fp=00000000…

brain --agent work recall "work" --top-k 3 --include-text
# #1  s2/m1/v1  episodic  ctx=0  sal=0.500  score=0.0164
#     first work memory
#
# 1 result
```

For sticky cross-shell selection, put the env var in your shell
rc:

```bash
# ~/.zshrc or ~/.bashrc
export BRAIN_AGENT=work
```

Now `brain encode "anything"` always binds to `work`.

---

## 2. Switching agents in a REPL session

Start fresh:

```bash
brain --agent demo
brain shell — connected as 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e (via --agent demo).
brain> \agent
agent_id = 019e3d1f-bd66-7890-a4bc-947ab6ca9c3e
source   = named (demo)

brain> encode "demo memory"
ok  s2/m1/v1  lsn=1
    agent=019e… · ctx=0 · episodic · sal=0.500 · fp=00000000…
```

Switch to `work` mid-session:

```
brain> \agent use work
note: rebound session to 'work' (01HMK_WORK_ULID)

brain> \agent
agent_id = 01HMK_WORK_ULID
source   = named (work, via \agent use)

brain> encode "work memory"
ok  s2/m1/v1  lsn=2
    agent=01hm… · ctx=0 · episodic · sal=0.500 · fp=00000000…
```

Note: `\agent use` rebinds the **live** session only; it does NOT
mutate the config file. To persist, use the env var in your shell
rc instead.

**Active txn guard.** `\agent use` refuses while a txn is open:

```
brain*> \agent use other
error: active transaction prevents rebinding; commit or abort first
```

---

## 3. Sharing an agent id with a colleague

Agent ids are opaque bytes (ULID/UUIDv7), safe to share. The
pattern:

```bash
# Your machine
brain agent show work
# NAME       ID                                       CREATED              NOTE
# work       01HMK_WORK_AGENT_ULID                    2026-05-19T20:00:00Z prod work notebook

# DM/email the id to your colleague.

# Colleague's machine
brain agent import work 01HMK_WORK_AGENT_ULID
# imported agent 'work' (01HMK_WORK_AGENT_ULID)

brain --agent work recall "shared memory"
# (sees the same data — both machines bind to the same agent on
#  the substrate)
```

This works because Brain's agent_id is just a 16-byte tag; the
shell's named-agent file is purely a local nickname-to-id mapping.

---

## 4. Subscribing only to YOUR agent's events on a shared shard

This is the **most important production pattern**. On a multi-
tenant shard, every encode/forget by every agent publishes to the
shared EventBus. Without an agent filter, every subscriber sees
every other agent's events.

The `--agents` filter scopes a subscription:

```bash
brain --agent work subscribe --agents $(brain --agent work agent show work | awk '/^work / {print $2}')
```

Cleaner with `jq`:

```bash
MY_ID=$(brain --agent work --output json agent show work | jq -r .id)
brain --agent work subscribe --agents "$MY_ID"
```

Now the subscriber only sees `work`'s events even if `demo` is
encoding into the same shard.

You can also pass multiple agent ids to follow several:

```bash
brain subscribe --agents 01HMK_AGENT_A --agents 01HMK_AGENT_B
```

(The filter is server-side; matched as a `HashSet::contains` per
event — see [`../../reference/shell/commands.md#subscribe`](../../reference/shell/commands.md#subscribe).)

---

## 5. Conflict cases

### `--agent` AND `--agent-id` together

```bash
$ brain --agent work --agent-id 01HMK_OTHER encode "x"
error: --agent and --agent-id are mutually exclusive; pick one
```

The shell never guesses. Use one.

### `BRAIN_AGENT` AND `BRAIN_AGENT_ID` together

```bash
$ BRAIN_AGENT=work BRAIN_AGENT_ID=01HMK_OTHER brain encode "x"
error: BRAIN_AGENT and BRAIN_AGENT_ID are both set; unset one
```

Same rule for env vars. The error message names both so you know
which is leftover from your rc file.

### Deleting the agent your session is using

```bash
$ brain --agent work agent delete work
error: cannot delete 'work' — current invocation is bound to it
hint: bind to a different agent (--agent NAME or unset BRAIN_AGENT) and retry
```

Guard against the foot-gun of evicting your own access.

### Unknown agent

```bash
$ brain --agent wokr recall "x"
error: unknown agent 'wokr'. Try `brain agent list` to see known agents, or `brain agent create wokr`.
```

Levenshtein-nearest hint, plus the actionable next step.

---

## When NOT to use named agents

- **Ephemeral exploration:** `brain` with no `--agent` / env mints a
  fresh ULID per session. Encodes don't survive — perfect for poking
  at a fresh deployment.
- **Tests / scripts that should be isolated per run:** keep ephemeral
  binding; named agents would pollute prod data.
- **Single-user laptops:** if there's only ever one identity, `BRAIN_AGENT_ID`
  with a hand-typed ULID is fewer moving parts than a config file.

---

## See also

- [`../../reference/shell/configuration.md`](../../reference/shell/configuration.md) — file schema + resolution rules
- [`../../reference/shell/repl-meta.md`](../../reference/shell/repl-meta.md) — `\agent` meta-commands
- [`subscribe-and-replay.md`](subscribe-and-replay.md) — chaining `recall.lsn` into `subscribe`
- [`troubleshooting.md`](troubleshooting.md) — when things misbehave
