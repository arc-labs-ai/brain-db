# Phase 11 — Milestone M5 plan

**Task:** HTTP client decision.

**Phase doc target:**
> Decision documented in `client/mod.rs` rustdoc; no churn if we
> choose (a).

**Reads:**
- `crates/brain-cli/src/http/mod.rs` — the existing hand-roll
  (~200 LOC blocking GET/POST/DELETE).
- `.claude/research/brain-http-design.md` §4.2 (HTTP client
  build-vs-buy table).
- `crates/brain-server/Cargo.toml` — current `reqwest` feature
  gating (used only by the summarizer extension).

---

## 1. The decision

M5 is **not an implementation milestone**. It's the point in the
roadmap where we look at the dep graph after M4 and decide whether
brain-http should expose a client. There are three honest paths.

### (a) Document and defer — *recommended*

- `brain-http/src/client/mod.rs` becomes a small docs stub: explains
  why no client ships in v1 and what would trigger one in v2.
- `brain-cli/src/http/mod.rs` stays exactly where it is. ~200 LOC
  blocking GET/POST/DELETE. Works. Has tests. No churn.
- M5 ships as a single docs commit.

### (b1) Move the brain-cli hand-roll into brain-http

- `brain-http/src/client/blocking.rs` adopts the brain-cli code
  verbatim. `brain-cli/src/http/mod.rs` becomes a re-export.
- Net effect: same code, different module path.
- ~250 LOC reorganization, zero functional change.
- Buys: one consumer of brain-http's client; opens the door to a
  matching async client.

### (b2) Add an async client on `hyper-util::client::legacy::Client`

- `brain-http/src/client/{request,response,pool,blocking}.rs`. The
  async client wraps `hyper_util::client::legacy::Client<...>`; the
  blocking facade wraps the async client with `tokio::runtime::Runtime`
  spawn-and-block.
- ~500-700 LOC.
- Buys: a real consumer for things that need to push HTTP (Phase 12
  OTLP export, future webhooks). Lets us drop `reqwest` from the
  summarizer feature later.

---

## 2. Why (a) is the right answer right now

Three reasons.

**No current consumer needs an async HTTP client.**
- brain-cli does blocking and is happy with its hand-roll.
- brain-sdk-rust speaks the binary wire protocol, not HTTP. No HTTP
  client surface needed.
- Phase 11 itself doesn't need one (server-only crate).
- Phase 12 (observability) *might* — but specifying ahead of a
  concrete need is exactly the speculative dep [`AUTONOMY.md`](../../AUTONOMY.md)
  §12 warns about. When Phase 12 shows up with "we need OTLP HTTP
  push," we revisit with real requirements.

**The brain-cli hand-roll is not technical debt.**
- ~200 LOC, no external deps beyond stdlib.
- Tests in `brain-cli/tests/{snapshot,rebuild,worker,...}` all hit it
  successfully via the admin server.
- Replacing it with a brain-http client moves code around without
  changing behaviour. That's churn, not value.

**Building a real async client is M5 + a half.**
- `hyper-util::client::legacy::Client` requires a `Connect`
  implementation. The default `HttpConnector` works for plain HTTP;
  TLS needs `hyper-rustls` wired in. That's another dep.
- Pooling, retry policy, redirect handling, timeout semantics, body
  collection helpers — each is a few hundred LOC of careful work.
- For a single hypothetical consumer (Phase 12 OTLP push) that's a
  bad trade.

---

## 3. What "document and defer" looks like

Two artifacts ship in M5:

### 3.1 `brain-http/src/client/mod.rs` (NEW)

