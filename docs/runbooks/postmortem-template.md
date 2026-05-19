# Postmortem template

**Audience:** the incident commander or designated author
writing up a postmortem after a Brain incident. Also
useful for anyone reviewing one.

**Goal:** give you a canonical structure to copy. Fork
this file, fill in the blanks, ship it. The template is
opinionated on purpose — consistency across postmortems
is more valuable than letting each author re-invent the
shape.

This document is two things in one:

1. A **template** you copy verbatim.
2. A **guide**: alongside each section, brief notes on
   what to write, what to avoid, and what good answers
   tend to look like.

The template starts well below, after the framing
sections. The guidance is shown in italicized callouts
that you delete when you fork the file.

---

## How to use this template

The lifecycle of a Brain postmortem:

1. **Copy** this file to
   `docs/postmortems/YYYY-MM-DD-<short-slug>.md`.
   Example: `2024-09-12-substrate-down-bad-deploy.md`.
2. **Fill it in** within 5 business days of incident
   resolution. The longer you wait, the more details
   evaporate. Pull from the incident channel, the
   diagnostic bundle (see `dr-01-diagnostic-bundle.md`),
   metrics screenshots, and your own notes.
3. **Mark it Draft.** Share with the responders for
   factual accuracy. They were there; they catch the
   "no, the spike was at 14:07, not 14:17" kind of fix.
4. **Move to In review.** Share with the wider
   engineering group. Anyone can comment. The author
   resolves comments.
5. **Mark it Final.** Once action items are filed (with
   owners and dates), the postmortem is closed.
6. **File action items** in your tracker, linked back to
   the postmortem. Action items without trackers don't
   happen.

A postmortem isn't done when it's written. It's done
when its action items are tracked.

---

## When to write a postmortem

| Severity | Postmortem? |
|---|---|
| **P1** | Always. No exceptions. |
| **P2** | Usually. Skip only if the cause is already-known and trivially fixed (e.g., a config typo caught in 2 minutes with zero user impact). |
| **P3** | Optional. Write one if there's a lesson worth capturing, even if user impact was minimal. |
| **P4 / near-miss** | Write one if you came close to a P1 — postmortems on what *almost* broke production are often the most valuable. |

See `ir-02-severity-ladder.md` for how severity is
classified. If you're unsure whether something rates a
postmortem, write one — the cost of an unnecessary
postmortem is small; the cost of a missed lesson is
large.

---

## The blameless principle

Brain postmortems are **blameless**. This is not a
nicety; it's a tool. The goal of a postmortem is to make
the *system* more resilient, not to assign fault to a
person. Systems fail. People work inside systems. When
a person made a mistake, the question is "why did the
system allow that mistake to cause harm?" — not "why
were they bad?"

### What "blameless" actually means

It does **not** mean:

- Pretending mistakes didn't happen.
- Refusing to name what someone did.
- Ignoring patterns of poor judgment.

It **does** mean:

- Framing failures as system properties, not personal
  ones.
- Assuming responders made the best decision they could
  with the information they had at the time.
- Asking "what would have led a reasonable person to
  the same outcome?" — and fixing that.

### Bad vs good framing

| Bad (blaming) | Good (blameless) |
|---|---|
| "Alice was slow to escalate." | "The on-call hesitated to escalate; the runbook didn't say at what point to. Action: add an escalation trigger to rb-02." |
| "Bob deployed a broken config." | "A config error reached production. Our pre-deploy check didn't catch it because it didn't run against a real shard. Action: extend the smoke deploy to include a 1-shard apply." |
| "Carol misread the metrics." | "The dashboard's p99 panel was visually similar to the p50 panel. The responder read p50 thinking it was p99. Action: re-label and re-color the latency panel." |
| "The team forgot the runbook." | "The runbook existed but wasn't linked from the alert. Responders had to find it manually, costing ~7 minutes. Action: add runbook link to alert template." |
| "The intern shouldn't have run that command." | "A destructive command was available to anyone with prod read access. Action: gate `brain-cli forget-bulk` behind a separate role." |

Notice what changes: the *person* disappears from the
left column, replaced by the *system condition* that
made the failure likely. Then an action item targets
the system.

