# 16.09 Open Questions

Acceptance and benchmarking questions unresolved as of this spec version.

---

## OQ-BA-1: Targets for non-reference hardware

**Issue.** Targets are for reference hardware. How should they translate to other hardware (smaller, larger, different generation)?

**Options.**

a) **Linear scaling.** Document expected scaling curves.

b) **Per-class targets.** Targets for "small", "standard", "large" hardware.

c) **Per-deployment.** Operators measure their own.

**Recommendation.** (a) plus (c). Document expected scaling; deployments verify their own targets.

---

## OQ-BA-2: SLO vs SLI vs hard target

**Issue.** Latency targets are stated as p99 ≤ 25 ms. Is this a hard contract, or a target with some flexibility?

**Options.**

a) **Hard contract.** Substrate must always meet; failure indicates bug.

b) **SLO target.** Substrate aims to meet 99% of the time; occasional misses are normal.

**Recommendation.** (b). Realistic target with explicit SLO framing. p99 is itself an aggregate over time, so some flexibility is built-in.

---

## OQ-BA-3: Workload realism

**Issue.** The reference workload (70/25/5) may not match all real workloads. Some are write-heavy, others read-heavy.

**Options.**

a) **One reference workload.** Other workloads tested separately.

b) **Multiple reference workloads.** "Read-heavy", "write-heavy", "balanced".

**Recommendation.** (b). Several workloads to characterize the substrate's behavior across regimes.

---

## OQ-BA-4: Recall benchmark dataset

**Issue.** Recall depends on the dataset. Synthetic data may not reflect real behavior.

**Options.**

a) **Synthetic only (status quo).**

b) **Real-world datasets.** E.g., publicly available embedding datasets.

c) **Both.**

**Recommendation.** (c). Synthetic for control, real for validation.

---

## OQ-BA-5: Latency targets for slow operations

**Issue.** Some operations (REASON depth-10, full PLAN with text) are slow by design. Targets must be realistic.

**Options.**

a) **Single target per operation type.**

b) **Targets parameterized by inputs.** E.g., REASON depth=N has target f(N).

**Recommendation.** (b). Specific operations have specific targets based on input characteristics.

---

## OQ-BA-6: Cold-start performance

**Issue.** Cold-start is slow (recovery + warm-up). Should this be a target?

**Options.**

a) **No cold-start target.** Excluded from primary targets.

b) **Recovery time target.** "Recover in < N seconds for a 1M-memory shard."

**Recommendation.** (b). Explicit recovery time target (10-30 sec for 1M memories).

---

## OQ-BA-7: Tail behavior under sustained overload

**Issue.** When sustained load exceeds capacity, what's the spec? Latency unbounded? Errors? Drops?

**Options.**

a) **Backpressure.** Return Overloaded errors.

b) **Bounded queues.** Drop after queue fills.

c) **Slow down.** Accept everything but with degraded latency.

**Recommendation.** (a). Already specified. The acceptance tests verify Overloaded is returned cleanly.

---

## OQ-BA-8: Comparison benchmarks

**Issue.** Comparing to other systems is fraught — they have different APIs, models, capabilities.

**Options.**

a) **No comparisons.** Each system stands alone.

b) **Apples-to-apples.** Where APIs match (vector search), compare. Note differences.

c) **Workload-based.** "For this workload, system A: X ops/sec; Brain: Y ops/sec; pgvector: Z ops/sec."

**Recommendation.** (b) and (c). Both useful; honest about limits.

---

## OQ-BA-9: Continuous benchmarking

**Issue.** Tracking benchmark trends over time helps catch slow regressions.

**Options.**

a) **Manual.** Run benchmarks at release time.

b) **CI-integrated.** Run continuously; chart over time.

c) **Public benchmark site.** Like AS-SAFE-Bench or similar.

**Recommendation.** (b) for v1; (c) is aspirational.

---

## OQ-BA-10: Acceptance criteria for "edge" deployments

**Issue.** Brain may run on edge devices (resource-constrained). Different targets apply.

**Options.**

a) **Not supported.** Brain is for servers.

b) **Edge profile.** A subset of features with relaxed targets for edge.

**Recommendation.** (a) for v1; (b) for future. Edge has different constraints; revisit.

---

*Continue to [`10_references.md`](10_references.md) for references.*
