# `brain` shell — reference subdirectory

In-depth, look-it-up reference for the `brain` interactive shell.
For the overview + quick-start, start at
[`../brain-shell.md`](../brain-shell.md).

| Page | What's in it |
|---|---|
| [`commands.md`](commands.md) | Per-verb reference: `encode`, `recall`, `plan`, `reason`, `forget`, `link`, `unlink`, `txn`, `subscribe`, `agent`, `config`, `shell`, `generate-completion`. Every flag, every output field. |
| [`repl-meta.md`](repl-meta.md) | Backslash meta-commands: `\agent`, `\config`, `\set`, `\unset`, `\timing`, `\connect`. Prompt encoding. History + completion paths. |
| [`output-formats.md`](output-formats.md) | Table vs JSON; per-verb JSON schemas; memory-id short vs long form; streaming subscribe shape. |
| [`configuration.md`](configuration.md) | `~/.config/brain/config.toml` schema, agent resolution precedence, `brain agent …` / `brain config …` CLI, migration. |
| [`errors.md`](errors.md) | Wire error codes → rendered messages → exit codes. Common cases with remedies. |

For *task-oriented* how-tos (named-agent workflows, subscribe
replay chaining, scripting with jq), see
[`../../guides/shell/`](../../guides/shell/).

For a 20-minute guided walkthrough, see
[`../../tutorials/03-shell-deep-dive.md`](../../tutorials/03-shell-deep-dive.md).