A useful self-check: if your draft postmortem named a
person, ask yourself, "if a different person had been on
call, would the outcome have changed?" If yes, the
problem is the system (training, docs, tooling,
staffing), not the individual. If no, you're already
talking about the system — just rephrase to make that
explicit.

---

## ↓↓↓ TEMPLATE STARTS HERE ↓↓↓

# Postmortem: <incident title>

**Date:** YYYY-MM-DD
**Author:** <name>
**Status:** Draft / In review / Final
**Severity:** P1 / P2 / P3
**Duration:** Xh Ym (from start to resolution)
**User impact:** <description>

> *Guide — header block:*
> *Title is a short noun phrase, not a verb sentence.
> "Substrate down after deploy of 2024-09-12" is good;
> "We had an outage because we deployed bad code" is
> bad. Date is the date the incident started (UTC). Status
> moves through Draft → In review → Final. Severity comes
> from `ir-02-severity-ladder.md`. Duration is from the
> first symptom (not the first page — symptoms often
> precede detection) to user-visible recovery. User
> impact in one line, quantified: "RECALL p99 > 10s for
> 47 min, ~3.1M failed reads, 0 data loss."*

## Summary

<2–3 sentence executive summary>

> *Guide — summary:*
> *Two or three sentences. What broke, who saw it, how
> long, what fixed it. Imagine a director with 30
> seconds reading only this block before a meeting.*
>
> *Good: "On 2024-09-12 14:07 UTC, a config rollout set
> the WAL group-commit window to 0 ms, collapsing
> throughput. RECALL p99 climbed to 12 s for 47 minutes
> affecting all production agents. Reverting the config
> at 14:54 restored normal latency within 90 seconds."*
>
> *Bad: "There was an incident on 2024-09-12. It was
> bad. We fixed it." — tells the reader nothing.*

## Timeline

<timestamped events; UTC>

```
2024-09-12 14:00 UTC  — deploy starts (config rollout)
2024-09-12 14:07 UTC  — first user reports slow RECALL
2024-09-12 14:09 UTC  — page fires on p99_latency alert
2024-09-12 14:12 UTC  — on-call acks, opens #inc-...
2024-09-12 14:18 UTC  — incident commander declared
2024-09-12 14:31 UTC  — root cause identified
                        (config diff inspected)
2024-09-12 14:54 UTC  — config reverted via rolling apply
2024-09-12 14:55 UTC  — p99 latency back below SLO
2024-09-12 15:10 UTC  — incident closed, comms sent
```

> *Guide — timeline:*
> *All times in UTC, no exceptions — mixed timezones in
> a timeline are a fast path to misreading. Include every
> meaningful event: first symptom, detection, page,
> ack, declaration, hypothesis pivots, mitigations,
> recovery, all-clear.*
>
> *A common mistake is to include only the "official"
> events (pages, declarations). Include the human
> events too: "14:22 — IC asks if WAL flush is stalling;
> rejected after checking iostat" is a real beat. The
> path through diagnosis matters as much as the
> destination.*
>
> *Pull timestamps from the incident channel, not from
> memory. Memory is wrong.*

## Impact

<user-visible impact; quantified where possible>

> *Guide — impact:*
> *Numbers, not adjectives. "Significant impact" is a
> non-answer. Quantify on at least these axes when they
> apply: how many users / agents / requests, what
> fraction of normal traffic, what duration, what data
> integrity outcome (zero loss / N stale reads /
> rolled-back writes), what compliance or SLO breach.*
>
> *Good: "All 12 production agents saw RECALL p99 > 10s
> from 14:07 to 14:54. ~3.1M RECALL requests returned
> over SLA (typical: < 200 ms). 0 lost writes — WAL
> integrity verified post-incident (see
> `dr-05-verifying-durability-invariants.md`). SLO breach
> on monthly availability: -0.4 percentage points."*
>
> *Bad: "Lots of users were affected. RECALL was slow."*

## Root cause

<what actually happened; the chain of causation>

