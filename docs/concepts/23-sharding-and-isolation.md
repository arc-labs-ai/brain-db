# 23 — Sharding and isolation

A single CPU has limits. A single shard's executor
(chapter 22) eventually saturates under load. The answer
is to run **multiple shards** in the same server, each
handling a slice of the workload. This chapter explains
how Brain partitions work across shards, how it isolates
agents from each other, and what's deliberately *not*
shipped in v1.

---

## Why shard at all

A single Glommio executor handles one CPU's worth of
work. On a modern server with 32 cores, a single-shard
deployment uses one core; the other 31 sit idle.

Sharding solves this. The substrate spins up N shards
(configurable; typical 4–32), each owning a slice of the
data, each pinned to its own CPU. A request arrives at
the edge, gets routed to the shard that owns the
relevant data, and runs on that shard's executor.

The win is **horizontal scaling within a single
server**: N cores → N shards → roughly N× the throughput
(minus the edge's overhead, which scales with Tokio's
work-stealing pool).

What sharding does *not* solve:

- Cross-server scaling. v1 doesn't have multi-node
  clustering. Brain runs as a single process per server.
- Single-shard write throughput. If you have one
  "hot" shard receiving most of the writes, the other
  shards being idle doesn't help.
- Truly enormous data sets (billions of memories). A
  single server has memory and disk limits even with
  sharding.

For most production workloads (agent counts in the
thousands to hundreds of thousands, memory counts in the
millions to tens of millions per agent), a single
multi-shard server is plenty.

---

## Partitioning by agent

Brain partitions on the **`agent_id`** field. Every
memory carries the `agent_id` of the agent that encoded
it; the shard a memory belongs to is determined entirely
by its `agent_id`.

```
shard_for(agent_id) = BLAKE3(agent_id) mod num_shards
```

A hash of the agent ID, modulo the number of shards. The
hash distributes agents uniformly across shards on
average; the modulo picks one.

