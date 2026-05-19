# Brain — contributor documentation

This is where you're meant to land if you're hacking on Brain
itself rather than running it.

## Working on Brain

Inside [`usage/`](usage/):

| Topic | File |
|---|---|
| Toolchain + clone | [`usage/01-setup.md`](usage/01-setup.md) |
| Build + verify (fmt-check + build + clippy + test + check-skills) | [`usage/02-build-and-verify.md`](usage/02-build-and-verify.md) |
| Run the server locally | [`usage/03-run-server.md`](usage/03-run-server.md) |
| Use the CLI | [`usage/04-cli.md`](usage/04-cli.md) |
| Use the Rust SDK | [`usage/05-sdk.md`](usage/05-sdk.md) |
| End-to-end walkthrough | [`usage/06-walkthrough.md`](usage/06-walkthrough.md) |
| Configuration knobs | [`usage/07-configuration.md`](usage/07-configuration.md) |
| Debugging | [`usage/08-debugging.md`](usage/08-debugging.md) |
| Troubleshooting | [`usage/09-troubleshooting.md`](usage/09-troubleshooting.md) |
| Tests + benches | [`usage/10-tests.md`](usage/10-tests.md) |
| Practical guide / quick reference | [`usage/practical-guide.md`](usage/practical-guide.md) |

## Spec deviations

[`spec-deviations.md`](spec-deviations.md) — every place the
implementation knowingly diverges from
[`../../spec/`](../../spec/) and why. Required reading before
landing a change that touches behaviour the spec describes.

## Phase plans

[`phases/`](phases/) — one file per phase (00 through 24).
Each documents the phase's scope, sub-tasks, "done-when"
criteria, scope cuts, and per-sub-task implementation plans.
Read the relevant phase doc before starting work on a
sub-task; per-sub-task plans live under
[`../../.claude/plans/`](../../.claude/plans/).

[`phases/README.md`](phases/README.md) is the phase index.

## Workflow

- Read the spec section, the phase doc, and the per-sub-task
  plan first.
- Implement, run the verify suite (fmt-check + build + clippy +
  test + check-skills), then commit.
- See [`../../CONTRIBUTING.md`](../../CONTRIBUTING.md) for the
  full contributor workflow + commit conventions.
