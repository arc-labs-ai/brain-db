# 18.05 Partial Failure Handling

When some shards or components fail but others continue.

## 1. The principle

Brain's design assumes failures are local. A failure in one shard shouldn't take down others.

In single-node deployments:
- One shard may have issues; others continue.
- The connection layer continues serving requests for healthy shards.

In future clustered deployments:
- One node may fail; others continue.
- Replicas (with replication enabled) take over.

## 2. Single-shard failure

A shard becomes unhealthy:

- Crashes (Glommio executor panics).
- Storage errors (disk failed for that shard).
- Hung worker.

Behavior:
- Operations targeting that shard fail with `ShardUnavailable`.
- Operations on other shards continue normally.
- Brain may attempt to recover the shard (restart its tasks).

## 3. The "shard health" detection

Each shard has a health state:

```
Healthy: all systems normal.
Degraded: minor issues but operational.
Unhealthy: significant issues; some operations fail.
Failed: all operations fail.
Recovering: restarting / replaying WAL.
```

Brain's health endpoint returns per-shard state.

## 4. Per-shard recovery

For a failed shard:

```
1. Detect via health check failure.
2. Stop operations on that shard.
3. Capture diagnostics (logs, profile).
4. Attempt recovery:
   a. Restart shard's executor.
   b. Replay WAL.
   c. Verify health.
5. If successful: re-enable.
6. If unsuccessful: keep disabled, alert.
```

Brain can recover automatically; operator review confirms.

## 5. The "shard quarantine"

If a shard is repeatedly failing:

```
1. Mark as quarantined.
2. New operations to it fail fast.
3. Brain doesn't keep retrying restart.
4. Alert escalates.
```

Quarantine prevents repeated failures from cascading.

## 6. Operator-triggered shard restart

Administer via the admin HTTP API (`/v1/*` on the admin listener); see [§17.04](../17_observability/04_admin_ops.md). (Operator action: stop and restart a single shard's executor in place — WAL replay brings it back up — without touching the other shards. Route name TBD.)

If the issue is transient (e.g., a stuck task), restart resolves it.

## 7. The "failed shard" client behavior

Clients querying a failed shard see:

- `ShardUnavailable` error.
- The error includes a hint that recovery may be in progress.

Clients can:
- Retry with backoff.
- Fall back to other shards (if data spans).
- Surface to user.

## 8. Cross-shard fan-out partial failure

For multi-shard operations (rare):

```
shards 0, 1, 2 → operations
shard 1 fails

Result: partial response with shards 0 and 2's data, error indicating shard 1 unavailable.
```

The response includes a `partial: true` flag. The client decides:
- Use the partial data.
- Wait for shard 1 recovery.
- Surface as failure.

## 9. The "graceful degradation" pattern

Some features can degrade gracefully:

- index maintenance worker stuck → recall quality slowly drops; alerts.
- Consolidation worker stuck → no new Consolidated memories; episodic still works.
- Decay worker stuck → salience doesn't decay; functional, but slowly accumulating.

These don't break operations; they just stop progress on background work.

## 10. The "shed load" partial response

Under heavy load, Brain may return Overloaded for some operations:

- Writes shed first (less critical for reads).
- Cross-shard operations shed before single-shard.
- Background work pauses.

Clients see Overloaded for shed operations. This is partial-success: Brain is still operational, but capacity is limited.

## 11. Embedder failure isolation

If the embedder fails (model unavailable, OOM, etc.):

- ENCODE operations fail (need to embed).
- RECALL operations may succeed (with cached cue) or fail (cache miss).
- Other operations continue (they don't need embedding).

The embedder's failure doesn't take down Brain.

## 12. Storage failure isolation

If a shard's storage layer fails:

- Reads on that shard return errors.
- Writes return errors.
- Other shards continue (separate storage).

For multi-shard operations, the failed shard is excluded.

## 13. Network failure isolation

If the network fails:

- New connections fail.
- Existing connections may stall and timeout.
- Internal Brain operations (within process) continue.

Once network heals, normal service resumes.

## 14. Worker failure isolation

If a background worker fails:

- That worker's job isn't progressing.
- Other workers continue.
- Request handling continues.

Workers are independent; one failure doesn't cascade.

## 15. The "blast radius" framing

For each failure type, the blast radius:

| Failure | Blast Radius |
|---|---|
| One shard's worker | One shard's worker |
| One shard's executor | One shard |
| One shard's storage | One shard |
| Embedder | All shards' encodes |
| Connection layer | All clients |
| Process | All shards |
| Machine | All shards on this machine |
| Region (future versions) | All shards in this region |

Smaller blast radius = better resilience.

## 16. The "recovery timeline"

For each failure type, expected recovery:

| Failure | Recovery Time |
|---|---|
| Worker crash | Auto-restart in seconds |
| Shard executor crash | Restart in 10s of seconds |
| Brain process crash | Auto-restart by supervisor + WAL replay |
| Hardware failure | Hours (replacement + restore) |
| Disaster | Hours to days (DR procedures) |

Knowing these helps set SLAs.

## 17. The "reduce coupling" principle

Brain reduces coupling across components:

- Shards are independent (no shared mutable state).
- Workers are independent.
- Connection handling is separate from data plane.

When something fails, only its dependents fail. Most of Brain keeps working.

## 18. The "circuit breakers" use

For dependencies (embedder, possibly external services):

- Wrap calls in circuit breakers.
- Fail fast when the dependency is down.
- Don't pile up backpressure.

This prevents one dependency from cascading into node-wide issues.

## 19. The "isolated test" pattern

In testing:

- Inject failures into one component.
- Verify others continue.
- Verify recovery once the injected failure clears.

These tests catch coupling that wasn't intended.

## 20. The user-facing partial-failure semantics

For agents using Brain:

- A partial failure means some operations work and some don't.
- The agent must handle both states gracefully.
- Brain's errors are clear about which operations failed.

The application doesn't need to model partial node state — just handle errors per operation.

---

*Continue to [`06_disaster_recovery.md`](06_disaster_recovery.md) for DR procedures.*
