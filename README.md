# ha-ha

Name the interfaces the compiler can't see as one unit, track how they change,
and check the changes against deterministic and LLM-judged rules.

A **ha-ha** is a boundary that is real to the people working on the code but
invisible to the type system — "the domain layer only talks to storage through
`Repository`" — where nothing enforces the contract but a reviewer's memory.

## The tools

`ha-ha extract` turns a nomination into a snapshot: the interface's shape at one
revision, as git-committable JSON. Shape comes from a language server
(`documentSymbol` + `hover`), so the per-language surface is a table row rather
than a parser — verified across rust-analyzer, gopls, `tsc --lsp`, pyright, and
metals.

`ha-ha check` will diff two snapshots and run the nomination's rules against the
delta. Not built yet.

```
ha-ha extract  [options] <project> <nomination>
ha-ha extract  [options] --config <file>
ha-ha validate <file>
```

## Nominations

Boundaries live in a `ha-ha.kdl`, nested under the project whose language server
they share — one spawn per project, not per boundary. On metals that halves a
two-boundary run.

```kdl
project "rust" "crates/app" {
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

The core pattern is **a deterministic signal deciding whether to ask a
nondeterministic question.** "The interface grew" is computable; "this method
should have reused `list_users`" is not, so the growth signal triggers a judge
rather than a threshold nobody can agree on.

`ha-ha.schema.kdl` is the normative description of the format, written in
[KDL Schema](https://github.com/kdl-org/kdl/blob/main/SCHEMA-SPEC.md) and
embedded in the binary. `ha-ha validate` checks a config against it without
starting a language server, and reports every problem at once:

```
$ ha-ha validate ha-ha.kdl
ha-ha.kdl: ok — 2 projects, 3 boundaries, 4 rules
```

## Claude Code plugin

This repo is also a plugin marketplace. Two skills help write nomination files:

- **`ha-ha-config`** — the node vocabulary, what `target=` looks like per
  language, and the validate-then-extract loop.
- **`nominate-boundary`** — deciding which interface is worth naming, finding the
  anchor symbol, and splitting a rule across the deterministic line.

```
claude plugin marketplace add chrisdickinson/ha-ha
claude plugin install ha-ha@ha-ha
```

## Building

```
cargo build
cargo test
```
