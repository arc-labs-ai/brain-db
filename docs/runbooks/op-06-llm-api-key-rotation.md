# OP-06: LLM API key rotation

**Severity:** **operator-triggered**. P3 routine,
P2 if responding to a leak.
**Alert:** `BrainLlmKeyExpiringSoon` (if your
provider issues keys with an expiry).
**SLO impact:** brief degradation of LLM extractor
throughput on hard restart; near-zero on overlap +
reload.
**Estimated duration:** 30 minutes to 2 hours.
**Skill level:** comfortable with env-var
management, your secret store, and the LLM
provider's key dashboard.

API keys for upstream LLM providers (Anthropic,
OpenAI, …) are long-lived credentials that grant
spend authority. Rotate them on a 90-day cadence,
immediately when leaked, and when personnel with
provider-console access change.

Brain's LLM extractors are an optional layer,
active only when a schema declares
`extractor: llm` for one or more predicates.
Substrate-only deployments have no LLM keys to
rotate.

The pattern is **overlap, then revoke**: mint the
new key, make the substrate use it, verify, *then*
revoke the old one. Reversing this order drops
every in-flight and queued extractor call until
the new key propagates — with per-call latency in
seconds, the queue backup can take many minutes
to clear.

---

## Am I in the right runbook?

Use this if you're rotating:

- `ANTHROPIC_API_KEY` (Claude family).
- `OPENAI_API_KEY` (GPT family).
- Any other provider key consumed by
  `brain-llm`.

**Not this runbook for:**

- TLS certificates on the substrate's listener —
  see [OP-04](op-04-tls-cert-rotation.md).
- Client-facing auth tokens (`brain-cli`, SDKs)
  — see [OP-05](op-05-auth-token-rotation.md).
- An LLM cost or volume spike (which may
  *trigger* a rotation if exfiltration is
  suspected) — see
  [RB-14](rb-14-llm-cost-spike.md) first.

If you've discovered a key in a public repo,
chat, or third-party log: this is a security
incident. Promote to P2 and proceed with a hard
restart — outage is the lesser harm. Pull
[DR-03](dr-03-safe-production-access.md) before
touching production.

---

## Pre-flight checklist

- [ ] **Identify which keys are in use.** A
      deployment may have several. Rotate one at
      a time.
      ```bash
      brain-cli admin llm config | jq '.providers[].name'
      ```
- [ ] **Confirm the substrate is healthy.**
      Rotating under existing pressure compounds
      problems.
      ```bash
      brain-cli admin shards | jq '[.[].status] | unique'
      # Expected: ["active"]
      ```
- [ ] **Check current LLM call health.** Existing
      auth errors mean the current key may
      already be broken; investigate before
      rotating.
      ```bash
      brain-cli admin llm recent-calls --limit 50 \
        | jq '[.[] | .status] | group_by(.) | map({k:.[0], n:length})'
      ```
- [ ] **Know your deploy mechanism.** systemd
      unit, Docker compose env, Kubernetes
      Secret, Vault sidecar — Step 3 depends on
      it.
- [ ] **Notify stakeholders.** Even a clean
      overlap rotation leaves a paper trail
      worth announcing; a hard restart needs a
      maintenance window.

---

## Step 1 — Create the new key in the provider's dashboard

### Anthropic (Claude)

`https://console.anthropic.com/` → **Settings →
API Keys → Create Key**. Name it for the rotation
schedule (e.g. `brain-prod-2026-05`). Set a
per-key spend limit slightly above expected
monthly burn if the provider offers one.

### OpenAI (GPT)

`https://platform.openai.com/` → **Settings → API
keys → Create new secret key**. Name and scope as
above; OpenAI's per-project restriction is worth
using if available.

**Copy the key immediately** — providers show
the secret once. If you close the dialog you have
to delete and start over.

**Do not** paste the key into chat, email,
ticket, or a shared doc. Dashboard → password
manager / secret store, nowhere else.

