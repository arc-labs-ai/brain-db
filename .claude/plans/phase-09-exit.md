# Phase 9 — Exit checklist

Phase 9 has no `9.18` sub-task. The phase doc closes with a 6-item
exit checklist (lines 370–377 of `docs/phases/phase-09-server.md`)
that gates the `phase-9-complete` tag.

This plan walks each item to a verifiable conclusion, then tags.

---

## 1. Checklist items

### 1.1 All sub-tasks complete

`grep -c '\[x\]' docs/phases/phase-09-server.md` should equal the
number of `### Task 9.` headers (16 sub-tasks: 9.1–9.17, with the
mid-phase renumberings noted in the doc).

**Action:** visual inspection — confirm every `### Task 9.*` line
ends in `[x]`. Mark ✓ or surface any missing.

### 1.2 `just verify` green

We've been running `just docker-verify` per sub-task. The phase
exit calls for `just verify` specifically (host fmt + workspace
tests + clippy). Both should be equivalent in green-state, but
the phase doc names `just verify` so we run that.

**Action:** `just verify`. If it diverges from `docker-verify`
(host-only flake), surface and decide whether to gate on the
docker run.

### 1.3 `cargo run --bin brain-server` accepts a connection

Boot the server with a minimal config, connect a hand-rolled
client over loopback, complete the handshake.

**Action:** prepare a minimal `config.toml` under a temp dir,
`cargo run --bin brain-server -- --config <path>` in the
background, `nc 127.0.0.1 <port>` or a one-shot Rust client to
verify TCP accept + HELLO/WELCOME. Tear down with SIGTERM and
confirm a clean exit.

The simplest path: write a tiny ad-hoc client binary or shell
script under `target/scratch/` (not committed) that does the
handshake from `crates/brain-server/tests/e2e.rs` against a
real port. Or — even simpler — just `nc -z 127.0.0.1 <port>`
to prove the listener is up, since 9.17's tests already prove
the handshake works over loopback.

**Decision:** use `nc -z` for the connect check. The handshake
is already covered by 9.17. The checklist phrasing "accepts a
connection from a sample client" is satisfied by TCP accept;
deeper coverage would duplicate 9.17's test surface.

### 1.4 E2E smoke test passes 100 iterations

9.17's `repeated_encode_recall_is_stable` is exactly this — 100
× (encode + recall) on one connection. Run the test 5–10 times
back-to-back to confirm it's deterministic, not flaky.

**Action:**
```
for i in $(seq 1 10); do
  cargo test -p brain-server --test e2e \
    repeated_encode_recall_is_stable -- --nocapture || break
done
```

Expect 10/10 passes. If a flake surfaces, surface and stop.

### 1.5 `just run-server` boots in < 5 seconds with empty data

Cold boot on an empty data dir. Time from `cargo run` invocation
to "ready to accept connections" log line (or to the first
successful `nc -z`) should be < 5 s after the binary is built.

**Action:**
```
cargo build --release --bin brain-server    # warm the binary
rm -rf /tmp/brain-empty && mkdir -p /tmp/brain-empty
time cargo run --release --bin brain-server -- --config <path> &
# wait for accept-loop
while ! nc -z 127.0.0.1 <port>; do sleep 0.1; done
# kill
```

Report the wall-time. If > 5 s, surface — likely arena/wal init
cost we haven't tuned.

**Note:** the spec says "< 5 seconds with empty data" without
specifying debug vs release. Release is the fair measurement
since it's what operators run. Debug-mode timing is informational.

### 1.6 Tag `phase-9-complete`

After the previous 5 items pass:
```
git tag phase-9-complete
```

Annotated tag with a short message summarizing what shipped:
```
git tag -a phase-9-complete -m "Phase 9 — brain-server: ..."
```

We pick annotated because the prior phase tags appear to use the
same style (verify before tagging).

---

## 2. Files touched

- `docs/phases/phase-09-server.md` — flip each `[ ]` in the exit
  checklist to `[x]` as each item passes; mark the tag landed at
  the end.
- (No code changes expected.)

---

## 3. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `just verify` (host) reveals a flake the docker run masked, or vice versa | Run both; if they disagree, surface and don't tag |
| Item 1.5's cold-boot exceeds 5 s on this machine | Report actual time; surface for the user to decide whether to gate the tag on it or relax the threshold for now |
| `repeated_encode_recall_is_stable` flakes 1/10 | Stop, root-cause the flake before tagging |
| Sample-client connect (1.3) hits a race where the listener isn't bound yet | Poll `nc -z` with a short interval and a 10 s timeout |

---

## 4. Done criteria

- [ ] All 6 exit-checklist items confirmed green.
- [ ] Phase doc updated: every checklist item marked `[x]`.
- [ ] Annotated tag `phase-9-complete` pushed onto the current
      commit.
- [ ] Single commit with the checklist flip; tag points at that
      commit.

---

## 5. Out of scope

- No new sub-tasks. If something is missing, that's a 9.18 (or
  Phase 10) conversation, not a smuggle-it-into-the-tag.
- No code changes outside the phase doc.
- No subprocess E2E (9.17's plan §8 defers it).
- No ROADMAP.md update (that happens when Phase 10 starts).

---

*Awaiting approval before executing the checklist.*
