# OP-05: Auth token rotation

**Severity:** **operator-triggered**. P3 routine,
P2 if responding to a known or suspected token leak
(which is a security incident in its own right).
**Alert:** none routinely; security-incident-driven
rotation is page-driven via your security on-call.
**SLO impact:** none if done correctly. Brief
authentication failures for clients if the old
token is removed before they've picked up the new
one.
**Estimated duration:** 1–3 hours depending on
number of clients and how their secrets are
distributed.
**Skill level:** comfortable with the substrate's
auth configuration, your secret-management system
(Vault / AWS Secrets Manager / sealed-secrets /
etc.), and coordinating a config change across
clients.

Why rotate: hygiene (a long-lived token has had
more chances to leak — laptop backup, screen-
share, CI log); known leak (token surfaced in a
public repo / screenshot / chat); personnel change
(an engineer with cluster-admin access leaves);
compliance (SOC2 or internal policy requiring
demonstrated rotation cadence).

The pattern that makes rotation safe is **dual-
valid overlap**: for a transition window, both
the old token and the new token authenticate
successfully. Clients pick up the new token at
their own pace. Once you've confirmed nobody is
using the old token anymore, you remove it.

The pattern that makes rotation *unsafe* is "yank
the old token, push the new token, hope all
clients update fast enough." Don't do that unless
you're in the security-emergency case where
you're deliberately trading availability for
safety.

---

## Am I in the right runbook?

Use this if you're rotating a **per-agent token**
(used by clients during `Handshake` to
authenticate as a specific `agent_id`), the
**cluster admin token** (used by
`brain-cli admin …` and operator tooling), or an
**automation token** (CI job, scheduled job,
service-to-substrate integration).

If you're rotating something that isn't a Brain
auth token: TLS certificates →
[OP-04](op-04-tls-cert-rotation.md), LLM provider
API keys → [OP-06](op-06-llm-api-key-rotation.md),
general config-value changes →
[OP-08](op-08-config-change-rollout.md).

If a token is known-leaked and you are currently
in an incident, you're still in this runbook —
jump to the "Security emergency" pitfall near the
end. Also open
[IR-04](ir-04-incident-communication.md) for the
external messaging side.

---

## Pre-flight checklist

Before starting:

- [ ] **Identify exactly which tokens you're
      rotating.** "All of them" is rarely correct.
      Write the list down; you'll work the list.
      ```bash
      brain-cli admin tokens --list
      ```
- [ ] **Identify the consumers of each token.**
      Agent processes, sidecars, CI pipelines,
      notebooks, dev laptops; for the admin token
      also operators and automation using
      `brain-cli admin`. If you can't enumerate
      the consumers, you can't verify that
      rotation completed — stop and figure that
      out first.
- [ ] **Confirm secret-management ownership.**
      Where do clients fetch their tokens? Vault,
      AWS Secrets Manager, a Kubernetes `Secret`,
      an ansible-vault file? If the answer is "a
      Slack message", that's the underlying
      problem and rotation is your chance to fix
      it. See pitfalls.
- [ ] **Decide on the overlap window.** 24–72
      hours is typical for client-driven pickup;
      shorter (≤ 1 hour) if every consumer is
      automation you control and can restart on
      demand. Write the planned cutover time into
      the ticket.
- [ ] **Notify consumers.** "Token rotation for
      `agent_id=X` scheduled HH:MM UTC. Please
      update by HH:MM UTC + overlap."
- [ ] **Check audit logging is enabled.** Step 4
      uses it to verify migration. If it's off,
      turn it on, or arrange explicit per-
      consumer confirmation instead.
- [ ] **Pre-commit a rollback path.** If the new
      token doesn't work, fall back to the old
      token (still valid during overlap). If the
      new *config* breaks the substrate, see
      [OP-08](op-08-config-change-rollout.md).

For the security-emergency case (known leak),
skip the overlap-window and notification steps
and read the dedicated pitfall below first.

---

## Step 1 — Generate the new token(s)

Use a cryptographically secure source. **32 bytes
minimum** (256 bits of entropy):

```bash
openssl rand -hex 32
# e.g.: 9fbe7b4a3c2d... (64 hex chars = 32 bytes)
```

If you're rotating multiple tokens, generate them
all up front and label them clearly. Don't reuse
the same token across agents — that defeats per-
agent identity.

```bash
# One per agent, written to a scratch file you'll
# clean up at the end of the rotation.
for AGENT in agent-prod-a agent-prod-b agent-ci; do
    printf '%s\t%s\n' "$AGENT" "$(openssl rand -hex 32)"
done > /tmp/rotation-$(date -u +%Y%m%d).tsv
chmod 600 /tmp/rotation-$(date -u +%Y%m%d).tsv
```

