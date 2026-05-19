# OP-04: TLS certificate rotation

**Severity:** **operator-triggered**. (Routine; **emergency** if
the cert is expiring imminently.)
**Alert:** `BrainTlsCertExpiringSoon` (typically fires at 30, 14,
and 7 days remaining).
**SLO impact:** none if done correctly; **total client outage**
the moment the cert expires.
**Estimated duration:** 30 minutes.
**Skill level:** comfortable with TLS basics (X.509, chain of
trust, key↔cert matching) and the substrate's `[server.tls]`
config block.

When this matters: a renewed cert needs to be in service before
the current one expires; the SAN list changed; or a private key
was exposed.

The cost of getting this wrong:

- **Cert expires:** every TLS client (SDKs, the CLI configured
  for TLS, dashboards) fails to connect. The substrate is
  healthy; nobody can talk to it.
- **Cert doesn't match the key:** the substrate refuses to
  start or reload — substrate outage on top of a cert problem.
- **Missing intermediates:** strict clients reject the
  connection. Lenient clients may succeed at first, masking it.

---

## Am I in the right runbook?

Use this if:

- TLS is enabled (`[server.tls]` is present, with `cert_path`
  and `key_path`).
- You have a new cert + key ready (or are about to obtain one).
- The substrate is healthy; this is a planned swap.

Use a different runbook if you're rotating the **auth token**
([OP-05](op-05-auth-token-rotation.md)) or an **LLM provider
key** ([OP-06](op-06-llm-api-key-rotation.md)) — different
credentials. For other `[server.*]` config changes, see
[OP-08](op-08-config-change-rollout.md).

If the cert has already expired, this is still the right
runbook — work through it with urgency.

---

## Pre-flight checklist

- [ ] **You have the new cert AND the new private key.** Both.
- [ ] **The cert covers the right SAN / hostnames.** A cert for
      `brain.example.com` won't work for `brain-prod.example.com`.
- [ ] **The cert is not already expired.** Operators in a hurry
      have installed expired certs.
- [ ] **The chain is complete.** Leaf → intermediate(s) → root.
      The substrate serves what you hand it; missing
      intermediates → strict clients reject.
- [ ] **You can roll back.** Old cert + key remain on disk. Use
      [DR-03](dr-03-safe-production-access.md) for safe access.
- [ ] **Stakeholders notified** if your deployment requires a
      restart (Step 4 path B).

---

## Step 1 — Verify the new cert's validity

Inspect the new files in a scratch location before staging.

```bash
openssl x509 -noout -text    -in /tmp/newcert.pem | less
openssl x509 -noout -enddate -in /tmp/newcert.pem
# notAfter=Aug  4 12:00:00 2026 GMT  ← well in the future

openssl x509 -noout -ext subjectAltName -in /tmp/newcert.pem
# Must list every DNS / IP clients dial.

openssl x509 -noout -subject -in /tmp/newcert.pem
```

Verify the chain:

```bash
openssl verify -CAfile /tmp/chain.pem /tmp/newcert.pem
# Expected: /tmp/newcert.pem: OK
#   or, against the OS trust store: openssl verify /tmp/newcert.pem
```

`unable to get issuer certificate` means the chain is incomplete.
Fix it before proceeding; strict clients will reject.

---

## Step 2 — Confirm the key matches the cert

The single most common operator mistake. The substrate refuses
to start with a mismatched pair.

For RSA, compare moduli; for ECDSA / Ed25519, compare public-key
fingerprints. Either way the two hashes must be identical.

```bash
# RSA
CERT_MOD=$(openssl x509 -noout -modulus -in /tmp/newcert.pem | openssl md5)
KEY_MOD=$(openssl rsa  -noout -modulus -in /tmp/newkey.pem  | openssl md5)
echo "cert: $CERT_MOD"; echo "key:  $KEY_MOD"

# ECDSA / Ed25519
CERT_PUB=$(openssl x509 -noout -pubkey -in /tmp/newcert.pem | openssl sha256)
KEY_PUB=$(openssl pkey -pubout  -in    /tmp/newkey.pem      | openssl sha256)
```

If they don't match, you have the wrong key file. Do not proceed
until they match.

---

## Step 3 — Stage the new files

The substrate reads paths from `[server.tls]`:

```toml
[server.tls]
cert_path = "/etc/brain/tls/server.crt"
key_path  = "/etc/brain/tls/server.key"
```

Keep the old files under a dated suffix; install the new ones at
the canonical paths. Rollback is then one `mv` away.

```bash
sudo install -d -m 0750 -o brain -g brain /etc/brain/tls
TS=$(date -u +%Y%m%dT%H%M%SZ)

# Back up current files in place.
sudo cp -a /etc/brain/tls/server.crt /etc/brain/tls/server.crt.$TS.bak
sudo cp -a /etc/brain/tls/server.key /etc/brain/tls/server.key.$TS.bak

# Install new files atomically.
sudo install -m 0644 -o brain -g brain /tmp/newcert.pem /etc/brain/tls/server.crt.new
sudo install -m 0600 -o brain -g brain /tmp/newkey.pem  /etc/brain/tls/server.key.new
sudo mv /etc/brain/tls/server.crt.new /etc/brain/tls/server.crt
sudo mv /etc/brain/tls/server.key.new /etc/brain/tls/server.key

ls -l /etc/brain/tls/
# server.key MUST be 0600 (or 0640 with the brain group).
```

A world-readable private key is its own security incident. Fix
permissions before reloading.

---

## Step 4 — Reload (or restart)

Two paths, depending on whether the build supports SIGHUP TLS
reload.

### Path A: SIGHUP hot-reload (preferred)

