# brain-crypto

> Sealing for the secrets Brain holds on a tenant's behalf.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Seals and opens the secrets Brain keeps for a tenant — today, the LLM provider
keys that pay for extraction and HyPE. A tenant's OpenAI or Anthropic key bills
to their account and is useful to an attacker on its own, so the credential
store holds only ciphertext and the plaintext exists just long enough to sign
one outbound provider call. The read path returns a provider, a model and a
display fingerprint; nothing that leaves this crate can reconstruct a key.

The crate is its own unit so that every use of a cipher in Brain is in one place
to review, and so that crates which never touch a secret do not link a
cryptographic implementation. Sealing is ChaCha20-Poly1305 via `aws-lc-rs` —
the same provider Brain's TLS stack already installs for rustls, so this adds
no second cryptographic implementation to the binary or to the audit surface.

**The binding is load-bearing.** Each secret is sealed against the namespace and
provider it belongs to, and that pair is authenticated alongside the ciphertext
(`Binding`). Moving one tenant's stored row into another tenant's slot fails to
open rather than yielding a usable key — which is what stops write access to the
credential table from becoming a way to spend someone else's provider budget.

**Rotation needs no coordinated rewrite.** The id of the key-encryption key used
for a seal travels in the clear beside the ciphertext, and several keys can be
live at once. Keys are listed one per line as `<id> <hex>`; the highest id is
the one new seals use, so appending a line rotates while existing rows stay
readable under the old key.

## Key modules

| Module | Purpose |
|---|---|
| `sealed` | `KeyRing`, `SealedSecret`, `Binding`, `generate_kek`, `fingerprint`, and the `CryptoError` taxonomy. A 32-byte key-encryption key (`KEK_LEN`) and a per-seal nonce. |
| `kek_file` | Where a deployment's keys come from: `load_or_create` generates one into the data directory on first need, while `from_supplied` / `from_supplied_hex` take one from a secret manager, an environment variable or a mounted file. |

## What the generated key protects

A key generated into the data directory sits in the same directory as the rows
it seals. That covers the ways credential ciphertext ordinarily escapes — a
database file copied out, a backup archive, a snapshot restored elsewhere,
anything that reads the metadata store — because none of those carry the key
file with them. It does **not** cover someone holding the whole data directory.
For that the key has to live somewhere Brain's disk is not, which is what
supplying one is for.

## Where it fits

Depends on `aws-lc-rs` (AEAD + RNG), `zeroize` (plaintext lifetime), `rkyv`
(`SealedSecret` is persisted as a row), and `thiserror`. It has no `brain-*`
dependencies. Consumed by `brain-metadata`, which stores the sealed rows, and
by `brain-server`, which loads the key ring at startup.

## Spec

- Tenancy & per-namespace credential authority:
  [`../../spec/20_tenancy/00_purpose.md`](../../spec/20_tenancy/00_purpose.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