> **What is BLAKE3?**
>
> BLAKE3 is a modern cryptographic hash function — fast,
> parallelisable, drop-in replacement for SHA-2. Brain
> uses it for routing, content addressing, and several
> other places where a strong-but-fast hash is needed.
>
> See [BLAKE3 official site](https://github.com/BLAKE3-team/BLAKE3).

This is **hash-based partitioning** — the simplest
sharding scheme. Once you know the agent_id and the
shard count, you know exactly which shard owns the
agent's data.

**Why hash and not range?** Range partitioning ("agents
A–F on shard 0, G–L on shard 1, …") creates hot spots —
some letter ranges have more agents than others. Hash
partitioning spreads load uniformly.

**Why on agent_id and not on memory_id?**

- Per-agent locality. All of an agent's memories live on
  one shard. A recall query touching the agent's
  memories doesn't fan out across shards.
- Simple security boundary. Agents can't see each
  other's memories; the shard boundary is the isolation
  boundary.
- Predictable performance. An agent's recall latency
  doesn't depend on shard load globally — only on
  *its* shard.

---

## Per-agent isolation

Memories are tagged with their `agent_id` at encode time.
Recall queries are *implicitly* filtered by agent: you
can only see memories owned by *your* agent.

This is the foundation of **per-agent isolation** —
Brain's basic multi-tenancy story. Two agents in the same
deployment can't see each other's memories.

The mechanics:

1. Every encode includes the session's `agent_id`. The
   memory's metadata row carries this field.
2. Every recall filters on `agent_id` before returning
   results. The substrate enforces this at the
   recall-handler level; the client *cannot* override it.
3. The wire protocol's authentication step (chapter 04 of
   the architecture tier) establishes which `agent_id`
   the session is bound to. Different sessions have
   different bindings; no session can claim to be a
   different agent post-handshake.

The isolation is **soft**, not hardware-level:

- Two agents share the same shard if their hashes
  collide.
- They share the same redb file, the same arena, the
  same WAL.
- A bug in the substrate's filtering code could leak
  one agent's data to another.

For *strong* isolation (compliance, regulated
multi-tenancy), the recommended pattern is **one Brain
instance per tenant**. Same binary, separate data
directories, separate processes, hardware-isolated where
the OS lets you. Brain doesn't try to be a multi-tenant
SaaS database; it's a substrate that you compose into
whatever isolation model you need.

---

## Routing a request to its shard

When a request arrives at the edge:

1. **TCP accept.** The Tokio edge layer handles the
   connection.
2. **Frame read.** The edge reads one wire frame
   (chapter 02 of the architecture tier) at a time.
3. **Authentication.** The handshake binds the session
   to an `agent_id`.
4. **Routing.** For each request frame, the edge
   computes `shard = BLAKE3(agent_id) mod N` and
   forwards the request to that shard's flume channel
   (chapter 22).
5. **Shard processing.** The shard's executor pulls the
   request, runs it, returns a response.
6. **Response.** The edge writes the response frame
   back to the client.

The routing step is microseconds. The hash is cheap and
the modulo is one instruction. Most of the time is in
steps 5 and 6.

A subtle point: the same connection might route to
*different shards* on consecutive requests, if the
agent's session changes (rare) or if the connection
serves multiple agents (uncommon but allowed). The
shard isn't sticky to the connection — only to the
agent.

---

## Special routing for `MemoryId`-bearing requests

A `MemoryId` (chapter 05) already encodes its shard in
its high bits:

```
MemoryId  (16 bytes)
[ shard_id ][ slot_id ][ slot_version ][ reserved ]
```

So requests like `forget(memory_id)` or `link(...)` that
already carry `MemoryId`s can route on the shard bits
directly, bypassing the BLAKE3 hash. This is purely a
short-cut; the result is the same shard, because the
shard is determined at *encode time* from the agent.

If you have a stale `MemoryId` from before the cluster's
shard count changed (which v1 doesn't allow at runtime
anyway), the shard bits might point at a shard that
doesn't exist. The result is `NotFound`.

---

## Shard count is fixed at deployment time

The number of shards is set in the server's config and
**doesn't change without a redeploy**. The hash-based
routing depends on the modulo being consistent.

Changing the shard count would re-route every existing
agent — agent A on the 4-shard layout might map to
shard 3, but on the 8-shard layout might map to shard 1.
A re-shard isn't a config change; it's a data migration.

**Picking the right shard count** at deployment time:

- **More than N** (where N = available CPU cores) doesn't
  help. Two shards on one core is worse than one shard
  on that core.
- **Less than N** leaves cores idle. Fine if you have
  spare cores for the edge, embedding, etc., but
  generally you want shards ≈ cores − overhead.
- **A power of two** isn't required, but it makes some
  modular-arithmetic tooling easier.

For most deployments, `shard_count = num_cores - 4`
(reserving cores for the edge, OS, etc.) is a reasonable
starting point. Tune by observing per-shard CPU
utilisation.

---

## Consistent hashing (and why v1 doesn't have it)

A more sophisticated sharding scheme — **consistent
hashing** — lets you add or remove shards without
re-routing every agent. Only a slice of agents moves; the
rest stay put.

> **What is consistent hashing?**
>
> A hashing scheme where adding or removing buckets only
> moves a fraction (~1/N) of keys to new buckets, rather
> than re-hashing every key. The standard reference for
> distributed systems with elastic membership.
>
> See [Wikipedia: Consistent hashing](https://en.wikipedia.org/wiki/Consistent_hashing).

Brain doesn't use consistent hashing in v1. The reason:
v1 doesn't support **adding or removing shards at
runtime**. The shard count is fixed; consistent hashing
would buy you nothing.

Why no elasticity in v1? Because runtime shard changes
require **rebalancing**: moving each migrating agent's
data from the old shard to the new shard, with
consistency. That's a meaningful piece of engineering
that v1 doesn't ship.

The honest story: if you grow out of one shard count,
you redeploy with more, and run a migration (export
from the old layout, import into the new). Most
deployments don't grow past their initial sizing for
years; the operational cost is low.

---

## What replication v1 doesn't have

Per-shard data lives on one server's disks. There's no
replication, no failover, no automatic backup.

If the server's disks die, the data on those disks dies.
You restore from snapshots (chapter 18), which means
you should be running the snapshot worker and copying
snapshots off-server.

For high availability, the deployment pattern is:

1. **Snapshots** — turn on the snapshot worker, snapshot
   every hour (or more).
2. **Off-server backup** — copy snapshots to S3 or
   equivalent. The architecture tier's deployment
   chapter covers patterns.
3. **Restore drills** — periodically restore a snapshot
   to verify the backup chain works.

Native multi-replica HA is on the roadmap but not in v1.
Brain is a substrate; if you need HA *today*, you wrap
it with external replication.

---

## What about reads from a tombstoned shard

If a shard fails to spawn (corrupt arena, missing
metadata file, etc.), the server keeps running but that
shard is *down*. Requests routed to it return errors.

The substrate is **fail-stop**: it refuses to serve
potentially-wrong data rather than serving silently
corrupted data. Half the agents get errors; the operator
investigates and restores from snapshot.

This is the right behaviour for a substrate that
prioritises correctness over uptime. A different system
might silently fall back to a degraded mode; Brain
prefers loud failure.

---

## Authentication and authorisation (briefly)

The wire protocol's handshake (chapter 02 of the
architecture tier) does two things:

1. **Authenticate**: prove the client has the right to
   bind to a given `agent_id`.
2. **Establish a session**: the session is bound to one
   `agent_id` for its lifetime.

Two auth methods ship in v1:

- **No auth** (development only). The client claims an
  `agent_id` and the server believes it.
- **Token auth.** A pre-shared token (or one fetched
  from an external IAM system) is presented in the
  handshake. The server validates it against a
  configured allow-list.

Once the session is established, the substrate enforces
per-agent filtering. There's no per-resource ACL system
in v1; if you can authenticate as agent A, you can do
anything to agent A's data.

For more sophisticated auth (per-resource permissions,
RBAC, etc.), the production pattern is to wrap Brain in
a proxy that does the auth-and-authorisation step,
then talks to Brain as a trusted backend.

---

## TLS

The wire protocol supports TLS for encryption in
transit. Configured per-server (single set of cert + key
or one per listening port). The handshake happens
before the protocol's HELLO/AUTH; TLS termination is
the edge's job.

Internal traffic (between processes inside a Brain
deployment, where they exist) doesn't currently use
TLS. Multi-process / multi-node TLS is part of the
future-clustering story.

> **What's TLS?**
>
> Transport Layer Security — the modern protocol for
> encrypted network communication (the "S" in HTTPS).
> Successor to SSL.
>
> See [Wikipedia: Transport Layer Security](https://en.wikipedia.org/wiki/Transport_Layer_Security).

---

## Recap

- Brain partitions data across **shards** in a single
  server, scaling horizontally on cores.
- The partition key is the **`agent_id`**, hashed with
  BLAKE3 modulo `shard_count`.
- Each agent's memories live on **one shard**. Per-agent
  isolation falls out for free.
- The shard count is **fixed at deployment time**. No
  runtime resizing; no consistent hashing in v1.
- **Replication is not in v1.** Run snapshots + off-server
  backups for durability. Native HA is on the roadmap.
- The substrate is **fail-stop**: a corrupt shard
  refuses to serve rather than serving wrong data.
- Auth and authorisation are minimal: per-agent
  isolation is enforced; richer ACLs are external.

---

## Where to go next

- **The architecture-tier deep dive:**
  [`../architecture/01-system-architecture.md`](../architecture/01-system-architecture.md).
- **Per-shard concurrency:** [chapter 22](22-concurrency-and-async.md).
- **What durability means here:** [chapter 18](18-storage-and-durability.md).
- **Deployment guides:** [`../guides/deployment/`](../guides/deployment/).
- **Security guides:** [`../guides/security/`](../guides/security/).