---

## Step 2 — Store the new key in your secret manager

The substrate's process must read it at runtime;
nothing else should.

```bash
# AWS Secrets Manager
aws secretsmanager put-secret-value \
    --secret-id brain/prod/anthropic-api-key \
    --secret-string "$NEW_KEY"

# HashiCorp Vault
vault kv put secret/brain/prod/anthropic \
    api_key="$NEW_KEY"

# Kubernetes Secret (--dry-run + apply avoids
# plaintext in kubectl history)
kubectl create secret generic brain-llm-keys \
    --from-literal=ANTHROPIC_API_KEY="$NEW_KEY" \
    --dry-run=client -o yaml | kubectl apply -f -

# systemd EnvironmentFile (0600, service-user
# owned)
sudo install -m 0600 -o brain -g brain \
    /dev/stdin /etc/brain/llm.env <<EOF
ANTHROPIC_API_KEY=$NEW_KEY
EOF
```

Then clear your shell history:
`unset NEW_KEY && history -d $(history 1)`.

---

## Step 3 — Deploy the new key to brain-server's environment

Three patterns. Pick the one your deployment
supports.

### Pattern A: Restart-based (always works)

Simple, universal, drops every in-flight LLM
call. Use the rolling-restart procedure from
[OP-01](op-01-rolling-restart.md) so the outage
is per-shard, not global.

```bash
# Update env first, then restart.

# systemd
sudo systemctl daemon-reload
sudo systemctl restart brain-server

# Docker compose
docker compose up -d --force-recreate brain-server

# Kubernetes
kubectl set env statefulset/brain-server \
    --from=secret/brain-llm-keys
kubectl rollout status statefulset/brain-server \
    --timeout=10m
```

In-flight calls fail with a transport error; the
substrate retries them once the new process is
up. Queue depth recovers in a few minutes.

### Pattern B: Hot reload via SIGHUP

If your build supports SIGHUP-driven env re-read
(check
`brain-cli admin features | grep llm_reload`),
this is the zero-downtime path:

```bash
# Update the EnvironmentFile / Secret first,
# then signal the running process.

# systemd (if ExecReload= is wired to SIGHUP)
sudo systemctl reload brain-server
# or:
sudo kill -HUP "$(pgrep -f brain-server)"

# Kubernetes
kubectl exec brain-server-0 -- kill -HUP 1
```

The LLM client re-reads the env var on the next
outbound call. In-flight calls continue on the
connection they're on; new calls use the new
key.

**Caveat:** SIGHUP only helps if your supervisor
updated the process environment *and*
`brain-server` reads from the EnvironmentFile
(not just its own `environ`) on reload. Verify
in staging before relying on this in production.

### Pattern C: Side-by-side keys (future)

The clean pattern is for the substrate to accept
*two* env vars (`ANTHROPIC_API_KEY` and
`ANTHROPIC_API_KEY_NEW`) and try the new one
first, falling back. **Not implemented in v1**;
documented as the target end-state. Until then,
use A or B.

---

## Step 4 — Verify the substrate is using the new key

The substrate logs every LLM call to its audit
table.

```bash
# Recent successful calls — should be non-empty
# within a few minutes of normal load.
brain-cli admin llm recent-calls \
    --status success --limit 20

# Auth failures should be zero.
brain-cli admin llm recent-calls \
    --status auth_failed --since 5m
# Expected: []
```

If your provider tags calls with a key ID,
confirm attribution to the *new* key:

```bash
brain-cli admin llm recent-calls --limit 5 \
    | jq '.[].key_id'
# Should match the new key's prefix.
```

Provider-side verification — log into the
dashboard and look at **last used** on both
keys:

- New key: usage within the last few minutes.
- Old key: trailing off; no new calls after the
  rotation moment.

If the old key still sees traffic after 5
minutes, **stop**. Some process hasn't picked up
the new key. Don't proceed to Step 5 until it's
quiet.

