# 13.09 SDK Versioning

How SDK versions evolve, what compatibility is guaranteed, and how to upgrade.

## 1. Semantic versioning

SDK versions follow [SemVer](https://semver.org/): `MAJOR.MINOR.PATCH`.

- **PATCH**: bug fixes; no API changes. Drop-in upgrade.
- **MINOR**: backward-compatible additions. Old code still works.
- **MAJOR**: breaking changes. Migration required.

E.g.: `1.0.0` → `1.1.0` adds methods; `1.1.0` → `1.1.1` fixes bugs; `1.1.1` → `2.0.0` may break existing code.

## 2. Substrate compatibility

SDK and substrate versions are coordinated:

| SDK version | Substrate version |
|---|---|
| 1.x | 1.x |
| 2.x | 2.x |
| 1.x ↔ 2.x | not guaranteed |

Within a major: any minor combination works. Cross-major: untested by default.

## 3. The "wire protocol negotiation"

When a client connects:

```
1. Client sends supported wire-protocol versions (e.g., [v1.0, v1.1, v1.2]).
2. Server responds with chosen version (e.g., v1.1 — the highest both support).
3. Subsequent frames use v1.1.
```

If no overlap:
- Server returns `IncompatibleVersion` error.
- Client must update.

This handshake is the version-compat enforcement point.

## 4. The "additive" change rule

Within a major:
- New opcodes can be added.
- New fields can be added (with defaults).
- Old opcodes/fields are stable.

Old SDKs can talk to new substrates: they just don't use the new features.

New SDKs can talk to old substrates: they detect missing features and error gracefully.

## 5. Deprecation

When something is deprecated:

- Marked `@deprecated` (or equivalent) in code with a warning.
- Documented in release notes with migration path.
- Removed in the next major version.

Example:

```rust
#[deprecated(since = "1.5.0", note = "Use `encode_v2` instead")]
pub fn encode_legacy(...) -> ...;
```

The SDK gives users a transition period before forced changes.

## 6. The deprecation timeline

```
v1.5.0: Feature X deprecated. Warning logged on use.
v1.6.0 - v1.x: Feature X still works; warning continues.
v2.0.0: Feature X removed. Compile error / runtime error.
```

So users have a major-version cycle to migrate. Typically 6-18 months.

## 7. The "removal release notes"

Major version bumps document removals:

```
v2.0.0 release notes:
  Removed:
    - Client.encode_legacy (deprecated since v1.5.0)
    - encode option `with_legacy_id` (deprecated since v1.7.0)
  Migration guide: <link>
```

Users can plan migrations from these notes.

## 8. The "breaking change" hierarchy

Different kinds of breaks:

- **API breaking**: a method is removed or its signature changes. Users must edit code.
- **Behavior breaking**: a method's behavior changes; old code compiles but produces different results. Users must understand the change.
- **Performance breaking**: a method's performance changes substantially. May not require code changes.

API breaking changes are loudest (compile errors). Behavior changes are the most insidious; release notes must call them out clearly.

## 9. The "lockstep" SDK update

When the substrate adds a new feature, the SDK adds support in the same release cycle:

```
Substrate v1.5.0: Adds new opcode FOO.
SDK v1.5.0: Adds new method client.foo().
```

Users upgrade both together to use the new feature. Old SDKs against new substrates silently don't expose FOO.

## 10. The "feature detection"

For applications targeting multiple substrate versions:

```rust
if client.supports(Feature::ConsolidationApi) {
    client.consolidate(...).await?;
} else {
    // Fall back
}
```

Feature detection lets applications use new features when available, fall back when not.

## 11. The "version pinning" tradeoff

In production:
- Pin the SDK version (don't auto-upgrade major).
- Test SDK upgrades in staging first.
- Coordinate with substrate upgrades.

The SDK's package manager (Cargo, pip, etc.) supports version pinning. Users should use it.

## 12. The minor/patch upgrade path

For minor and patch upgrades:

- Drop-in replacement.
- Run tests to verify.
- Deploy.

Most upgrades within a major are this simple.

## 13. The major upgrade path

For major upgrades:

- Read the migration guide.
- Identify deprecated features in your code.
- Update them to new equivalents.
- Run tests.
- Deploy.

The SDK provides a `cargo brain-migrate` (or similar) tool that scans for known patterns and suggests updates:

```
$ cargo brain-migrate
Found 3 uses of deprecated API:
  src/main.rs:15: client.encode_legacy(...) → client.encode(...)
  src/main.rs:42: client.encode_legacy(...) → client.encode(...)
  ...
Apply changes? [y/n]
```

Migration becomes assisted, not entirely manual.

## 14. The cross-language version coordination

Each language's SDK has its own version number. They're coordinated:

- Brain v1.0.0 = brain-rust v1.0.0 = brain-python v1.0.0 = brain-typescript v1.0.0.
- Substrate features in v1.x are in all SDKs at v1.x.
- Patch versions may diverge (a bug in Python SDK doesn't affect Rust).

Major versions are aligned. Patch versions are independent.

## 15. The "long-term support" tracks

For enterprise users:

- `1.0.x` LTS — supported for 2 years.
- `1.x` — supported until v2.0 release.
- `2.x` — replaces 1.x as the active major.

Bug fixes are backported to LTS. Security fixes are always backported.

## 16. The "early access" for new features

For features in development:

- An "experimental" feature flag.
- Opt-in via configuration.
- May change without warning.

```rust
let client = Client::builder()
    .enable_experimental_features(true)
    .build();
```

Production code should not enable experimental features.

## 17. The "stable" promise

Marked-stable APIs:
- Won't change in incompatible ways within a major.
- Documented in stable docs.
- Tested in conformance suite.

Marked-experimental APIs:
- May change at any time.
- Documented separately.
- Not tested in conformance suite.

The line is clear; users know what's stable.

## 18. The "0.x" pre-1.0 phase

Before v1.0:
- Anything can change.
- Releases are tagged 0.x.
- Users should expect breaking changes between 0.x releases.

Brain's pre-release phase will be in 0.x. The substrate and SDK versions stabilize together at 1.0.

---

*Continue to [`10_open_questions.md`](10_open_questions.md) for unresolved questions.*
