# 03.13 Open Questions

Wire-protocol-level questions unresolved as of this spec version.

---

## OQ-WP-1: HTTP/2 or QUIC mapping

**Issue.** The protocol is currently TCP-only with custom framing. Wrapping or co-implementing it over HTTP/2 (or QUIC for HTTP/3) would help with deployment in environments that need HTTP-style routing, and could leverage HTTP-aware load balancers.

**Options.**

a) **Stay TCP.** Operators wanting HTTP routing put a TCP-aware proxy in front (Envoy with TCP routing rules, HAProxy in TCP mode).

b) **Add an HTTP/2 frame mapping.** Each Brain frame becomes an HTTP/2 stream message. Adds protocol surface area and dependencies.

c) **Run alongside.** TCP for primary deployment; HTTP/2 mapping for environments that require it. Both supported.

**Recommendation.** Stay TCP for v1. Revisit if user demand for HTTP/2 emerges. The TCP design is intentional and the latency floor benefits from not paying HTTP/2 framing overhead.

---

## OQ-WP-2: Compression

**Issue.** Frame payloads are uncompressed. Some payloads (long text in ENCODE, large RECALL responses with full content) could benefit from compression. Trade-off: CPU cost vs network bandwidth.

**Options.**

a) **No compression.** Simplest. Bandwidth cost grows linearly with corpus size for large RECALL responses.

b) **Per-frame zstd.** Each frame independently compressed. Negotiated via feature flag at handshake.

c) **Stream-level compression.** Long-running streams (SUBSCRIBE) get a dedicated compression context that improves ratios over time.

**Recommendation.** Defer. For typical agent traffic (small frames, frequent), the wire-format overhead is small and compression's per-frame setup cost is comparable to the savings. If we observe a deployment where frames are large enough to benefit, add a feature flag for per-frame zstd.

---

## OQ-WP-3: Server-initiated push beyond SUBSCRIBE

**Issue.** Currently the server only initiates frames on existing client streams (responses) or on subscribed streams. Use cases for unsolicited server push exist:

- "The model has been migrated; please re-issue your last query."
- "A long-running PLAN is making progress; here's a status update."
- "Server is shutting down; please disconnect gracefully."

**Options.**

a) **Even-numbered stream IDs for server push.** Reserve them now for v2 use; document them as forbidden in v1.

b) **Use existing PING/PONG for liveness; add server-push only as needed.** Simpler; revisit when use cases solidify.

c) **Use SUBSCRIBE for everything.** Define a "server-events" subscription that delivers operational notifications.

**Recommendation.** Reserve even-numbered stream IDs (option a). Don't implement server-push in v1; the reservation is cheap and enables future addition without a wire-version bump if the additions fit in the existing frame format.

---

## OQ-WP-4: Out-of-order frame delivery for streaming responses

**Issue.** Streaming responses (RECALL, PLAN, REASON) currently emit frames in order. For PLAN and REASON, results may be discovered out of order (different search branches complete at different times). The server currently serializes them; a parallel-emission mode could deliver faster.

**Options.**

a) **Strictly ordered.** Status quo. Simpler; results arrive as they're computed and serialized.

b) **Optional out-of-order.** A flag at request time enables out-of-order delivery; each result frame carries a sequence number for client-side reordering.

c) **Multiple parallel streams.** Use stream multiplexing — the operation opens multiple stream IDs, one per branch. Adds complexity for marginal gain.

**Recommendation.** Defer. Streaming order is rarely the bottleneck; embedding and search latency dominate. If profiling shows ordering is meaningfully delaying responses, revisit option (b).

---

## OQ-WP-5: Wire-level encryption beyond TLS

**Issue.** TLS protects in-flight bytes but doesn't protect against compromise of the server itself or from a malicious operator. Some deployments may want client-side encryption of sensitive memory content.

**Options.**

