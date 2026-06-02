# 18.06 Disaster Recovery (DR)

Procedures for recovering from disasters that cause significant data loss.

## 1. The DR scenarios

- **Data center loss**: fire, flood, power, etc. The deployment is destroyed.
- **Region failure** (cloud): cloud provider's region is unavailable for hours/days.
- **Catastrophic data corruption**: silent corruption goes undetected; eventually surfaces; backups may be corrupted too.
- **Major operator error**: e.g., `agent delete` on every agent.
- **Security breach**: attacker tampers with data and audit.

## 2. The DR objectives

- **RPO (Recovery Point Objective)**: how much data loss is acceptable?
- **RTO (Recovery Time Objective)**: how quickly must service resume?

These vary by deployment. Brain provides mechanisms; operators choose the cadence.

| Tier | RPO | RTO |
|---|---|---|
| Test/Dev | days | hours |
| Standard production | hours | hours |
| Mission-critical | minutes | minutes |
| Real-time | seconds | seconds |

For real-time tier, Brain v1 isn't sufficient (no replication). Replication in a future major version targets this.

## 3. The backup strategy

For DR, snapshots are the foundation:

```
Local snapshots (in the same data center):
  - For fast recovery from single-shard issues.
  - Rapid restore.

Off-site snapshots (different region/DC):
  - For DR.
  - Slower restore (network transfer).
  - Protect against site loss.
```

Both should be in place for production.

## 4. Snapshot shipping

After a snapshot is created:

```bash
curl -s -X POST http://127.0.0.1:9092/v1/snapshots -d '{"name":"daily-2026-05-07"}'
# Snapshot is at /var/lib/brain/snapshots/daily-2026-05-07
```

Operator ships off-site:

```bash
aws s3 sync /var/lib/brain/snapshots/daily-2026-05-07 \
  s3://my-brain-backups/2026-05-07/
```

For automation, configure Brain's snapshot worker with an off-site uploader. Brain doesn't have direct cloud-storage integration in v1; standard tools handle it.

## 5. Snapshot retention policy

Recommended:

- Keep all snapshots from the last 7 days.
- Keep weekly snapshots for the last 90 days.
- Keep monthly snapshots for the last year.

Storage cost is moderate; the protection is worth it.

## 6. The DR drill

Periodically:

1. Take a current snapshot.
2. Restore it to a separate environment.
3. Verify the restored node works.
4. Document any issues.

Drills surface issues before real disasters. Without drills, DR procedures may not work when needed.

## 7. The DR runbook

When a disaster strikes:

```
1. Confirm the disaster (don't act on a transient outage).
2. Notify stakeholders.
3. Identify the most recent valid backup.
4. Provision recovery environment.
5. Restore from backup.
6. Verify.
7. Switch traffic.
8. Communicate restoration.
```

This is the high-level path; specifics depend on the deployment.

## 8. The "alternate site" preparation

For fast DR:

- Have an alternate site (different data center / region) ready.
- Pre-deploy infrastructure (machines, network, etc.).
- Periodically refresh with current snapshots.

When disaster hits:
- Restore from the most recent off-site snapshot.
- Switch traffic.
- Total downtime: minutes to hours (depending on snapshot transfer time).

## 9. The "secondary node" approach

Run a secondary Brain node in a different region:

- Receives snapshots periodically (e.g., every 6 hours).
- Restores them to keep state current.
- Ready to take over with limited data loss.

This is more expensive (running 2× the infrastructure) but offers better RTO.

For full real-time replication, a future major version's replication will be needed.

## 10. The data-only DR

Sometimes only data needs DR; Brain code can be redeployed quickly:

- Data files (arena, WAL, metadata) are backed up.
- Brain binary is in version control / artifact storage.

In a disaster:
- Spin up new infrastructure.
- Deploy Brain version.
- Restore data.
- Resume.

Total time: a few hours typically.

## 11. The "DR tests in CI"

Brain's CI runs DR scenarios:

- Snapshot, restore, verify.
- Fail-then-recover sequences.
- Cross-region restore (in a multi-region test environment).

These tests catch DR-procedure regressions.

## 12. The "failed DR" path

What if the DR doesn't work?

- The off-site snapshot is corrupt.
- The infrastructure can't be provisioned.
- The restore takes too long.

Mitigations:

- Multiple backup copies (at least 2 off-site destinations).
- Pre-validated restore procedures.
- Capacity reservations for DR infrastructure.

If all fails, data may be permanently lost. The operator's process should plan for this with stakeholders.

## 13. Documentation requirements

For DR:

- Step-by-step procedures.
- Contact lists (who to notify).
- Escalation paths.
- Backup locations and credentials.
- Recovery infrastructure access.

Documentation must be:
- Up-to-date.
- Accessible during a disaster (off-line copies).
- Tested (drills).

## 14. The audit trail in DR

The audit log is critical for forensic investigation post-disaster:

- Was this an attack? Hardware? Human?
- What was the state when the disaster hit?
- Were any operations in flight?

Audit logs should be backed up off-site too.

## 15. Communication during DR

During DR:

- Notify users / customers.
- Communicate expected downtime.
- Provide updates as restore progresses.

Clear communication reduces user frustration. Brain doesn't provide tooling; standard incident-management tools (StatusPage, etc.) integrate.

## 16. Post-DR analysis

After restoration:

- Post-mortem.
- Identify root cause.
- Review the DR process — what worked, what didn't.
- Improve procedures.

DR is rare; learning from each instance matters.

## 17. The "incident severity" framing

For incident response, common severity levels:

- SEV-1: full outage; data potentially at risk.
- SEV-2: partial outage; significant impact.
- SEV-3: degraded; minor impact.
- SEV-4: cosmetic; no user impact.

Disasters are typically SEV-1. Procedures, on-call, and communication should match.

## 18. The DR cost analysis

Costs:

- Backup storage: low (~$0.01/GB-month for cloud).
- Off-site transfer: moderate (~$0.05-0.10/GB).
- Standby infrastructure: high (running 2× costs).
- DR drills: low (engineering time, ~1 day per quarter).

Total: ~10-30% added to operational cost for moderate DR. ~50-100% for hot-standby.

The cost is the price of resilience. Skipping it risks a much larger cost later.

## 19. The "recovery from old snapshot" semantics

If the only snapshot is days old:

- Lots of data loss.
- The agent's recent context is gone.
- Application-level recovery: re-encode missing data if available.

For chatbot scenarios, this is recoverable (re-summarize recent conversations). For knowledge bases, it may be a permanent gap.

## 20. The "what cannot be recovered" honest list

Even with good DR:

- Data created between last backup and disaster: lost.
- In-flight operations at the moment of disaster: lost.
- Data that was specifically deleted before the disaster: stays gone (which is correct).

Brain's design pushes the "lost" category to be small, but it's not zero. Operators should set expectations accordingly.

---

*Continue to [`07_chaos_testing.md`](07_chaos_testing.md) for chaos testing.*