> *Guide — root cause:*
> *The full causal chain — not just the single line of
> code. Walk it: why did this happen → because of X →
> why didn't X get caught → because of Y → why Y → ...
> until you reach something foundational. The "Five
> Whys" is a useful heuristic but don't fetishize the
> number five.*
>
> *Distinguish technical cause from contributing
> factors: the technical cause is the proximate
> mechanism; contributing factors are the conditions
> that let the technical cause reach production.*
>
> *Good: "Technical cause: a config rollout set
> `wal.group_commit_window_ms = 0`, which caused every
> WAL append to flush synchronously rather than batch.
> The shard write loop became the bottleneck; queued
> RECALL requests piled up behind writes on the same
> connection-layer thread.*
>
> *Contributing factors: (1) the config schema permits
> 0 as a value; the validator only rejects negatives.
> (2) The pre-deploy smoke test runs on an empty
> shard, where group commit doesn't matter — write
> volume is too low to expose the issue. (3) The
> rollout was applied in one batch across all shards,
> not staged, so there was no canary signal."*

## Trigger

<the immediate cause that started the incident — distinct from root cause>

> *Guide — trigger:*
> *Trigger answers "what specifically happened at
> 14:00?" — the human action or external event that
> kicked things off. Root cause answers "why did that
> trigger cause damage?" — the system property that let
> the trigger turn into an incident.*
>
> *Triggers often look mundane. "A routine config
> rollout." "A scheduled snapshot job." "A spike in
> request volume from agent X." That's normal — most
> triggers are routine; the news is what was sitting in
> the system waiting to be triggered.*
>
> *Good: "Rollout of config commit `a3f7b21` at 14:00
> UTC, applied via `brain-cli config apply --all`."*
>
> *Bad: "A bad config." — that's a value judgment, not
> a trigger.*

## Resolution

<what fixed it>

> *Guide — resolution:*
> *What action restored service. Include the command or
> the steps. If the fix was permanent, say so. If it was
> a workaround pending a real fix, say that too — and
> file the real fix as an action item.*
>
> *Good: "Reverted to config commit `e0c9d18` via
> `brain-cli config apply --revision e0c9d18 --all`.
> Rolling apply completed at 14:54 UTC; p99 dropped
> below SLO within 90 seconds. This was a full revert,
> not a workaround — the bad config is no longer in
> production."*
>
> *Bad: "We rolled back."*

## What went well

<yes, this matters — celebrate good calls>

> *Guide — what went well:*
> *Often skipped. Don't skip it. Postmortems that only
> document failure breed a culture of fear. Surface the
> good calls: the runbook that worked, the responder
> who escalated at the right moment, the rollback that
> didn't break anything, the monitoring that caught it
> 7 minutes before users did.*
>
> *Specific examples beat general praise. "Detection
> was fast — the p99 alert fired 2 minutes after the
> first user-affecting moment" beats "we did a good
> job."*

## What went poorly

<systemic issues, not individual failures>

> *Guide — what went poorly:*
> *This is the most important section, and the easiest
> to get wrong. Reread the **blameless principle**
> above before writing. Every item here should be a
> system property, not a person.*
>
> *Good entries:*
> *- "The config validator did not reject 0 ms even
>    though 0 is operationally invalid."*
> *- "The pre-deploy smoke test does not exercise
>    write-heavy load."*
> *- "Rollouts apply to all shards simultaneously;
>    there is no canary stage."*
> *- "The incident commander role wasn't claimed until
>    11 minutes in; the runbook doesn't say who claims
>    it."*
>
> *Bad entries:*
> *- "The deployer didn't double-check the config."*
> *- "On-call was too slow."*
>
> *Each item in this section should map to at least one
> action item below — if it doesn't, either the
> problem isn't real or you owe an action item.*

## Action items

<specific, owned, dated, prioritized>

| # | Action | Owner | Priority | Due | Tracker |
|---|---|---|---|---|---|
| 1 | Reject `wal.group_commit_window_ms = 0` in config validator | <name> | P1 | YYYY-MM-DD | BRAIN-1234 |
| 2 | Add write-load profile to pre-deploy smoke test | <name> | P1 | YYYY-MM-DD | BRAIN-1235 |
| 3 | Stage config rollouts: canary shard, wait 5 min, then fleet | <name> | P2 | YYYY-MM-DD | BRAIN-1236 |
| 4 | Update `ir-03-escalation-policy.md` with IC-claim trigger | <name> | P2 | YYYY-MM-DD | BRAIN-1237 |