Watch dashboards for at least 10 minutes:
extractor latency at baseline, error rate at
zero, queue depth steady.

---

## Step 5 — Revoke the old key

Once Step 4 confirms the new key is in use and
the old key is idle:

- **Anthropic:** `https://console.anthropic.com/`
  → **API Keys**, find by the name you chose in
  Step 1, **Delete**. Irreversible.
- **OpenAI:** `https://platform.openai.com/` →
  **API keys**, **Revoke**.

Confirm in your monitoring that LLM calls keep
succeeding for at least 10 minutes after
revocation. If anything was still quietly using
the old key, you'll see auth errors now.

Remove the old key from your secret store as
well — keeping a revoked key around in plaintext
is risk for no benefit.

---

## Rollback

If after Step 3 the new key isn't working (auth
errors, persistent failures):

1. **Don't revoke the old key.** This is why
   Step 5 is last.
2. Put the old key value back into the env
   source and re-run Step 3's deploy step.
3. Investigate the new key: typos, scope
   mismatch, project restriction, provider-side
   propagation delay (5–10 minutes is normal
   for some providers).

If you already revoked the old key (leak
response, or you skipped ahead): mint a *third*
key, deploy it, and treat the gap as an
incident — file an IR and post the timeline.

---

## Post-operation

Post in your team channel:

```
:key: LLM key rotation complete at HH:MM UTC.
Provider: <Anthropic / OpenAI / …>
Pattern: <restart / hot reload>
Old key revoked: yes
Duration: Xm.
Next rotation due: <YYYY-MM-DD>
```

Update your rotation calendar with the next due
date (90 days out, by default). File a follow-up
if anything required manual intervention.

---

## Pitfalls

### Revoking the old key before the new one is live

The most common foot-gun. Every queued and
in-flight LLM call fails with an auth error
until the new key propagates. Step 5 is last for
a reason.

### Leaking the new key during rotation

Pasting it into chat, committing it to a config
repo "just for now", emailing it to yourself —
every one of these has happened. If you slip,
rotate again immediately and treat it as an
incident.

### Forgetting non-substrate consumers

Batch ingestion scripts, ad-hoc notebooks, a
separate evaluation harness may all use the same
key. Inventory them and rotate in the same
window, or expect revocation to break them.

### Cost tracking broken by rename

If your finance pipeline groups by `key_id` or
display name, changing the name between
rotations breaks the time series. Keep a stable
naming convention, or update dashboards to
bridge old and new.

### Hot reload that didn't actually reload

SIGHUP semantics vary by build, supervisor, and
env source. Without Step 4's verification, you
may believe the rotation succeeded while the old
key is still in use — until you revoke it and
find out the worst way.

### Treating routine and incident rotation the same

A scheduled rotation has the luxury of overlap.
A leak response does not: every extra minute
with a leaked key in the wild is extra exposure.
Promote to P2, accept the outage, revoke first.

---

## Prevention

- **90-day automatic rotation.** Put the next
  rotation on your calendar before closing this
  runbook.
- **Per-key spend limits at the provider.** Caps
  blast radius if a key leaks before you notice.
- **Per-key scopes / project restrictions.** The
  substrate's key should only call the endpoints
  it uses.
- **Short-lived keys** where supported. A 24-hour
  key that leaks is a much smaller problem than
  a 12-month one.
- **Secret scanning** on repos, CI logs, and
  chat archives. Most leaks are accidental
  commits.

---

## Related runbooks

- [OP-01 — Rolling restart / version upgrade](op-01-rolling-restart.md)
- [OP-04 — TLS certificate rotation](op-04-tls-cert-rotation.md)
- [OP-05 — Auth token rotation](op-05-auth-token-rotation.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)
- [RB-14 — LLM cost spike](rb-14-llm-cost-spike.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)

---

## Last validated

*Update on first use.*