The scratch file should live on disk for as short
a time as possible; push the values into secret
storage, then shred it. Do not commit it. Do not
paste it into chat.

---

## Step 2 — Add new tokens to the allow-list (overlap with old)

Add the new tokens *alongside* the old ones. Both
must validate during the overlap window.

```bash
# Add new tokens.
brain-cli admin tokens add \
    --agent-id agent-prod-a \
    --token "9fbe7b4a..." \
    --label "rotation-$(date -u +%Y%m%d)"

# Repeat for each agent / token being rotated.
```

For the admin token, the command is similar but
scoped to the admin role:

```bash
brain-cli admin tokens add \
    --role admin \
    --token "..." \
    --label "rotation-$(date -u +%Y%m%d)"
```

Verify both old and new are present and active:

```bash
brain-cli admin tokens --list
# Look for the agent / role; should show TWO
# entries (the old labelled with its earlier
# rotation date, and the new one labelled with
# today).
```

The new tokens are now valid. The old tokens are
*also* still valid. No client is affected yet —
the handshake validates against the allow-list,
so any token in the list works.

If your auth config lives in a file rather than
runtime state, this is the step that needs a
config reload (`brain-cli admin reload-auth` or
the equivalent for your deployment). Verify the
reload took effect by re-running
`brain-cli admin tokens --list`.

---

## Step 3 — Distribute new tokens to clients

This step happens *outside* the substrate and is
the messiest part of token rotation.

The right pattern: **clients fetch tokens from a
secret-management system that you've already
populated with the new value.** Vault, AWS
Secrets Manager, Google Secret Manager,
Kubernetes `Secret`s with sealed-secrets, etc.

```bash
# Vault example.
vault kv put secret/brain/agent-prod-a \
    token="9fbe7b4a..."

# AWS Secrets Manager example.
aws secretsmanager put-secret-value \
    --secret-id brain/agent-prod-a \
    --secret-string "9fbe7b4a..."

# Kubernetes Secret example.
kubectl create secret generic brain-agent-prod-a \
    --from-literal=token="9fbe7b4a..." \
    --dry-run=client -o yaml \
    | kubectl apply -f -
```

Clients pick up the new value on their next fetch
— on restart, on a periodic refresh cycle, or on
a config-reload signal. The substrate doesn't
drive this; the secret manager + client behavior
do. If a client doesn't auto-refresh, this step
includes restarting (or signalling) it.

After distribution, securely delete the scratch
file from Step 1:

```bash
shred -u /tmp/rotation-$(date -u +%Y%m%d).tsv
```

(`rm -P` on macOS. On modern SSDs `shred` isn't
guaranteed; the better defense is to never have
written the value to disk beyond the secret
manager.)

---

## Step 4 — Verify all clients are using new tokens

The gate before Step 5. Don't skip it. Removing
the old token before clients have migrated is the
#1 failure mode of this runbook.

Use the audit log to see which tokens are
actually authenticating connections:

```bash
# Show recent successful auths, grouped by token label.
brain-cli admin audit --since 1h --event handshake \
    | jq -r '.token_label' \
    | sort | uniq -c | sort -rn
```

What you want to see:

- The **new** token label has traffic (clients
  are using it).
- The **old** token label has no traffic for at
  least the last full client refresh cycle (in
  practice: long enough that you're confident no
  consumer still holds it). For 24-hour-cached
  consumers, observe a full 24 hours with zero
  hits on the old label.

If the old label still shows traffic:

```bash
# Identify which agent / source IP is still using it.
brain-cli admin audit --since 1h --event handshake \
    --token-label "rotation-2025XXXX-old" \
    | jq '{ts, agent_id, remote_addr}'
```

Then go chase down that consumer. Do not proceed
to Step 5 until the old token is quiet. If you
can't identify the consumer, extend the overlap
window — don't cut over and break them.

For deployments where consumers hold long-lived
connections and rarely reconnect, audit-log
*absence* may not be conclusive within your
window — you may need to force a reconnect (a
planned rolling restart of the consumer, or a
Brain-side connection drain — see
[RB-13](rb-13-connection-saturation.md)) to
flush out who's holding the old token.

---

## Step 5 — Revoke old tokens

When and only when Step 4 confirms migration:

```bash
brain-cli admin tokens remove \
    --agent-id agent-prod-a \
    --label "rotation-2025XXXX-old"

# For the admin token:
brain-cli admin tokens remove \
    --role admin \
    --label "rotation-2025XXXX-old"
```

Verify:

```bash
brain-cli admin tokens --list
# Old labels gone; only the new ones remain.
```

Watch the audit log for the next 10–15 minutes
for any `auth_failed` events:

```bash
brain-cli admin audit --since 15m --event auth_failed \
    | jq '{ts, agent_id, remote_addr, reason}'
```

