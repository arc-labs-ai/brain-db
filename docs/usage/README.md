# Brain — Local usage guide

Hands-on walkthrough of running Brain locally and exercising every
command. Each page covers:

- **What to do** — exact commands.
- **What to expect** — concrete output examples.
- **Verify** — checks that prove the step worked before moving on.

If you want the operator-facing production guide (systemd, scraping,
runbooks), see [`docs/guides/`](../guides/) instead. This directory
is for **developers running Brain locally inside the dev container**.

## Pages

| # | Page | Topic |
|---|---|---|
| 01 | [Setup](01-setup.md) | Prerequisites + clone + container + shell |
| 02 | [Build and verify](02-build-and-verify.md) | `cargo build`, `just verify`, reading the output |
| 03 | [Run the server](03-run-server.md) | Start `brain-server` with `config/dev.toml`; env overrides; the data directory |
| 04 | [Admin CLI](04-cli.md) | Every `brain` CLI command with input / output / verify |
| 05 | [Rust SDK](05-sdk.md) | Connect, encode, recall, forget, link, transactions |
| 06 | [End-to-end walkthrough](06-walkthrough.md) | 8-phase `store_and_recall` example tour |
| 07 | [Configuration](07-configuration.md) | Full `config/dev.toml` field reference |
| 08 | [Debugging](08-debugging.md) | Logs, metrics scraping, debug-snapshot, backtraces |
| 09 | [Troubleshooting](09-troubleshooting.md) | io_uring permission, port conflicts, model download, test flakes |
| 10 | [Running tests](10-tests.md) | e2e suites, miri, per-crate tests |

## Quick path

If you just want to ENCODE + RECALL once and confirm everything works,
the minimum sequence is:

```bash
git clone https://github.com/brain-db-io/brain-db.git
cd brain-db
just docker-up                          # build the dev container
just docker-shell                       # enter it
# inside container:
cargo run --bin brain-server -- --config config/dev.toml &
# wait for "listening" log line
cargo run --example store_and_recall -p brain-sdk-rust
```

You should see 30 memories encoded across 8 domains, 8 RECALL queries
returning relevant results, and a summary block at the end.

Detailed step-by-step coverage starts at [`01-setup.md`](01-setup.md).
