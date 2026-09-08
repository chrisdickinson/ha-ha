---
name: ha-ha-config
description: Use when writing or editing a `ha-ha.kdl` boundary nomination file — adding a boundary, adding or changing a rule, fixing a validation error, or working out what `target=` should be for a given language. Covers the node vocabulary, the embedded KDL Schema, and the validate-then-extract loop.
---

# Writing a `ha-ha.kdl`

A `ha-ha.kdl` names the interfaces the compiler can't see as one unit, and the
rules that watch them change. This skill is about getting the *file* right. For
deciding which interface is worth naming in the first place, use
`nominate-boundary`.

## The shape

Three levels, and the nesting is load-bearing:

```kdl
project "rust" "crates/app" {          // one language server, spawned once
  boundary "storage-domain" target="src/storage/mod.rs#Repository" {
    description "Repository trait consumed by the domain layer"

    provider "src/storage/**"
    consumer "src/domain/**"

    rule "keep-it-small" when="interface.grew" signals="member.count member.added" {
      prompt """
        This boundary is meant to stay small. It grew. Is the addition earning
        its place, or is there an existing member a caller could reuse?
        """
    }
  }
}
```

**Boundaries nest under a project because the language server is per-project.**
One `project` node means one server spawn, reused across every boundary beneath
it. For metals that is the difference between a five-minute run and a ten-minute
one. Splitting boundaries that share a root into separate `project` nodes is the
single most expensive mistake you can make in this file.

## Node vocabulary

| Node | Where | Form |
| --- | --- | --- |
| `project` | top level | `project <adapter> <root>` — adapter is `rust`, `typescript`, `go`, `python`, `scala`, or a native adapter name. `<root>` resolves relative to the config file. |
| `boundary` | in `project` | `boundary <id> target=<nomination>` — `<id>` is the diff key: stable, and unique across the whole file. |
| `description` | in `boundary` | `description <text>` — at most one. |
| `provider` / `consumer` | in `boundary` | `provider <glob> <glob>…` — repeatable. Only read when a rule expands to call sites. |
| `rule` | in `boundary` or `project` | `rule <id> when=<predicate> signals=<space-separated>` — needs a `prompt` child. A rule on the `project` applies to every boundary in it. |
| `prompt` | in `rule` | `prompt <text>` — required, exactly one. `"""` bodies dedent against the closing delimiter. |
| `expand` | in `rule` | `expand sides="provider consumer" each="references" context-lines=4` — at most one. |

`when` is a closed vocabulary: `member.added`, `member.removed`,
`member.signature_changed`, `interface.grew`, `interface.changed`, `always`.
Omitting `when` means the rule runs on any change.

The normative description is `ha-ha.schema.kdl`, a
[KDL Schema](https://github.com/kdl-org/kdl/blob/main/SCHEMA-SPEC.md) document
that `ha-ha` embeds and checks every config against. When the table above and the
schema disagree, the schema is right.

## Writing `target=`

This is where nominations actually go wrong. A target is `<path>`,
`<path>#<Symbol>`, or `<path>#<Outer>::<Inner>`, relative to the project root.
The path may be a file or a directory, and **what a directory means differs by
language**:

| Language | A directory resolves to | Example target |
| --- | --- | --- |
| rust | `mod.rs`, else `lib.rs`, else `main.rs` | `src/storage/mod.rs#Repository` |
| go | every non-`_test.go` file in it | `storage#Repository` |
| typescript | `index.ts` (or `.tsx`/`.js`/…) | `src/storage#Repository` |
| python | `__init__.py` | `storage#Repository` |
| scala | every `.scala` file in it | `src/main/scala/storage#storage::Repository` |

Two traps:

- **Scala ids carry the package prefix.** metals emits a package symbol where
  gopls does not, so the symbol is `storage::Repository`, not `Repository`. This
  is consistent within the language, which is what a diff key needs.
- **A TypeScript symbol exported by a separate `export { x }` statement reads as
  private.** Nominate the declaration, not the re-export.

## The loop

Validate first — it costs nothing and starts no server:

```
ha-ha validate ha-ha.kdl
```

It reports *every* problem at once with `file:line:col`, so fix the whole list
in one pass. A clean run prints a count:

```
ha-ha.kdl: ok — 2 projects, 3 boundaries, 4 rules
```

Then confirm the target actually resolves to the symbols you meant. A one-shot
extract is the cheapest way to check a single nomination:

```
ha-ha extract crates/app 'src/storage/mod.rs#Repository' --pretty
```

Read the `members` array. If it is empty, or full of things you didn't mean, the
target is wrong — not the rule. Only once targets resolve should you run the
whole file:

```
ha-ha extract --config ha-ha.kdl --pretty
```

## Anti-patterns

| Don't | Why |
| --- | --- |
| Give two boundaries in the same repo their own `project` node | You pay the server startup twice for nothing. One project per root. |
| Reuse a boundary `id` | Ids are diff keys. A collision silently merges two histories, so `validate` rejects it. |
| Write `target=""` or point a boundary at a whole package "to catch everything" | That is API *discovery*, which re-introduces per-language export semantics. A boundary points at a symbol. |
| Write a `prompt` that restates a deterministic check | If it is computable from the diff, it belongs in `when`, not in prose. See `nominate-boundary` for the split. |
| Skip `validate` and go straight to `extract` | On a slow server you wait minutes to learn you typed `boundry`. |

## Example

```
User: add the task queue to ha-ha.kdl
You:  [read ha-ha.kdl — there is already a `project "rust" "crates/app"`]
      [find the anchor: grep for the trait — crates/app/src/queue/mod.rs, `trait TaskQueue`]
      [add a boundary under the EXISTING rust project, not a new one]
      [ha-ha validate ha-ha.kdl → ok — 1 project, 3 boundaries, 5 rules]
      [ha-ha extract crates/app 'src/queue/mod.rs#TaskQueue' → 6 members, looks right]
      Added `task-queue` under the existing rust project so it shares the
      rust-analyzer session. It resolves to 6 members.
```
