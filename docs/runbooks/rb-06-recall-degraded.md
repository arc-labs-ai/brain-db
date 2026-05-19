# RB-06: HNSW recall degraded

**Severity:** **P2**.
**Alert:** `BrainRecallQualityDegraded`.
**SLO impact:** recall returns fewer or less-relevant
results than expected. Latency is usually unchanged;
the *quality* is the problem.
**Estimated duration:** 30 minutes (manual rebuild) to
several hours (re-embed all memories).
**Skill level:** comfortable with HNSW concepts and
the substrate's index maintenance workers.

`recall` is returning results that the operator (or
client) reports as worse than usual. The substrate is
serving; the *quality* of the answers has dropped.

---

## Am I in the right runbook?

You should see one or more of these:

- `brain_hnsw_recall_at_k_estimate` below threshold
  (e.g., < 0.92 for recall@10).
- Customer reports that recall is returning fewer
  relevant results.
- A monitoring panel showing degraded retrieval
  quality.
- `brain_hnsw_tombstone_ratio` elevated (often the
  proximate cause).

If recall is *slow* (rather than *low quality*),
that's [RB-02](rb-02-high-latency.md). If recall
returns no results at all, that's likely
[RB-01](rb-01-substrate-down.md) or
[RB-08](rb-08-unresponsive.md).

---

## Stop the bleeding

Recall quality degrades gradually. There's no
"bleeding" in the immediate-action sense. Do the
standard:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle ([DR-01](dr-01-diagnostic-bundle.md)).
3. **Confirm the symptom.** A recall quality alert
   relies on a metric that may itself be wrong. Try
   a few hand-picked queries you know the right
   answers to:

   ```bash
   brain-cli recall "specific cue you know about" --top-k 10
   ```

   Did the obvious top result come back? If yes,
   the alert may be flaky; if no, you have a real
   problem.

---

## Diagnose

### 1. Tombstone ratio

```promql
brain_hnsw_tombstone_ratio
```

By shard:

```promql
brain_hnsw_tombstone_ratio{shard_id="<n>"}
```

Read:

- **`tombstone_ratio > 0.30`** → too many forgotten
  memories cluttering the graph. Direct cause of
  recall degradation. Proceed to *Remediate →
  Rebuild HNSW*.
- **`tombstone_ratio < 0.10`** → not the cause. Look
  elsewhere.
- **`tombstone_ratio` between 0.10–0.30** → borderline;
  consider rebuilding but probably not the root
  cause.

### 2. Recent mass FORGET?

A recent batch FORGET (often from compliance or
cleanup) is the most common cause of accumulated
tombstones. Check:

```promql
rate(brain_forget_total[1h])
```

Spikes in the FORGET rate correlate with rising
tombstone ratio. If you see a recent spike (last
24 hours), you're in [RB-09](rb-09-mass-forget.md)
territory; that's the more specific runbook for
this case.

### 3. HNSW node count vs live memories

A sanity check on the index:

```promql
brain_hnsw_node_count
brain_metadata_active_memory_count
```

The HNSW node count should be roughly equal to the
active memory count *plus* the tombstoned count. If
HNSW is much smaller than the memory count, the
index is incomplete — recall will miss memories.

This can happen if:

- A recent rebuild didn't finish (interrupted by a
  restart).
- A worker bug caused some memories to be skipped.
- Recent encodes didn't make it into the HNSW.

Check the `hnsw_maintenance` worker's recent
activity:

```bash
brain-cli admin worker show hnsw_maintenance
```

### 4. Embedder fingerprint mismatch

If the embedder model changed but memories weren't
re-embedded, queries (using the new model's
embeddings) won't find the old memories well — the
embedding spaces don't align.

```promql
brain_memory_count_by_model_fp
```

Or via the CLI:

```bash
brain-cli admin model-fingerprints
```

Output:

```
Active model fingerprint: a1b2c3d4...
Memories by fingerprint:
  a1b2c3d4...: 142,000 (current)
  e5f6g7h8...: 18,000 (stale)
```

If you see a meaningful population with stale
fingerprints, those memories' embeddings are from a
different model. Recall against them is unreliable.

Two paths:

- **Re-embed the stale memories** (preferred). The
  `embedder_re_embed` admin command (if your version
  ships it) iterates stale memories and re-embeds
  in the background.
- **Roll back the embedder** to the previous model
  (if you can). The fingerprints align again.

This is the case for [OP-07](op-07-embedder-model-upgrade.md);
the operational runbook covers a clean upgrade with
re-embedding planned.

### 5. ef_search misconfiguration

The HNSW search has an `ef_search` parameter — higher
gives better recall, lower gives lower latency.
Production default is 64; some operators have
lowered it for latency reasons and forgotten:

```bash
brain-cli admin config | grep -A2 '\[hnsw\]'
```

Confirm `ef_search >= 64` for normal recall quality.
If it's been lowered (e.g. 16), recall@10 will be
worse — that's the configured behaviour, not a bug.

### 6. Filter side-effects

If recall is via the hybrid path (knowledge-active
mode), filters can drop results unexpectedly. Check:

```bash
brain-cli query_trace --cue "your query" | jq '.filter_chain_stats'
```

Output shows survivor counts after each filter
stage:

```json
{
  "before": 100,
  "after_kind": 100,
  "after_time": 23,     ← time filter dropped 77 of 100
  "after_confidence": 23,
  "after_limit": 10
}
```

If a specific filter is dropping more than expected,
the *filter* is the cause, not the index. Verify
the filter parameters with the client.

---

## Remediate

### Rebuild HNSW

The most common fix. If the tombstone ratio is the
problem:

```bash
brain-cli rebuild-ann --shard <n>
```

Or for all shards:

```bash
for SHARD in $(brain-cli admin shards | jq -r '.[].id'); do
    brain-cli rebuild-ann --shard "$SHARD"
done
```

A rebuild takes ~30 seconds per 1M nodes. During
rebuild, latency on the affected shard *increases*
(the rebuild worker uses CPU); plan accordingly:

- For a non-critical shard, just go.
- For a critical shard during peak hours, consider
  draining traffic via your load balancer first
  (if your deployment supports per-shard routing).
- Otherwise: accept the brief latency dip in
  exchange for restored recall quality.

After rebuild:

- `tombstone_ratio` drops to 0.
- `node_count` reduces to live-memory count.
- Recall quality returns to baseline.

### Re-embed stale memories

If model fingerprints diverged:

```bash
brain-cli admin re-embed --filter "fingerprint != a1b2c3d4..."
```

This runs in the background — re-embeds each stale
memory under the current model. Slow (each re-embed
is a fresh inference: ~5-10 ms on CPU, so 10 K
memories ≈ 50-100 s, 1 M memories ≈ several hours).

You can scope by shard or by memory subset to
prioritise.

If the substrate version doesn't ship the re-embed
command, the workaround is to recreate the affected
memories: read each memory's text, forget it,
re-encode. Painful for large fleets.

### Restore `ef_search` to a reasonable value

If misconfigured:

```toml
[hnsw]
ef_search = 64       # default; raise to 128 for better recall
```

Roll out ([OP-08](op-08-config-change-rollout.md)).
No data rewrite needed; the parameter is used at
query time.

A typical recall@10 / `ef_search` curve:

| `ef_search` | recall@10 (1M index) | latency p99 |
|---|---|---|
| 16 | ~85 % | ~0.5 ms |
| 32 | ~92 % | ~1 ms |
| 64 | ~96 % | ~2 ms |
| 128 | ~98 % | ~4 ms |
| 256 | ~99 % | ~8 ms |

Pick the point on the curve that gives you the
quality you need without breaking latency SLO.

### Fix filter parameters

If a filter is dropping more than expected, this
isn't an index problem — it's a client / config
problem. Adjust the filter:

- Loosen `confidence_min` if too aggressive.
- Widen the time range.
- Remove redundant filters.

---

## Verify

After the fix:

```bash
# Recall@K estimate should recover.
curl -fsS http://127.0.0.1:9091/metrics | grep recall_at_k

# Hand-test a few queries.
brain-cli recall "known query"
brain-cli query --cue "known query" --top-k 10
```

Confirm with whoever reported the degradation
(client team, etc.) that results look right again.

The `BrainRecallQualityDegraded` alert clears once
the metric returns to its normal range. Allow ~5
minutes for the metric to update.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Root cause: <e.g., tombstone ratio 0.34 from FORGET batch on Sept 10>.
Remediation: <e.g., rebuilt HNSW on shards 1, 3, 4>.
User impact: ~Xh of degraded recall quality.
Follow-up: TICKET-NNNN.
Postmortem: <yes/no>.
```

Postmortem rule for RB-06:

- **Usually yes** — recall quality issues are often
  recurring, so a postmortem captures the trend.
- **Skip** if the cause was a one-off (model upgrade
  with planned re-embed; this is normal).

---

## Prevention

- **Auto-rebuild before tombstones get bad.** The
  `hnsw_maintenance` worker has a threshold for
  triggering rebuilds. Default is conservative
  (often 0.40); lowering to 0.20-0.30 catches
  problems earlier.
- **Alert on rising tombstone ratio,** not just on
  high recall-quality drop. The leading indicator is
  cheaper to act on.
- **Plan re-embeds during model upgrades.** Don't
  swap the embedder model and hope for the best —
  follow [OP-07](op-07-embedder-model-upgrade.md).
- **Measure recall@K periodically** with a
  representative test set. The metric the substrate
  exposes is an estimate; ground truth needs
  client-side evaluation.
- **Don't lower `ef_search`** to save latency unless
  you've measured the recall impact and decided it's
  acceptable.

---

## Related runbooks

- [RB-02 — High latency](rb-02-high-latency.md) (same
  metric, different angle)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md) (if
  `hnsw_maintenance` is stuck)
- [RB-09 — Mass FORGET aftermath](rb-09-mass-forget.md)
  (the specific high-tombstone cause)
- [OP-07 — Embedder model upgrade](op-07-embedder-model-upgrade.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