a) **Application-level encryption.** Clients encrypt the text before ENCODE; decrypt after RECALL. Brain stores ciphertext. The substrate's embedding model produces vectors from the ciphertext (which is meaningless), so similarity search doesn't work.

b) **Searchable encryption schemes.** Cryptographic protocols (homomorphic encryption, structured encryption) that allow similarity search over encrypted vectors. Significant performance cost; complex; immature.

c) **Server-side enclave.** Run the embedding and search inside a trusted execution environment (Intel SGX, AWS Nitro Enclaves). Hardware-bound.

d) **Don't address it at the wire level.** Leave encryption to the application; document that Brain operators see plaintext.

**Recommendation.** Option (d) for v1. Encryption beyond TLS is a hard problem that requires deeper architectural support; the wire protocol is the wrong layer to solve it.

---

## OQ-WP-6: Bidirectional flow control

**Issue.** TCP provides byte-level flow control via the receive window. Brain's protocol doesn't have application-level flow control beyond that. For high-throughput streaming responses, the client may want to signal "slow down, I'm a slow consumer".

**Options.**

a) **Rely on TCP flow control.** Status quo. The client doesn't read; TCP window narrows; server stalls.

b) **Application-level credits.** Each stream has a credit count; the client grants credits as it processes; the server pauses when out of credits.

c) **Backpressure-aware streaming.** Server emits a frame, waits for ACK, emits next.

**Recommendation.** Option (a) for v1; TCP flow control is sufficient for the workloads we expect. If we observe streams where consumer-side latency varies wildly and the server's emission rate matters, revisit option (b).

---

## OQ-WP-7: Client identity in multi-tenant deployments

**Issue.** The handshake authenticates a session to an `agent_id`. Some deployments may have multiple agents per session (an admin tool that operates across agents). The current protocol requires one agent per connection.

**Options.**

a) **One agent per connection.** Status quo. Admin tools open multiple connections.

b) **Multi-agent sessions.** AUTH establishes a "principal" who can speak as multiple `agent_id`s. Each operation specifies the agent. Validation checks the principal's authorization for that agent.

c) **Principal + impersonation.** A principal authenticates; subsequent operations may include an "impersonate" agent_id, which is checked at the authorization layer.

**Recommendation.** Status quo for v1. The added complexity isn't justified for the workloads we're targeting. Operations admin tools that span agents should use multiple connections — the cost is small.

---

## OQ-WP-8: ALPN identifier

**Issue.** When TLS-wrapped, Brain doesn't currently advertise an ALPN identifier. Some load balancers and routers can use ALPN to decide where to send a connection.

**Options.**

a) **Define an ALPN string.** Suggest `"brain/1"`. Servers advertise it; clients can include it in their ALPN list.

b) **Don't bother.** Brain's TCP port is dedicated; ALPN is unnecessary.

**Recommendation.** Option (a). Define `"brain/1"` as the ALPN string for wire version 1; future versions get `"brain/2"` etc. Cost is negligible; benefit is meaningful for operators using ALPN-aware infrastructure.

---

## OQ-WP-9: Stream cancellation finer than frame-level

**Issue.** Currently, stream cancellation is a frame-level operation (the client sends a CANCEL frame). For very long-running operations (PLAN, REASON), the cancellation latency depends on the server's ability to interrupt the running task.

**Options.**

a) **Status quo.** CANCEL is best-effort; the server interrupts at convenient checkpoints.

b) **Hard cancellation.** CANCEL forces a kill; partial state is discarded.

c) **Cancellation tokens.** Operations periodically check a per-stream cancellation token; CANCEL flips the token; operations notice within a bounded time.

**Recommendation.** Option (c) is the right shape. Detail it in [10. Concurrency + Epoch Model](../10_concurrency_epochs/) — it's a runtime concern, not a wire concern. The wire protocol's CANCEL frame stays as-is; the server's response to it gets richer.

---

*Continue to [`14_references.md`](14_references.md) for references.*