A trickle of failures from unknown source IPs
right after revocation is the smoking gun for "a
client we didn't know about was still using the
old token." See the pitfalls section.

---

## Rollback

If clients start failing auth and you need them
back fast:

1. **Re-add the old token to the allow-list**
   with the same value (you should still have it
   until the rotation ticket closes):
   ```bash
   brain-cli admin tokens add \
       --agent-id agent-prod-a \
       --token "<old-value>" \
       --label "rotation-rollback-$(date -u +%Y%m%d)"
   ```
2. Clients reconnect successfully; the overlap
   resumes.
3. Investigate why clients didn't pick up the new
   token (Step 4 missed something).
4. Resume from Step 3 once the gap is understood.

If the new config broke the substrate (rather
than just the clients), that's a config-rollout
problem — see
[OP-08](op-08-config-change-rollout.md).

If you've already shredded the old token value
and clients are failing, you can't roll back by
re-adding the old token; you'd have to generate
and re-distribute new tokens. This is why the
overlap-then-verify-then-revoke ordering matters.

---

## Post-operation

Post in your team channel:

```
:white_check_mark: Auth token rotation complete at HH:MM UTC.
Tokens rotated: <list of labels / agent IDs>.
Overlap window: HH:MM UTC start → HH:MM UTC end.
Issues encountered: <none / list>.
Next scheduled rotation: <date or "automated">.
```

Update the rotation tracking ticket / inventory:
when each token was issued, rotated, revoked. If
you have a quarterly rotation cadence, this is
the artifact that demonstrates the cadence to
auditors.

If you found an undocumented consumer during
Step 4, file a follow-up to add it to the
consumer inventory.

---

## Pitfalls

### Revoking the old token before clients migrated

The cardinal sin of this runbook; Step 4 exists
to prevent it. "Yank the old one, clients will
reconnect with the new one" is true only for
clients that re-fetch quickly. CI pipelines on a
daily schedule won't notice until they next run;
a notebook open since last week won't notice
until someone closes and reopens it.

### Distributing the new token in a chat channel

Slack, Discord, Teams, even an "ops-private"
channel — none of these are secret managers. The
token is now in chat history, search indexes,
mobile notification mirrors, possibly an archive
bot. If your distribution mechanism *is* chat,
rotation is your chance to fix that. Use a
secret manager.

### Rotating only the admin token, not the agent tokens

"We rotated the admin token last quarter, we're
good." Per-agent tokens are usually higher-volume
and longer-lived, and more likely to leak. Rotate
the full surface, not just the operator-facing
one.

### Forgetting an automated client

CI jobs, scheduled extractor runs, notebooks left
running on someone's workstation, the dev cluster
pointing at prod, a Grafana datasource —
automation is the consumer-set you underestimate.
Use the audit log (Step 4) as ground truth, not
your mental model.

### Same token for multiple agents

If you reused one token across `agent-prod-a` and
`agent-prod-b`, rotation is your chance to split
them. Generate two distinct tokens; update each
consumer's config separately. Per-agent identity
is the whole point of `agent_id` in the
handshake.

### Skipping the audit-log verification

If audit logging is off, you can't tell if Step 5
is safe. Turn it on for the rotation, or
coordinate explicit per-consumer confirmation
before proceeding. "I think it's been long
enough" is not a verification.

### Security emergency — leaked token

If a token is *known* to have leaked (committed
to a public repo, sent in a screenshot,
exfiltrated in a breach), the overlap pattern
inverts:

1. **Revoke immediately.** Accept the client
   impact:
   ```bash
   brain-cli admin tokens remove \
       --agent-id <agent> \
       --label <leaked-label>
   ```
2. Generate and distribute the new token (Steps
   1, 2, 3) on emergency footing.
3. Open a P2 (or P1, depending on scope) security
   incident; coordinate with
   [IR-04](ir-04-incident-communication.md) for
   external messaging.
4. Post-mortem how the leak happened
   (distribution mechanism, access controls,
   logging) and fix the source.

You're trading availability for safety. A leaked
token in the wild costs more than 30 minutes of
client auth failures.

### Hand-rolled rotation cadence

Manual once-a-quarter rotation is fine for small-
team / small-fleet deployments. Past that scale,
automate it: scheduled rotation (`cron` + a
script that runs this runbook's steps), short-
lived tokens with refresh, or integration with an
identity provider. Manual rotation that gets
skipped is worse than no policy — it gives false
assurance.

---

## Related runbooks

- [OP-04 — TLS certificate rotation](op-04-tls-cert-rotation.md)
- [OP-06 — LLM API key rotation](op-06-llm-api-key-rotation.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)
- [RB-13 — Connection saturation](rb-13-connection-saturation.md)
- [IR-04 — Incident communication](ir-04-incident-communication.md)

---

## Last validated

*Update on first use.*