`tokio-rustls` re-reads `cert_path` / `key_path` on SIGHUP and
rotates the cert in memory. Existing sessions keep their old
cert; new connections present the new one.

```bash
# Confirm SIGHUP TLS reload is supported in this build.
brain-cli admin features | jq '.tls_sighup_reload'
# Expected: true

sudo systemctl kill -s HUP brain-server
#   or: sudo kill -HUP "$(pgrep -x brain-server)"

# Watch for the reload event.
sudo journalctl -u brain-server -n 50 --no-pager | grep -i tls
# Expected: tls: reloaded certificate, not_after=<date>
```

If SIGHUP isn't supported (older build, or
`tls_sighup_reload: false`), use Path B.

### Path B: Full restart

A restart re-reads the cert files on startup. This briefly drops
all client connections.

```bash
sudo systemctl restart brain-server

for i in {1..30}; do
    curl -fsS http://127.0.0.1:9091/healthz && break
    sleep 2
done
```

For multi-host deployments, follow
[OP-01](op-01-rolling-restart.md) for the rolling pattern —
restart one host, verify it serves the new cert, then proceed.

---

## Step 5 — Verify

A reload only succeeded if a fresh handshake serves the new cert.

```bash
# What's actually on the wire.
openssl s_client -connect brain-host:9090 -showcerts </dev/null 2>/dev/null \
    | openssl x509 -noout -dates -subject -ext subjectAltName
# Expected: notAfter=<new expiry>; SAN lists every hostname clients dial.

# Confirm served cert matches what's on disk.
SERVED=$(openssl s_client -connect brain-host:9090 </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256)
ONDISK=$(openssl x509 -noout -fingerprint -sha256 -in /etc/brain/tls/server.crt)
echo "served: $SERVED"; echo "ondisk: $ONDISK"
# Identical → the substrate is serving the file you installed.
```

If the build exposes a TLS info command, also:

```bash
brain-cli admin tls-info
# not_after matches new cert; fingerprint matches.
```

Smoke-test a real client:

```bash
brain-cli --tls encode "tls rotation smoke $(date +%s)"
brain-cli --tls recall "tls rotation smoke"
```

Watch dashboards for 5–10 minutes for any handshake-failure spike.

---

## Rollback

The dated `.bak` files from Step 3 are your rollback.

```bash
ls -1t /etc/brain/tls/server.crt.*.bak | head -1
# /etc/brain/tls/server.crt.20260519T120000Z.bak

TS=20260519T120000Z
sudo cp -a /etc/brain/tls/server.crt.$TS.bak /etc/brain/tls/server.crt
sudo cp -a /etc/brain/tls/server.key.$TS.bak /etc/brain/tls/server.key

# Reload the same way as Step 4.
sudo systemctl kill -s HUP brain-server
#   or: sudo systemctl restart brain-server
```

Re-verify with `openssl s_client` that the **old** cert is being
served again.

If you rolled back because the new cert was malformed, file a
CA / PKI ticket; don't re-attempt with the same files. If the
old cert is itself near expiry, you've bought hours, not weeks —
get a corrected new cert urgently.

---

## Post-operation

Post in your team channel:

```
:white_check_mark: TLS cert rotated at HH:MM UTC.
Old not_after: <date>   New not_after: <date>
Method: SIGHUP reload / full restart
Issues: <none / list>
```

Update calendar / PKI tooling with the new expiry. Confirm
`BrainTlsCertExpiringSoon` has cleared (one scrape interval).
Archive the `.bak` files after about a week — old private keys
shouldn't linger indefinitely.

---

## Pitfalls

### Key doesn't match cert

The single most common mistake. The substrate refuses to start,
or SIGHUP logs a TLS-load failure and keeps serving the old
cert. Always run Step 2 *before* touching the live files. If you
skipped it and the substrate now won't start, you're in
[RB-01](rb-01-substrate-down.md).

### Missing intermediates

A leaf-only `cert_path` means strict clients (those without the
intermediate cached) reject the connection. The substrate serves
what you hand it. Use `openssl s_client -showcerts` to see what's
on the wire — leaf first, then each intermediate in order.

### Private key permissions wrong

A world-readable `server.key` is a security incident in its own
right. Always `chmod 0600` (or `0640` with the right group).

### Client tools caching the old cert

`s_client`, browsers, and some SDK clients cache sessions. The
same host may briefly show the old cert via session reuse. Force
a fresh handshake (`-no_ticket -reconnect 0`) or restart the
client process.

### Rotating only the cert, not the key

If your CA regenerated the key and you only updated `cert_path`,
the substrate will fail the handshake. Rotate the pair together
unless you explicitly issued the CSR with the existing key.

### No alerting on expiry

The best way to avoid a 3 AM cert-expiry incident is to never
let a cert reach 7 days remaining without an operator already on
it. Wire `BrainTlsCertExpiringSoon` at **30 / 14 / 7** days.
Ideally, automate renewal (Let's Encrypt + certbot, ACME, or
your PKI's equivalent) so Steps 1–4 run unattended at ~30 days
remaining.

### No backout

The dated `.bak` naming in Step 3 is not optional. Without it,
rollback means re-issuing from your CA — hours, not minutes.

---

## Related runbooks

- [OP-01 — Rolling restart](op-01-rolling-restart.md) — multi-host
  pattern for Step 4 Path B.
- [OP-05 — Auth token rotation](op-05-auth-token-rotation.md) —
  different credential.
- [OP-06 — LLM API key rotation](op-06-llm-api-key-rotation.md) —
  different credential.
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)
  — other `[server.*]` changes.
- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md) — if
  the reload / restart leaves the substrate refusing to start.
- [DR-03 — Safe production access](dr-03-safe-production-access.md)
  — access pattern for touching production cert files.

---

## Last validated

*Update on first use.*
