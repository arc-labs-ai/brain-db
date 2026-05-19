# Tutorials

**Audience:** users learning Brain by doing.

**Goal:** *learning*. Each tutorial walks step-by-step from a
known starting point to a known endpoint. You follow the
instructions; the instructions work. No prior Brain knowledge
assumed.

Tutorials are deliberately distinct from
[`../guides/`](../guides/): a guide says "here's how to do X in
your real environment"; a tutorial says "do exactly this and you
will end up here". Tutorials prioritise the journey; guides
prioritise the goal.

## Path

Read in order. Each one builds on the previous.

| # | Tutorial | You'll have |
|---|---|---|
| 01 | [`01-quickstart-docker.md`](01-quickstart-docker.md) | Brain running in Docker, health endpoint green |
| 02 | [`02-first-substrate-app.md`](02-first-substrate-app.md) | Memories encoded, substrate recall returning hits |
| 03 | [`03-shell-deep-dive.md`](03-shell-deep-dive.md) | Hands-on tour of `brain` — encode, recall, subscribe, txn, named agents |
| 04 | `04-first-knowledge-app.md` *(planned)* | A schema declared, typed statements being extracted |
| 05 | `05-hybrid-query-walkthrough.md` *(planned)* | Hybrid retrieval with filters, query routing |
| 06 | `06-multi-shard-deploy.md` *(planned)* | A multi-shard topology, sized for ~1 M memories |

## When a tutorial breaks

Tutorials are tested against each release. If a step fails:

1. Check the [troubleshooting section](01-quickstart-docker.md#troubleshooting)
   in the relevant tutorial.
2. Open an issue with the tutorial filename, the step that failed,
   and `docker logs brain` output.
3. As a fallback, the equivalent how-to under
   [`../guides/`](../guides/) usually has more detail.

## See also

- [`../concepts/`](../concepts/) — read these *in parallel* with
  the tutorials if you want background on what's happening.
- [`../guides/`](../guides/) — the production-shaped versions of
  what the tutorials demonstrate.
