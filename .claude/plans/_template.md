# Plan: Phase NN [— Task MM, Title]

**Status:** draft | awaiting-confirmation | approved | superseded
**Date:** YYYY-MM-DD
**Author:** Claude (autonomous)
**Estimated commits:** 1 | 2 | 3

---

## 1. Scope

What this sub-task does, in 2–4 sentences. Be explicit about what it does NOT do (defers to a later sub-task, owned by another phase, etc.).

## 2. Spec references

- `spec/.../foo.md` — relevant sections with one-line summaries.
- Quote any binding constraints verbatim.

## 3. External validation

For work that involves new frameworks, libraries, algorithms, or architectural choices. Skip when purely internal wiring.

- **Library / framework** — what version, what features, what known issues.
- **URL**: short excerpt + link.
- **URL**: ...

If skipped, write "Not applicable — internal wiring only."

## 4. Architecture sketch

Types, modules, public surface. ASCII or short prose. Show the shape, not every line.

```
mod foo;
pub struct Foo { ... }
impl Foo {
    pub fn bar(&self) -> Result<...>;
}
```

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Chosen approach | ... | ... | ✓ |
| Alt A | ... | ... | rejected because ... |
| Alt B | ... | ... | rejected because ... |

## 6. Risks / open questions

- Risk 1: ... — mitigation: ...
- Open question: spec ambiguity at `...` (see `*_open_questions.md`). Resolution: ...

## 7. Test plan

Map each "Done when" item from the phase doc to one or more tests:

- `[ ] Done-when bullet` ← `test_foo_does_x`, `test_bar_round_trips`.

Add property tests, fuzz coverage, or chaos tests where applicable.

## 8. Commit shape

- Commit 1: title — what it contains, why it's separable.
- Commit 2: title — ...

## 9. Confirmation

After this plan is approved, proceed to implementation.
