# Upgrade Brain

Brain follows SemVer for the binary + wire-protocol stable surfaces
that hit `v1.0.0`. Within a major version, upgrades are
backwards-compatible.

## Compatibility matrix

| Upgrade type | Wire protocol | Data files | Action |
|---|---|---|---|
| **Patch** (`v1.0.0 → v1.0.1`) | unchanged | unchanged | drop-in restart |
| **Minor** (`v1.0 → v1.1`) | additive only (new opcodes, new fields) | additive only (new tables / new metric families) | drop-in restart |
| **Major** (`v1 → v2`) | breaking | may require migration | follow the major-version migration guide (TBD) |

The `brain_build_info{version=...}` metric and the
`HELLO/WELCOME` handshake's `chosen_version` carry the running
version end-to-end so operators can verify post-upgrade.

## Pre-upgrade checklist

1. **Take a snapshot.** This is the rollback floor:
   ```bash
   brain-cli snapshot create --shard 0 --addr <admin-addr>
   brain-cli snapshot list --addr <admin-addr>
   ```
   Record the snapshot IDs.

2. **Verify the existing version.**
   ```bash
   curl -s http://<metrics-addr>/metrics \
     | grep brain_build_info
   ```

3. **Check the CHANGELOG.** Look for breaking-change notices,
   migration steps, and any "before upgrading" prerequisites in
   [`CHANGELOG.md`](../../CHANGELOG.md).

4. **Drain a canary first** (multi-node deployments). Upgrade one
   shard / host before the rest.

## Patch / minor upgrade

For a single-host deployment:

```bash
# 1. Stop the substrate.
systemctl stop brain-server

# 2. Replace the binary.
sudo install -m 755 /path/to/new/brain-server /usr/local/bin/

# 3. Start.
systemctl start brain-server

# 4. Verify.
systemctl status brain-server
curl -s http://<metrics-addr>/healthz
curl -s http://<metrics-addr>/metrics | grep brain_build_info
```

For multi-host: rolling restart. Brain v1 is single-host per
shard, so each shard goes briefly offline during its own restart —
clients should retry (SDK does this by default).

## Rolling back

If the new version misbehaves:

```bash
# 1. Stop.
systemctl stop brain-server

# 2. Restore the previous binary.
sudo install -m 755 /path/to/old/brain-server /usr/local/bin/

# 3. (If wire / data changed) restore the pre-upgrade snapshot.
brain-cli snapshot restore --id <pre-upgrade-id> --confirm

# 4. Start.
systemctl start brain-server
```

Patch upgrades are guaranteed binary-compatible, so the snapshot
restore is only needed if the data path changed (which a patch
release shouldn't).

## Major-version migration (v1 → v2, TBD)

Major-version migration is a documented procedure per release. The
v2 migration guide will live at `docs/guides/upgrade-v2.md` when
v2 is cut.

The default v1 → v2 expectation:
- Read-back compatibility for v1-written data via a migration tool.
- v1 client SDKs work against v2 server (negotiated down via
  HELLO/WELCOME).
- v2 client SDKs against v1 server: the SDK refuses with a clear
  version-mismatch error.

## Wire-protocol versioning

Spec §03/02 §3 defines the HELLO/WELCOME handshake's version
negotiation. The substrate accepts a range of protocol versions;
the SDK picks the highest mutually-supported. Versions are
explicit in the wire (frame header → `protocol_version`).

Adding a new opcode = minor bump. Reordering fields or changing a
type = major bump. The `brain-protocol-version-bump` skill checks
PRs for breaking changes.

## Upgrade test matrix

The Phase 13 acceptance gates include migration testing per spec
§16/08 §11:

- Load v1 data with v1 binary: works.
- Load v1 data with v1.x binary (newer): works.
- Load v1.x data with v1 binary: read-only or error (forward
  compatibility limited).

For each release, the operator runs a smoke test against a copy
of production data before promoting.