> *Guide — action items:*
> *Every action item must be S-O-D-P: **Specific**
> (concrete enough to know when it's done), **Owned**
> (one named person — not "the team"), **Dated** (a
> real due date, not "soon"), **Prioritized** (P1 = must
> ship within 2 weeks; P2 = within a month; P3 =
> backlog).*
>
> *If an action item doesn't have an owner, it doesn't
> have a future. "Someone should look at this" is
> equivalent to nobody will.*
>
> *Watch for the trap of vague "improve" items —
> "improve the deploy process" is not an action item;
> "stage rollouts via a canary shard first" is.*
>
> *Action items go into your tracker (link them in the
> table). The postmortem is the source of context; the
> tracker is where the work lives.*

## Lessons learned

> *Guide — lessons learned:*
> *One to three short paragraphs of general lessons
> that apply beyond this incident — patterns to watch
> for, principles reinforced, intuitions updated.*
>
> *Good lessons are transferable: "Validators should
> reject operationally-invalid values, not just
> syntactically-invalid ones" applies far beyond this
> incident.*
>
> *Bad lessons are restatements: "We should not have
> deployed a bad config" — that's just the incident
> recurring as a sentence.*

## Appendix: data

> *Guide — appendix:*
> *Anything that supports the analysis but would
> clutter the main body: dashboards screenshots, log
> snippets, the diagnostic bundle path (see
> `dr-01-diagnostic-bundle.md`), graphs, config diffs,
> related incidents.*
>
> *Examples of what belongs here:*
> *- Diagnostic bundle: `s3://brain-incident-bundles/2024-09-12T14-07Z.tar.zst`*
> *- Config diff: link to the commit*
> *- Latency graph: screenshot or query link*
> *- Related incidents: `2024-07-03-similar-config-issue.md`*
> *- Wire-protocol traces, if relevant (see
>    `dr-02-reading-traces-metrics-logs.md`)*

## ↑↑↑ TEMPLATE ENDS HERE ↑↑↑

---

## Where the postmortem goes

A completed Brain postmortem lives at:

```
docs/postmortems/YYYY-MM-DD-<short-slug>.md
```

One file per incident. Filenames sort chronologically;
the slug makes them human-findable.

### Review flow

1. **Draft** — author fills in the template. Status:
   `Draft`. Shared with the responders for factual
   accuracy. Typical turnaround: 2–3 business days.
2. **In review** — author marks status `In review`,
   posts the link in `#engineering`. Anyone can
   comment. The author resolves or replies to every
   substantive comment.
3. **Final** — once action items have owners and
   tracker entries, the author marks status `Final`.
   The postmortem is closed.

For P1 incidents: an explicit review meeting is held.
The author walks the room through the timeline and the
action items. Attendance is open. The point is shared
context, not approval.

For P2 incidents: review is asynchronous unless someone
requests a meeting.

### Closing the loop

A postmortem is **not done** when it's marked Final.
It's done when:

- Every action item has a tracker entry.
- The owners have acknowledged the entries.
- The P1 / P2 action items have due dates within
  reasonable horizons (2 weeks / 1 month).

If action items drift past their due dates, that's its
own signal — either the priority was wrong (rebaseline
it) or the team is overloaded (escalate). Quiet drift
is the path to repeating the incident.

---

## Cross-references

- **`ir-01-how-to-use-runbooks.md`** — the broader
  incident response flow this template plugs into.
- **`ir-02-severity-ladder.md`** — defines P1/P2/P3, so
  the Severity field is filled correctly.
- **`ir-03-escalation-policy.md`** — describes when and
  to whom incidents escalate; useful when writing the
  Timeline and What-went-poorly sections.
- **`ir-04-incident-communication.md`** — describes the
  incident-channel cadence the Timeline draws from.
- **`dr-01-diagnostic-bundle.md`** — the data collected
  during incidents; cite the bundle path in the
  Appendix.

---

## Reviewing this template

This template is itself subject to drift. Review it
**after every postmortem cycle** — if a section was
hard to fill in, or felt like it didn't fit the
incident, the template needs updating, not the
incident. Major revisions: at least annually, or after
any incident where the postmortem itself failed to
capture a useful lesson.

Last validated: *Review this template after each P1
postmortem; structurally re-examine at least once per
year.*
