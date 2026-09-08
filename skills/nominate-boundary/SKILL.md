---
name: nominate-boundary
description: Use when deciding which interface in a codebase is worth naming as a ha-ha boundary — "what should we nominate here?", "is this a boundary?", adding ha-ha to a project for the first time, or reviewing whether an existing nomination is pulling its weight. Covers what makes something a boundary, finding the anchor symbol, and writing the first rule.
---

# Nominating a boundary

A **ha-ha** is an interface the compiler can't see as one unit — a contract that
is real to the people working on the code but invisible to the type system. This
skill is about finding one worth naming. For getting the file syntax right once
you have, use `ha-ha-config`.

## What makes something a boundary

Not every module is a boundary. The test is whether there are **two sides that
change independently**, and whether a change on one side can hurt the other in a
way the compiler won't catch.

Good signals:

- A trait/interface with a **provider** and a **consumer** that live in different
  layers, crates, or teams.
- A contract that is *conventionally* enforced — "the domain layer only talks to
  storage through `Repository`" — where nothing stops a violation but a reviewer.
- A surface where **size or shape is the property that matters**, not
  correctness: something meant to stay small, or stay bulk-oriented, or keep a
  naming convention.
- A seam an LLM reviewer would have to re-derive every time, because it spans
  files that the diff shows separately.

Weak signals — probably not worth nominating:

- A module with exactly one caller. There is no boundary, just a function.
- A type the compiler already guards end to end, where any breaking change is a
  build failure. Deterministic tooling already covers it.
- A whole package "because it's the public API". That is API discovery, not
  nomination, and it drags in per-language export semantics the design avoids.

## Steps

### 1. Find the two sides

Look for the interface, then ask who is on each side of it. If you cannot name a
provider and a consumer as distinct sets of files, stop — there is no boundary
yet. Search for the trait or interface declaration and its implementors:

```
# who declares it, who implements it, who calls it
grep -rn "trait Repository\|impl Repository" crates/
```

### 2. Find the anchor symbol

A boundary points at a **symbol**, not a directory-as-catch-all. Get the exact
file and symbol name — the anchor is what makes the nomination stable when files
move around it.

### 3. Confirm it resolves before writing any rules

This is the step people skip. A one-shot extract tells you what the language
server actually sees:

```
ha-ha extract crates/app 'src/storage/mod.rs#Repository' --pretty
```

Read the `members` array. Empty means the symbol didn't resolve — usually the
wrong path, or a language whose directory semantics you guessed wrong (see the
table in `ha-ha-config`). Too many members usually means you nominated a
container rather than the interface.

Only when the members list matches what you'd describe as "the interface" is the
nomination right.

### 4. Pick the sides

`provider` and `consumer` globs are only read when a rule expands to call sites,
so they can be approximate. Get them roughly right; they are not a security
boundary.

### 5. Write one rule, not five

Start with the property you actually care about, and put it on the right side of
the deterministic line:

- **Computable from the diff?** It belongs in `when`. "A member was removed" is
  `when="member.removed"` — no prose needed.
- **Not thresholdable?** That is the judge's job. "Is this interface getting too
  big" has no number, so give the model the growth signal and let it call it:
  `when="interface.grew"` with a prompt that asks the question.

The pattern is: **a deterministic signal decides whether to ask; the prompt makes
the call that isn't computable.** A prompt that restates a computable check is
wasted, and a `when` that tries to encode a judgment is a threshold you will
argue about forever.

### 6. Validate, then run

```
ha-ha validate ha-ha.kdl
ha-ha extract --config ha-ha.kdl --pretty
```

## Anti-patterns

| Don't | Why |
| --- | --- |
| Nominate a module because it's the easiest thing to point at | A boundary needs two sides. A module with one caller is just code. |
| Nominate everything on the first pass | Each boundary is a snapshot to review forever. Start with one you'd actually defend in code review. |
| Write the rules before confirming the target resolves | Rules against an empty member list look like they pass. Extract first, always. |
| Encode a judgment as a threshold (`max_members=8`) | You will spend more time arguing about the number than the design. Feed the signal to a judge instead. |
| Nominate a boundary the compiler already enforces | Deterministic tooling already caught it; a snapshot adds review burden with no new information. |

## Example

```
User: we keep accidentally adding one-off query methods to the storage layer.
      can ha-ha catch that?
You:  [find the two sides: crates/app/src/storage/** provides,
       crates/app/src/domain/** consumes — that's a real boundary]
      [anchor: src/storage/mod.rs, `trait Repository`]
      [ha-ha extract crates/app 'src/storage/mod.rs#Repository' --pretty
         → 6 members, all the query methods. right surface.]
      [the property is "stay small / prefer bulk", which has no threshold —
       so: when="interface.grew" plus a prompt that asks about reuse]
      Nominated `storage-domain`. The trigger is deterministic (the interface
      grew); the judgment — whether the new method should have reused
      `list_users` — is the prompt's. Validated and extracting 6 members.
```
