# RB-10: Network partition

**Severity:** **N/A in v1**.
**Alert:** (none in v1).
**SLO impact:** depends on the partition's shape;
this runbook will be the operational guide once v2
clustering ships.
**Estimated duration:** N/A.
**Skill level:** N/A.

This runbook is a **placeholder** for the future
multi-node clustering scenario. In v1, Brain is
single-node and "network partition" only refers to
the substrate-to-client partition.

---

## What v1 means by "network partition"

In v1, Brain runs as a **single process** on a
single host. There's no native replication, no
cross-node clustering, no Raft / Paxos / equivalent.

So the only meaningful "partition" is between Brain
and its clients (or between Brain and its
observability stack). That's a network-config /
LB issue, not a Brain issue.

If the substrate is reachable from monitoring but
not from clients, you have a load-balancer or
firewall problem. Not in scope here; refer to your
network team.

---

## What you do during a "partition" in v1

The substrate has no special partition behaviour
because there's no replication to be partitioned. It
keeps serving (or not) based purely on its local
state. Clients see:

- **Brain healthy, clients can't reach it:** clients
  fail open / fail closed per their own retry
  policy. Brain is unchanged.
- **Brain unhealthy:** Brain is unreachable to
  everyone. Other runbooks apply
  ([RB-01](rb-01-substrate-down.md),
  [RB-08](rb-08-unresponsive.md)).
- **Brain partitioned from monitoring but reachable
  by clients:** alerts won't fire, but service
  continues. Notice via missing-data alerts.

None of these is a "partition" in the distributed-
systems sense (split-brain, divergent replicas,
quorum loss).

---

## What v2 might add

A future multi-node clustering capability would
introduce:

- Multiple substrate nodes coordinated via consensus.
- Cross-node replication of memories and metadata.
- Quorum reads / writes.
- Leader election.

Network partitions then become meaningful: nodes
disagree about who's authoritative; quorum may be
lost; writes may stall.

When that ships, this runbook will be rewritten with
specifics. For now, it's a stub so the RB-N
numbering stays stable.

---

## If you're reading this because of a real incident

You're likely in the wrong runbook. Check:

- Are clients getting connection errors? →
  [RB-13](rb-13-connection-saturation.md) or your
  org's network runbook.
- Is the substrate up but unresponsive? →
  [RB-08](rb-08-unresponsive.md).
- Is the substrate down? → [RB-01](rb-01-substrate-down.md).
- Did monitoring stop receiving data while clients
  are still happy? → check your scraping infrastructure
  (Prometheus, etc.).

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md)
- [RB-08 — Substrate becoming unresponsive](rb-08-unresponsive.md)
- [RB-13 — Connection saturation](rb-13-connection-saturation.md)
- [IR-03 — Escalation policy](ir-03-escalation-policy.md)

---

## Last validated

*This runbook is a v1 placeholder. Re-validation
when v2 clustering ships.*
