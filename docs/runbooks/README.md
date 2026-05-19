# Runbooks

Procedures for the operator on call. When an alert fires,
when something looks wrong, when you need to do something
risky to production — these are the documents to read.

If you're new to Brain operations, start with
[`ir-01-how-to-use-runbooks.md`](ir-01-how-to-use-runbooks.md).
If an alert just fired, look up the alert name in the
**incident runbooks** table below.

---

## Four parts

### Part 1 — Incident response basics

Foundational context. Read these once when you join the
on-call rotation; refer back when you need to remember
the severity ladder or how to escalate.

| | File |
|---|---|
| IR-01 | [How to use these runbooks](ir-01-how-to-use-runbooks.md) |
| IR-02 | [Severity ladder and SLOs](ir-02-severity-ladder.md) |
| IR-03 | [Escalation policy](ir-03-escalation-policy.md) |
| IR-04 | [Incident communication](ir-04-incident-communication.md) |

### Part 2 — Diagnostic recipes

Reusable building blocks that incident runbooks cite.
When a runbook says "collect a diagnostic bundle" or
"check the metrics dashboard," it links here.

| | File |
|---|---|
| DR-01 | [Collecting a diagnostic bundle](dr-01-diagnostic-bundle.md) |
| DR-02 | [Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md) |
| DR-03 | [Safe production access](dr-03-safe-production-access.md) |
| DR-04 | [Taking a profile or heap dump](dr-04-profiles-and-heap-dumps.md) |
| DR-05 | [Verifying durability invariants](dr-05-verifying-durability-invariants.md) |

### Part 3 — Incident runbooks

When an alert fires, you find your runbook here. Each
maps to an alert annotation (`runbook: RB-N`) or a
generic symptom.

| | Runbook | Linked alert |
|---|---|---|
| RB-01 | [Substrate doesn't start](rb-01-substrate-down.md) | `BrainSubstrateDown` |
| RB-02 | [High latency on a shard](rb-02-high-latency.md) | `BrainHighLatency` |
| RB-03 | [Memory pressure / OOM](rb-03-memory-pressure.md) | `BrainHighMemoryPressure` |
| RB-04 | [Disk filling](rb-04-disk-filling.md) | `BrainDiskFilling` |
| RB-05 | [Worker stuck](rb-05-worker-stuck.md) | `BrainWorkerStuck` |
| RB-06 | [HNSW recall degraded](rb-06-recall-degraded.md) | `BrainRecallQualityDegraded` |
| RB-07 | [Recovery from corruption](rb-07-corruption-recovery.md) | (chaos-detected) |
| RB-08 | [Substrate becoming unresponsive](rb-08-unresponsive.md) | (composite) |
| RB-09 | [Mass FORGET aftermath](rb-09-mass-forget.md) | `BrainHighTombstoneRatio` |
| RB-10 | [Network partition](rb-10-network-partition.md) | (v2 only) |
| RB-11 | [Schema toggle](rb-11-schema-toggle.md) | (operator-triggered) |
| RB-12 | [Restart loop](rb-12-restart-loop.md) | `BrainRestartLoop` |
| RB-13 | [Connection saturation](rb-13-connection-saturation.md) | `BrainConnectionsExhausted` |
| RB-14 | [LLM cost spike](rb-14-llm-cost-spike.md) | `BrainLlmSpendHigh` |

### Part 4 — Operational runbooks

Planned procedures. You're not on fire — you're doing
something deliberate and don't want to break things.

| | Runbook |
|---|---|
| OP-01 | [Rolling restart / version upgrade](op-01-rolling-restart.md) |
| OP-02 | [Snapshot restore drill](op-02-snapshot-restore-drill.md) |
| OP-03 | [Backup verification](op-03-backup-verification.md) |
| OP-04 | [TLS certificate rotation](op-04-tls-cert-rotation.md) |
| OP-05 | [Auth token rotation](op-05-auth-token-rotation.md) |
| OP-06 | [LLM API key rotation](op-06-llm-api-key-rotation.md) |
| OP-07 | [Embedder model upgrade](op-07-embedder-model-upgrade.md) |
| OP-08 | [Configuration change rollout](op-08-config-change-rollout.md) |

### Plus

| File | When |
|---|---|
| [Postmortem template](postmortem-template.md) | After a P1 or P2, or any incident worth learning from. |

---

## The standard runbook template

Every incident and operational runbook follows the same
structure. So an operator who has worked through one knows
where to find every section in any other:

```
# RB-N: <Title>

**Severity:** P1 / P2 / P3 / P4
**Alert:** `BrainXyz` (or "operator-triggered")
**SLO impact:** what users see while this is happening
**Estimated duration:** typical time from page to fixed
**Skill level:** what familiarity is assumed

## Am I in the right runbook?
   (Quick triage — symptoms to confirm.)

## Stop the bleeding
   (Immediate actions before diagnosis, if the system is on fire.)

## Diagnose
   (Step-by-step procedure with commands, expected output, branches.)

## Remediate
   (Fix, organised by root cause.)

## Verify
   (How to confirm the fix actually worked.)

## Post-incident
   (What to log, what tickets to file, when to write a postmortem.)

## Prevention
   (What to change so this is less likely next time.)

## Related runbooks

## Last validated
```

Operational runbooks (`OP-N`) use the same template with
"Stop the bleeding" replaced by "Pre-flight checklist."

---

## When to use a runbook

**An alert fires.** Open the linked runbook in Part 3
(`RB-N`). Run through it top to bottom. The Diagnose
section is structured as branches — follow the branch your
symptoms match.

**You're planning maintenance.** Open the relevant
operational runbook in Part 4 (`OP-N`) *before* you
start. Walk the pre-flight checklist; then follow the
steps. Don't improvise.

**Something looks wrong but no alert fired.** Skim Part
3 by symptom column to find the closest match. If none
fits, start with [DR-02 — Reading traces, metrics, and
logs](dr-02-reading-traces-metrics-logs.md) and work
from there.

**You're writing a postmortem.** Use
[`postmortem-template.md`](postmortem-template.md). Keep
it blameless.

---

## Severity ladder (summary)

For the full definition, see
[IR-02](ir-02-severity-ladder.md).

| Severity | What | Page? | Ack target |
|---|---|---|---|
| P1 | Production down or major data risk | Yes, immediately | 15 min |
| P2 | Significant degradation, user-visible | Yes, working hours | 30 min |
| P3 | Minor degradation, partial impact | Ticket; next business day | 24 h |
| P4 | Informational; no current impact | Ticket; opportunistic | 1 week |

Every runbook declares its severity at the top.

---

## Editing these runbooks

These files are **operator-edited**. They're not
auto-generated from anything. When a procedure changes,
update the runbook. When you've validated a runbook by
running through it on a real (or chaos-test) incident,
update the `Last validated:` field.

Two rules:

1. **Keep the template.** Same sections, same order. An
   operator at 3 AM shouldn't have to hunt for the
   diagnostic steps.
2. **Be specific.** A runbook is not the place to be
   vague. "Check the dashboard" is wrong; "Open
   `Brain — Overview`, look at the *Latency p99 by op*
   panel" is right.

If a runbook contradicts the codebase, **the codebase is
the source of truth** for *what the substrate does*. The
runbook reflects *what the operator does*. Sometimes
those need to be updated together.
