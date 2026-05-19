# RB-14: LLM cost spike

**Severity:** **P2** typically (cost is a business
problem, not an outage). **P1** if cost is approaching
a hard cap that will disable extraction entirely.
**Alert:** `BrainLlmSpendHigh` (LLM API cost above a
budget threshold over a rolling window).
**SLO impact:** none direct — the substrate is
healthy. But ongoing cost overrun has business
impact.
**Estimated duration:** 30 minutes to immediate
mitigation; days for root-cause investigation.
**Skill level:** comfortable with extractor configs,
LLM-tier semantics, and your provider's billing
dashboard.

The LLM-tier extractors are consuming more API
budget than expected. The substrate is serving
normally; what's broken is the *cost profile*.

This runbook is for **the cost itself**. If the cost
spike is caused by a backfill, see
[RB-11](rb-11-schema-toggle.md) for backfill-specific
context.

---

## Am I in the right runbook?

You should see:

- `brain_llm_cost_micro_usd_total` rate elevated.
- Your LLM provider's dashboard shows daily / hourly
  spend above projection.
- (Optional) `brain_llm_cache_hit_rate` dropped from
  its baseline.
- (Optional) `brain_llm_calls_total` rate elevated.

If LLM extractors are *failing* (errors, not cost),
that's a separate problem — see
[RB-05](rb-05-worker-stuck.md) if the extractor
worker is the issue.

---

## Stop the bleeding

Money is bleeding now. Stop the spend first; diagnose
after.

1. Acknowledge the page; open the incident channel.
2. **Identify the dominant cost source:**

   ```bash
   brain-cli admin llm cost-by-extractor --window 1h
   ```

   Output:

   ```
   extractor_name           calls   tokens_in   tokens_out   cost_usd
   preference_extraction     8421   1,242,891    312,047        $42.18
   contradiction_finder       142      18,221      4,891         $0.62
   relationship_extractor      14       2,891        421         $0.09
   ```

   The biggest line is your culprit.

3. **Decide: pause, throttle, or let it run?**

   - **If cost rate is doubling per hour:** pause
     the dominant extractor immediately. Cost stops
     accruing.
   - **If cost rate is elevated but bounded:** more
     thoughtful response. Investigate before
     pausing.
   - **If cost is approaching a hard budget cap that
     will disable extraction entirely:** absolutely
     pause now, before the substrate disables it
     automatically and you can't observe the
     symptom.

   To pause an extractor:

   ```bash
   brain-cli admin extractor disable preference_extraction
   ```

   The extractor stops running on new encodes. Re-
   enable later with:

   ```bash
   brain-cli admin extractor enable preference_extraction
   ```

---

## Diagnose

### 1. Why is one extractor spending so much?

Three possible causes:

- **Cache hit rate dropped.** The LLM cache normally
  absorbs repeats; if hit rate is low, every encode
  hits the LLM. Check:

  ```promql
  brain_llm_cache_hit_rate{extractor="preference_extraction"}
  ```

  Healthy: 30-80% depending on workload. <10% means
  the cache isn't catching anything; likely an
  extractor-version bump invalidated all entries,
  or the cache TTL expired all entries at once.

- **Encode rate spiked.** More encodes → more
  extractor calls → more cost.

  ```promql
  rate(brain_encode_total[1h])
  ```

- **Per-call cost rose.** Same number of calls, more
  expensive per call. Could be:
  - Memories are getting longer (more input tokens).
  - The extractor's prompt is bigger than it used
    to be.
  - Provider raised prices.

### 2. Is it the backfill?

Backfills run LLM extractors against the entire
history. Check if a backfill is in progress:

```bash
brain-cli admin backfill status
```

If yes: backfill is the cause. Decide:

- **Continue and accept the cost** — common if the
  backfill is planned and budgeted.
- **Pause and re-plan** — common if the projected
  cost is much higher than expected (estimate was
  wrong; LLM is doing more work than predicted).

To pause a backfill:

```bash
brain-cli admin backfill pause --request-id <id>
```

Resume later (or cancel entirely).

### 3. Is it organic traffic growth?

If encode rate has been rising for a while,
LLM-extractor cost rises proportionally. Look at
the trend over the last 7-30 days:

```promql
sum(rate(brain_llm_cost_micro_usd_total[1h]))
sum(rate(brain_encode_total[1h]))
```

If both are rising together, the cost spike is
organic. Adjust budgets / config to match new
reality.

### 4. Is the extractor running on the wrong memories?

The `trigger` field in an extractor's declaration
gates which memories it processes. Common mistakes:

- A new extractor added with an over-broad trigger
  (`on encode` instead of `on encode where
  memory.kind = episodic`).
- A `trigger` query that's accidentally matching
  more than expected.

Check the extractor's trigger:

```bash
brain-cli schema show preference_extraction
```

If the trigger looks wrong, the fix is a schema
update narrowing it.

### 5. Is the prompt sizing problematic?

Check per-call token usage:

```promql
histogram_quantile(0.95,
  rate(brain_llm_input_tokens_bucket{extractor="preference_extraction"}[5m]))
```

If p95 input tokens are high (>4000), each call is
expensive. Could be:

- The prompt is too verbose.
- Few-shot examples are big.
- Memory texts are unusually long.