```rust
//! HTTP client surface.
//!
//! **Status (v1, M5):** no client. brain-http ships server-only.
//!
//! ## Why
//!
//! No consumer in the Brain workspace needs an async HTTP client:
//!
//! - `brain-cli` uses a self-contained ~200 LOC blocking client at
//!   [`brain-cli::http`] — works, tested, no external deps.
//! - `brain-sdk-rust` speaks the binary wire protocol over TCP,
//!   not HTTP.
//! - `brain-server`'s only HTTP client is the optional `reqwest`
//!   feature pulled in for the LLM summarizer (Phase 9.15).
//!
//! Building a brain-http client now would be a speculative
//! dependency per [`AUTONOMY.md`] §12 — we'd be writing code that
//! solves no current problem.
//!
//! ## When to revisit
//!
//! Add a client to brain-http when **any one** of these is true:
//!
//! - Phase 12 ships OTLP HTTP push and needs a runtime-shareable
//!   client (rather than a per-request reqwest builder).
//! - A new consumer (e.g. webhook delivery from a worker) needs
//!   async HTTP and the brain-cli blocking client isn't a fit.
//! - The `reqwest` summarizer dep becomes painful enough that
//!   replacing it with our own thin client is worth ~700 LOC.
//!
//! At that point, the natural shape is a builder on
//! [`hyper_util::client::legacy::Client`] with a `Connect`
//! implementation for plain HTTP and a feature-gated `tls` variant
//! pulling in `hyper-rustls`. Pooling, retry, and timeout shapes
//! mirror the server side's `ServerLimits`.
//!
//! For now: this module is intentionally empty. Edit history will
//! show this rustdoc as the design log.
```

### 3.2 Update `docs/phases/phase-11-brain-http.md`

- Tick the M5 box.
- Update the M5 "Done when" line: `decision documented in
  client/mod.rs rustdoc; brain-cli::http unchanged`.

That's it. M5 lands as a single commit, no tests change, verify is
green by inspection.

---

## 4. What this implies for the rest of Phase 11

- **M6 (WebSocket server)**: lands as planned. `tokio-tungstenite`
  handles the WS upgrade internally; doesn't need our HTTP client.
- **M7 (WebSocket client)**: lands as planned. `tokio_tungstenite::
  connect_async` does the upgrade itself; doesn't need our HTTP
  client either.
- **M8 (hardening + benches)**: unchanged.

So choosing (a) costs us nothing downstream. (b1) or (b2) would only
matter if someone wants the surface available now.

---

## 5. Done when

- [ ] `crates/brain-http/src/client/mod.rs` added with the deferral
      rustdoc above.
- [ ] `crates/brain-http/src/lib.rs` mounts `pub mod client;` behind
      the `client` feature flag (already in `Cargo.toml`).
- [ ] Phase doc 11.M5 ticked, "Done when" line updated.
- [ ] `just docker-verify` green.

---

## 6. Open questions

1. **Should brain-cli depend on brain-http?** Path (a) says no — they
   stay independent. The blocking client lives in brain-cli; brain-http
   ships server-only. **Recommendation:** keep them independent. brain-http
   shouldn't acquire brain-cli's dep tree just for one file.

2. **What if I want to delete the `reqwest` summarizer dep?** That's
   path (b2) and a separate Phase-11-or-12 task. Not in scope for
   M5; surfacing here so we don't forget.

3. **Should the `client` feature flag be removed from `Cargo.toml`?**
   It's currently declared but unused. **Recommendation:** keep it.
   The flag is the contract: turning it on means "I want client
   support." When (b1) or (b2) lands, the implementation slots in
   behind the existing flag.

---

## 7. Override paths

If you want (b1) or (b2) instead of (a):

- **(b1) "move the hand-roll":** I'll write a per-file migration
  plan (similar shape to M3). ~250 LOC reorganization, brain-cli
  gains a dep on brain-http. Adds one module under brain-http;
  brain-cli's `http/mod.rs` becomes a 3-line re-export.

- **(b2) "build the async client":** I'll write a full milestone
  plan with deps, module layout, pool design, and tests. ~700 LOC
  production + ~400 LOC tests. Roughly two weeks of work.

Either path I can draft on request. Default is (a).
