# IR-04: Incident communication

**Audience:** every on-call operator and anyone leading
an incident response.

The technical work of fixing an incident is half the
job. The other half is making sure other humans — your
teammates, your stakeholders, your customers — know what's
going on. This document covers the comms half.

The core principle: **silence is more expensive than
imperfect updates**. A teammate who doesn't know what's
happening will improvise (badly) or assume the worst.

---

## Three audiences

Incident comms serves three audiences with different
needs:

| Audience | Wants | Cadence | Channel |
|---|---|---|---|
| **Responders** | Technical detail, what's been tried, what to try next | Continuous | Incident channel |
| **Internal stakeholders** | What's broken, who's working it, when normal | Periodic updates | Incident channel + #engineering |
| **External users** | Are they affected, ETA, workaround | At milestones | Status page, customer emails |

Each layer compresses the one inside it. Responders share
raw technical detail; stakeholders get a curated
summary; external users get the user-facing facts.

---

## The incident channel

For every P1 and P2, open a dedicated incident channel
on whatever platform your org uses (Slack, Teams, etc.).

### Naming

A clear name lets people find the channel:

```
#inc-2024-09-12-brain-high-latency
#inc-2024-09-13-brain-substrate-down
```

The pattern: `#inc-<date>-<service>-<short-description>`.

Avoid:

- Cryptic channel names. `#inc-127` tells nobody
  anything.
- Reusing channels. Each incident is its own channel.
  History from previous incidents pollutes the
  current one's signal.
- Updating an existing channel name mid-incident
  (links break).

### Who's in

The channel starts with just the on-call operator. Others
join as needed:

- **Responders** (second on-call, engineering): join
  automatically when paged.
- **Stakeholders** (manager, product owner): join when
  the incident becomes long or visible enough that they
  need to know.
- **Curious observers**: welcome to read, asked to stay
  quiet unless they have substantive input.

For P1 incidents that may have customer impact, also
loop in the relevant business owner / customer success
lead early.

### What goes in the channel

**Yes:**

- The page summary and runbook reference.
- Commands you ran and what they showed (output
  pasted, or a link to a longer paste).
- Hypotheses (with status: tested, in flight, ruled
  out).
- Decisions (with who made them).
- Coordination notes ("I'm going to try X, you take
  Y").
- Time-stamped milestones (started remediation, fix
  applied, verified, etc.).

**No:**

- Long debates. Discuss in a thread if needed.
- Speculation unmarked as such. "I bet it's the
  embedder" — fine if you mark it as a hypothesis.
- Blame. "Why didn't anyone notice this earlier?" —
  postmortem territory, not real-time channel.

---

## Status updates

For P1 and P2, post structured status updates on a
fixed cadence. Other people are watching the channel
to know how to plan their day; they need predictable
signal.

### Cadence

| Severity | Update cadence |
|---|---|
| P1 | Every 15 min |
| P2 | Every 30 min |
| P3 | As needed (no fixed cadence) |
| P4 | n/a |

These are *minimums*. If something significant happens
between scheduled updates, post immediately. If nothing
has changed for 15 minutes, post anyway to confirm
you're still working it.

### The update template

```
:hourglass: STATUS UPDATE - 15:00 UTC

What we know:
- p99 latency on shard 3 sustained at ~190ms (down from 220ms peak)
- Root cause: HNSW tombstone ratio exceeded threshold

What we're doing:
- Rebuild started at 14:42, currently ~60% complete
- ETA to completion: ~5 minutes

User impact:
- Recall on shard 3 still degraded (a tier-2 of users)
- Other shards unaffected

Next update at 15:15 unless something changes.
```

Five sections: what we know, what we're doing, user
impact, blockers (omitted if none), next update time.

The "next update time" is critical. Anyone reading the
channel should be able to predict when they'll learn
more.

### When to skip the structure

For very fast-moving incidents (sub-15-minute), don't
force the template. Just post:

```
:rotating_light: Bringing shard 3 back up via snapshot restore.
:rotating_light: Restore in progress.
:rotating_light: Restore done. Verifying.
:rotating_light: Verified. Recall normal. Closing.
```

Conversely, for very slow-moving incidents (multi-hour
investigation), the template is essential — without it
the channel becomes unreadable history.

---

## Internal stakeholder comms

For P1 and P2 incidents lasting longer than 1 hour, or
incidents with cross-team impact, post a summary in a
team-wide channel (e.g., `#engineering` or
`#brain-team`) every hour or so:

```
:loudspeaker: BRAIN INCIDENT UPDATE - 15:00 UTC (started 13:21)

Status: ongoing, mitigation in progress.
Severity: P2.
User impact: a subset of recall queries on shard 3 are slow.
Hypothesis: HNSW tombstone accumulation.
Working channel: #inc-2024-09-12-brain-high-latency

Next stakeholder update at 16:00 UTC.
```

This is **derived from** the incident channel's status
updates but written for an audience who isn't reading
the channel. Compress technical detail; emphasise impact
and ETA.

The stakeholder update goes to people who want signal,
not detail. They'll click into the incident channel if
they want more.

---

## External communication

If the incident affects external users, you may need
external comms via:

- **Status page** (e.g. statuspage.io, internal
  equivalent).
- **Customer email or push notification** (for
  significant SLA impact).
- **Public-facing channels** (rare; usually only for
  service-wide outages).

Don't wing this. Most orgs have a designated person /
process for external comms during incidents (customer
success, marketing, comms team). Loop them in *before*
posting publicly.

### Status page updates

If you have a status page, the pattern is:

1. **Investigating.** Posted at incident start. "We're
   aware of elevated latency on the Brain service and
   are investigating."
2. **Identified.** Posted once root cause is known.
   "We've identified the cause and are working on a
   fix."
3. **Monitoring.** Posted once fix is applied.
   "Mitigation is in place; we're monitoring for
   recurrence."
4. **Resolved.** Posted once verified. "The incident
   has been resolved. We'll publish a postmortem
   shortly."

Each update is short (1-2 sentences). The status page
is not the place to explain the technical root cause;
that's the postmortem.

### What *not* to say externally

- Don't share internal jargon. "HNSW tombstone ratio"
  means nothing to your users; "search quality
  degradation" is what they care about.
- Don't blame third parties unless certain ("it was the
  Anthropic API" — fine to say internally, but verify
  before posting to external customers).