Trim the prompt or the examples; that immediately
lowers per-call cost.

### 6. Are you in a runaway loop?

A failure mode where the substrate retries failed
LLM calls and burns through budget:

```promql
rate(brain_llm_retry_total[5m])
```

If retry rate is high relative to call rate, a
specific request is failing and retrying. Look at
errors:

```bash
brain-cli admin llm recent-errors --extractor preference_extraction
```

If all retries are for one shape of memory (e.g.,
those with very long text), the cost is from
repeatedly failing on the same poison input.

---

## Remediate

### Pause the offender

(Already covered in Stop-the-bleeding.) If the
extractor was an emergency disable, leave it off
until you've fixed the root cause.

### Narrow the extractor trigger

If the extractor was running too broadly:

```diff
 extractor preference_extraction {
     kind = llm
     target = statement Preference
     model = "claude-haiku-4-5"
-    trigger: on encode
+    trigger: on encode where memory.kind = episodic
+              and memory.text contains "prefer"
     ...
 }
```

Schema upload as in [RB-11](rb-11-schema-toggle.md).
The narrower trigger means fewer LLM calls.

### Tighten cost budgets

```diff
 extractor preference_extraction {
     ...
-    cost_budget: "$0.005 per memory"
+    cost_budget: "$0.0008 per memory"
 }
```

A tighter per-call budget means the substrate skips
expensive inputs (long memories, etc.) — they're
audited as `SkippedBudget` instead of consuming
LLM budget.

This is a hard ceiling. Tightening it too
aggressively may cause too many skips and reduce
recall quality.

### Switch to a cheaper model

A bigger lever:

```diff
-    model = "claude-sonnet-4-6"
+    model = "claude-haiku-4-5"
```

Sonnet is ~5x more expensive than Haiku. For many
extraction workloads, Haiku is sufficient. Test in
staging before broad rollout — quality may drop on
some predicates.

### Switch tiers entirely

Even cheaper:

```diff
 extractor preference_extraction {
-    kind = llm
+    kind = classifier
     ...
 }
```

A classifier is ~$0 / call but requires you to have
(or train) a model that does the same job. Often a
multi-week project, not an incident remediation.

### Cancel a runaway backfill

```bash
brain-cli admin backfill cancel --request-id <id>
```

This stops the backfill immediately. Already-
processed memories keep their extracted data; the
rest stay unprocessed. Re-plan and re-run with
tighter cost controls.

### Raise the budget (if cost is legitimate)

If the spend matches the workload's value:

```toml
[knowledge.llm_budget]
daily_cap_usd = 200       # was 50
```

Make sure your stakeholders are aligned before
raising. The whole point of the alert is "do you
*want* to be spending this much?" — saying "yes" is
a deliberate business decision.

---

## Verify

After remediation:

```promql
# Cost rate should drop.
sum(rate(brain_llm_cost_micro_usd_total[10m]))

# If you paused an extractor, calls for it should be zero.
rate(brain_llm_calls_total{extractor="preference_extraction"}[5m])
```

Confirm with your provider's billing dashboard
(updates with some lag — minutes to an hour).

The `BrainLlmSpendHigh` alert clears once the
rolling-window spend drops below threshold.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Symptom: LLM spend at $X/day, 3x projected.
Root cause: <e.g., extractor "preference_extraction" had over-broad trigger;
processed every encode, not just those mentioning preferences>.
Remediation: <e.g., narrowed trigger to "memory.text contains 'prefer'";
disabled until reviewed>.
Cost impact: $Y over the spike window.
Follow-up: TICKET-NNNN (extractor cost-projection process).
Postmortem: <yes/no>.
```

Postmortem rule for RB-14:

- **Yes** if the cost spike was unexpected (process
  gap, not a deliberate experiment).
- **Yes** if the cost was significant (defined per
  org).
- **Skip** if you were running a planned experiment
  that you'd already accounted for.

---

## Prevention

- **Set per-extractor cost budgets** with a hard
  cap, not just monitoring. The substrate honours
  cost budgets at the extractor level; configure
  them.
- **Set a deployment-wide LLM spend cap.** When
  spend exceeds the cap, the substrate auto-pauses
  LLM extractors. Better than burning unbounded
  budget while operators sleep.
- **Always dry-run backfills** ([RB-11](rb-11-schema-toggle.md)).
  Dry-run prints the projected cost; review before
  committing.
- **Alert on rising-then-not-falling cost.** A
  spike that mitigates itself within an hour is
  often a benign burst; a spike that stays high for
  hours is a process / config problem.
- **Use the cheapest tier that works.** Promotion
  ladder: pattern → classifier → LLM. The LLM tier
  is the last resort, not the first.
- **Cache TTL trade-offs.** A 7-day TTL captures
  most repeat patterns; longer means more storage
  but higher hit rate. Tune to your workload.
- **Monitor cache hit rate** as a leading indicator.
  A dropping rate is the first sign of a config
  change or workload shift that will manifest as
  cost.

---

## Related runbooks

- [RB-04 — Disk filling](rb-04-disk-filling.md) (LLM
  cache disk usage)
- [RB-11 — Schema toggle](rb-11-schema-toggle.md)
  (cost during backfills)
- [Concepts: extractors](../concepts/14-extractors.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