- Don't promise specific ETAs unless you're confident.
  "Working on it" is safer than "fixed in 10 minutes"
  and then taking 45.
- Don't say "no data loss" unless you've verified.

---

## After the incident: closing the loop

Once Verify passes, you close the incident:

### In the incident channel

```
:white_check_mark: RESOLVED at 15:42 UTC.

Duration: 1h 21min (alert at 14:31, mitigation at 15:42).
Root cause: HNSW tombstone ratio reached 0.41 on shard 3
following heavy FORGET traffic earlier today. Auto-rebuild
threshold (0.50) hadn't fired.
Remediation: manual rebuild via brain-cli.
User impact: a portion of recall queries on shard 3 returned
slower responses for ~80 minutes.
Follow-ups:
- BRAIN-1234: lower auto-rebuild threshold to 0.35.
- BRAIN-1235: add alert for FORGET-rate-then-tombstone-rise.

Postmortem: required (P2, novel root cause). I'll draft this week.

Channel will be archived in 24 hours.
```

### In the stakeholder channel

A 2-sentence resolution note:

```
:white_check_mark: Brain incident resolved at 15:42 UTC.
P2, ~80 min user impact on shard 3 recall.
Postmortem to follow.
```

### On the status page (if applicable)

```
The incident has been resolved. We are conducting a
postmortem and will share findings.
```

### Filing follow-up tickets

Don't leave action items in the channel. Migrate them
to tickets:

- One ticket per follow-up.
- Each ticket links back to the incident channel.
- Assign owners and target dates.

The channel will get archived; the tickets persist.

---

## Postmortems

The postmortem rule (also in [IR-02](ir-02-severity-ladder.md)):

| Severity | Postmortem? |
|---|---|
| P1 | Always |
| P2 | Usually (skip only if same root cause had a postmortem in last 60 days) |
| P3 | Optional; only if there's something worth learning |
| P4 | Never |

### Who writes it

The on-call operator who handled the incident, usually
with input from anyone who joined. The on-call who *started*
the incident drives the postmortem; if they handed off,
they coordinate with the new responder.

Write within one week of resolution. Stale postmortems
miss context.

### What goes in it

See [`postmortem-template.md`](postmortem-template.md) for
the structure. The headlines:

- **Timeline** — what happened when.
- **Root cause** — the underlying cause, not just the
  trigger.
- **Impact** — users affected, duration, business cost.
- **What went well** — yes, this matters.
- **What went poorly** — honestly.
- **Action items** — specific, owned, dated.

The most important property: **blameless**. The
postmortem is about systems and processes, not about
individual judgment. "On-call hesitated to escalate" is
fine; "Alice was slow to escalate" is not.

### The blameless principle

People make decisions based on the information they had
at the time. The postmortem assumes everyone acted
reasonably; the failure is systemic.

Bad postmortem statement:

> Bob restarted the substrate before verifying the WAL
> was clean. He should have known better.

Good postmortem statement:

> The on-call restarted the substrate before verifying
> the WAL. The runbook doesn't explicitly require WAL
> verification before restart. Action item: add the
> verification step to RB-07.

Same incident; different framing. The second one
*fixes the system*; the first one *blames a person*.

---

## Anti-patterns

### Silence

"Things will be cleaner if I just focus on fixing it."
Wrong. While you're heads-down for 30 minutes, others
are speculating, paging unnecessarily, and assuming
worse.

Even a one-line "still investigating, no new info" is
better than 30 minutes of silence.

### Over-posting

Posting every command you run, every metric you check.
The channel becomes unreadable; people stop reading.

Post **decisions and milestones**, not every action.
Pipe raw output into threads if needed.

### Jargon without translation

If a non-on-call person reads the channel and can't
follow what's happening, the comms are wrong. Translate
internal terms when posting summaries:

- "HNSW recall degraded" → "search quality is dropping"
- "WAL truncation" → "log integrity issue"
- "Tombstone ratio above threshold" → "too many
  forgotten memories cluttering the index"

### Speculating without marking

"It's probably the embedder" — fine if marked as
hypothesis. Bad if it sounds like fact. Future readers
will treat it as fact.

### Resolving prematurely

Posting "resolved" before Verify passes. The fix
"looked like it worked" but didn't fully. Now you have
to walk back the resolution and lose credibility.

Verify first. Resolve after.

---

## Related docs

- [IR-01 — How to use these runbooks](ir-01-how-to-use-runbooks.md)
- [IR-02 — Severity ladder](ir-02-severity-ladder.md)
- [IR-03 — Escalation policy](ir-03-escalation-policy.md)
- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [Postmortem template](postmortem-template.md)
