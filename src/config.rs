//! `ha-ha.kdl` → [`Config`].
//!
//! One `project` node per extraction source, with boundaries nested beneath it
//! so a language server is spawned once and reused across all of them.
//!
//! The walk is deliberately strict: an unknown node, a missing argument, or an
//! unrecognized predicate is an error carrying a `file:line:col`. A config that
//! silently does nothing is the worst way for a boundary to stop being watched,
//! so a typo'd `boundry` fails loudly rather than dropping a rule on the floor.

use kdl::{KdlDocument, KdlError, KdlNode};
use std::path::Path;

use crate::Result;
use crate::lang;
use crate::model::{Config, Expand, Nomination, PREDICATES, Project, Rule, Sides, Source};
use crate::schema;

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // Roots and globs are written relative to the config file, not the cwd.
    let base = path.parent().filter(|p| !p.as_os_str().is_empty());
    parse(
        &text,
        &path.display().to_string(),
        base.unwrap_or(Path::new(".")),
    )
}

pub fn parse(text: &str, origin: &str, base: &Path) -> Result<Config> {
    let doc = KdlDocument::parse(text).map_err(|e| syntax(&e, origin, text))?;
    let cx = Ctx { origin, text, base };

    // Structure first, against the schema that documents the format — it
    // reports every problem at once, where the walk below stops at the first.
    // What the schema cannot express (unique ids, an adapter naming a server)
    // stays in the walk.
    let errors = schema::builtin()?.validate(&doc, &|node| {
        let (line, col) = line_col(text, node.span().offset());
        format!("{origin}:{line}:{col}")
    });
    if !errors.is_empty() {
        let mut out = format!(
            "{origin}: {} {} the schema",
            errors.len(),
            if errors.len() == 1 {
                "problem with"
            } else {
                "problems with"
            }
        );
        for error in &errors {
            out.push_str(&format!("\n  {error}"));
        }
        return Err(out.into());
    }

    let mut projects = Vec::new();
    let mut seen = Vec::new();
    for node in doc.nodes() {
        match node.name().value() {
            "project" => projects.push(project(node, &cx, &mut seen)?),
            other => {
                return Err(cx.err(
                    node,
                    format!("unknown node `{other}` at top level — expected `project`"),
                ));
            }
        }
    }
    Ok(Config { projects })
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// `project <adapter> <root> { … }`
fn project(node: &KdlNode, cx: &Ctx, seen: &mut Vec<String>) -> Result<Project> {
    let adapter = cx.arg(node, 0, "adapter name")?;
    let root = cx.base.join(cx.arg(node, 1, "root path")?);

    // A known language adapter drives a server; anything else is a native
    // adapter for an already-reified IDL, which `extract` reports as
    // unimplemented rather than guessing at.
    let source = if lang::by_name(&adapter).is_some() {
        Source::Lsp {
            server: adapter,
            root,
        }
    } else {
        Source::Native { adapter, root }
    };

    let mut rules = Vec::new();
    let mut boundaries = Vec::new();
    for child in node.iter_children() {
        match child.name().value() {
            // Rules here apply to every boundary in the project. They stay on
            // the project rather than being copied down, so the check tool can
            // tell a shared rule from a boundary's own.
            "rule" => rules.push(rule(child, cx)?),
            "boundary" => boundaries.push(boundary(child, cx, seen)?),
            other => {
                return Err(cx.err(
                    child,
                    format!("unknown node `{other}` in `project` — expected `boundary` or `rule`"),
                ));
            }
        }
    }
    Ok(Project {
        source,
        rules,
        boundaries,
    })
}

/// `boundary <id> target=<nomination> { … }`
fn boundary(node: &KdlNode, cx: &Ctx, seen: &mut Vec<String>) -> Result<Nomination> {
    let id = cx.arg(node, 0, "id")?;
    if seen.contains(&id) {
        return Err(cx.err(
            node,
            format!("duplicate boundary id `{id}` — ids are diff keys and must be unique"),
        ));
    }
    seen.push(id.clone());

    let target = cx.prop(node, "target")?.ok_or_else(|| {
        cx.err(
            node,
            format!("boundary `{id}` needs `target=\"<path>#<Symbol>\"` — a boundary must point at a symbol"),
        )
    })?;
    if target.trim().is_empty() {
        return Err(cx.err(node, format!("boundary `{id}` has an empty `target`")));
    }

    let mut description = String::new();
    let mut sides = Sides::default();
    let mut rules = Vec::new();
    for child in node.iter_children() {
        match child.name().value() {
            "description" => description = cx.arg(child, 0, "text")?,
            "provider" => sides.provider.extend(cx.args(child)?),
            "consumer" => sides.consumer.extend(cx.args(child)?),
            "rule" => rules.push(rule(child, cx)?),
            other => {
                return Err(cx.err(
                    child,
                    format!(
                        "unknown node `{other}` in `boundary` — expected \
                         `description`, `provider`, `consumer`, or `rule`"
                    ),
                ));
            }
        }
    }
    Ok(Nomination {
        id,
        description,
        target,
        sides,
        rules,
    })
}

/// `rule <id> when=<predicate> signals=<space-separated> { prompt "…"; expand … }`
fn rule(node: &KdlNode, cx: &Ctx) -> Result<Rule> {
    let id = cx.arg(node, 0, "id")?;

    // An unknown predicate would simply never fire. Catch it here, where we can
    // still say where it was written.
    let when = cx.prop(node, "when")?;
    if let Some(w) = &when
        && !PREDICATES.contains(&w.as_str())
    {
        return Err(cx.err(
            node,
            format!("unknown predicate `{w}` — known: {}", PREDICATES.join(", ")),
        ));
    }

    let signals = cx
        .prop(node, "signals")?
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let mut prompt = None;
    let mut expand = None;
    for child in node.iter_children() {
        match child.name().value() {
            "prompt" => prompt = Some(cx.arg(child, 0, "text")?),
            "expand" => expand = Some(expand_of(child, cx)?),
            other => {
                return Err(cx.err(
                    child,
                    format!("unknown node `{other}` in `rule` — expected `prompt` or `expand`"),
                ));
            }
        }
    }

    let prompt = prompt.ok_or_else(|| {
        cx.err(
            node,
            format!("rule `{id}` needs a `prompt` — a rule is a judgment in natural language"),
        )
    })?;
    Ok(Rule::Judge {
        id,
        when,
        signals,
        expand,
        prompt,
    })
}

/// `expand sides=<space-separated> each="references" context-lines=<n>`
fn expand_of(node: &KdlNode, cx: &Ctx) -> Result<Expand> {
    let defaults = Expand::default();

    let sides: Vec<String> = cx
        .prop(node, "sides")?
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if let Some(bad) = sides
        .iter()
        .find(|s| !matches!(s.as_str(), "provider" | "consumer"))
    {
        return Err(cx.err(
            node,
            format!("unknown side `{bad}` — expected `provider` or `consumer`"),
        ));
    }

    let each = cx.prop(node, "each")?.unwrap_or(defaults.each);
    if each != "references" {
        return Err(cx.err(
            node,
            format!("unknown expansion `{each}` — only `references` today"),
        ));
    }

    let context_lines = match node.get("context-lines") {
        None => defaults.context_lines,
        Some(v) => v
            .as_integer()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| cx.err(node, "`context-lines` must be a non-negative integer"))?,
    };

    Ok(Expand {
        sides,
        each,
        context_lines,
    })
}

// ---------------------------------------------------------------------------
// Reading entries, and saying where they went wrong
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    origin: &'a str,
    text: &'a str,
    base: &'a Path,
}

impl Ctx<'_> {
    fn err(&self, node: &KdlNode, msg: impl std::fmt::Display) -> Box<dyn std::error::Error> {
        let (line, col) = line_col(self.text, node.span().offset());
        format!("{}:{line}:{col}: {msg}", self.origin).into()
    }

    /// A required, non-empty positional string argument.
    fn arg(&self, node: &KdlNode, idx: usize, what: &str) -> Result<String> {
        let name = node.name().value();
        match node.get(idx).and_then(|v| v.as_string()) {
            Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
            Some(_) => Err(self.err(node, format!("`{name}` needs a non-empty {what}"))),
            None => Err(self.err(node, format!("`{name}` needs a {what}"))),
        }
    }

    /// An optional string property.
    fn prop(&self, node: &KdlNode, key: &str) -> Result<Option<String>> {
        match node.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_string()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| self.err(node, format!("`{key}` must be a string"))),
        }
    }

    /// Every positional string argument, for the repeatable list nodes.
    fn args(&self, node: &KdlNode) -> Result<Vec<String>> {
        let name = node.name().value();
        if node.entries().iter().all(|e| e.name().is_some()) {
            return Err(self.err(node, format!("`{name}` needs at least one value")));
        }
        node.entries()
            .iter()
            .filter(|e| e.name().is_none())
            .map(|e| {
                e.value()
                    .as_string()
                    .map(str::to_string)
                    .ok_or_else(|| self.err(node, format!("`{name}` takes string values")))
            })
            .collect()
    }
}

/// `KdlError`'s own `Display` is just "Failed to parse KDL document" — the
/// substance is in its diagnostics, so render those with locations.
fn syntax(err: &KdlError, origin: &str, text: &str) -> Box<dyn std::error::Error> {
    let mut out = format!("{origin}: invalid KDL");
    for d in &err.diagnostics {
        let (line, col) = line_col(text, d.span.offset());
        let msg = d.message.as_deref().unwrap_or("parse error");
        out.push_str(&format!("\n  {origin}:{line}:{col}: {msg}"));
        if let Some(help) = &d.help {
            out.push_str(&format!("\n    help: {help}"));
        }
    }
    out.into()
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let (mut line, mut col) = (1, 1);
    for ch in text.chars().take(offset) {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(src: &str) -> Result<Config> {
        parse(src, "ha-ha.kdl", Path::new("/repo"))
    }

    const EXAMPLE: &str = r#"
project "rust" "crates/app" {
  rule "shared" when="interface.changed" {
    prompt "Applies to every boundary in the project."
  }

  boundary "storage-domain" target="src/storage/mod.rs#Repository" {
    description "Repository trait consumed by the domain layer"

    provider "src/storage/**"
    consumer "src/domain/**" "src/api/**"

    rule "keep-it-small" when="interface.grew" signals="member.count member.added" {
      prompt """
        This boundary is meant to stay small. It grew.
        Is the addition earning its place?
        """
    }

    rule "usage-on-change" when="member.signature_changed" {
      expand sides="provider consumer" context-lines=4
      prompt "A signature changed."
    }
  }

  boundary "domain-tasks" target="src/domain/tasks"
}

project "openapi" "spec/openapi.json"
"#;

    #[test]
    fn parses_the_example() {
        let config = cfg(EXAMPLE).expect("parse");
        assert_eq!(config.projects.len(), 2);

        let rust = &config.projects[0];
        match &rust.source {
            Source::Lsp { server, root } => {
                assert_eq!(server, "rust");
                // Roots resolve against the config file, not the cwd.
                assert_eq!(root, Path::new("/repo/crates/app"));
            }
            other => panic!("expected an LSP source, got {other:?}"),
        }
        // A project-level rule stays on the project rather than being copied
        // into each boundary — the check tool needs to tell them apart.
        assert_eq!(rust.rules.len(), 1);
        assert_eq!(rust.boundaries.len(), 2);

        let storage = &rust.boundaries[0];
        assert_eq!(storage.target, "src/storage/mod.rs#Repository");
        assert_eq!(
            storage.description,
            "Repository trait consumed by the domain layer"
        );
        assert_eq!(storage.sides.provider, ["src/storage/**"]);
        assert_eq!(storage.sides.consumer, ["src/domain/**", "src/api/**"]);
        assert_eq!(storage.rules.len(), 2);

        // A boundary with no children is still a boundary.
        assert_eq!(rust.boundaries[1].target, "src/domain/tasks");
        assert!(rust.boundaries[1].rules.is_empty());

        // An unknown adapter is a native source, not an error: `extract`
        // reports it as unimplemented rather than the config refusing to load.
        assert!(
            matches!(&config.projects[1].source, Source::Native { adapter, .. } if adapter == "openapi")
        );
    }

    #[test]
    fn reads_a_rule() {
        let config = cfg(EXAMPLE).expect("parse");
        let rules = &config.projects[0].boundaries[0].rules;
        match &rules[0] {
            Rule::Judge {
                id,
                when,
                signals,
                expand,
                prompt,
            } => {
                assert_eq!(id, "keep-it-small");
                assert_eq!(when.as_deref(), Some("interface.grew"));
                assert_eq!(signals, &["member.count", "member.added"]);
                assert!(expand.is_none());
                // The multiline body dedents against the closing delimiter, so
                // a prompt can be indented to match the config around it.
                assert_eq!(
                    prompt,
                    "This boundary is meant to stay small. It grew.\nIs the addition earning its place?"
                );
            }
            other => panic!("expected a judge, got {other:?}"),
        }
        match &rules[1] {
            Rule::Judge {
                expand: Some(e), ..
            } => {
                assert_eq!(e.sides, ["provider", "consumer"]);
                assert_eq!(e.each, "references");
                assert_eq!(e.context_lines, 4);
            }
            other => panic!("expected an expanding judge, got {other:?}"),
        }
    }

    #[test]
    fn expand_defaults_when_unspecified() {
        let config = cfg(r#"
project "rust" "." {
  boundary "b" target="src/lib.rs" {
    rule "r" { expand sides="provider"; prompt "p" }
  }
}"#)
        .expect("parse");
        match &config.projects[0].boundaries[0].rules[0] {
            Rule::Judge {
                expand: Some(e), ..
            } => {
                assert_eq!(e.each, "references");
                assert_eq!(e.context_lines, 3);
            }
            other => panic!("expected an expanding judge, got {other:?}"),
        }
    }

    /// Every rejection below would otherwise be a boundary that silently stops
    /// being watched, so each one has to name where it went wrong.
    fn rejects(src: &str, expect: &str) {
        let err = cfg(src).expect_err("should be rejected").to_string();
        assert!(
            err.contains(expect),
            "expected an error mentioning {expect:?}, got: {err}"
        );
        assert!(err.contains("ha-ha.kdl:"), "error lacks a location: {err}");
    }

    #[test]
    fn rejects_a_boundary_without_a_target() {
        rejects(
            r#"project "rust" "." { boundary "b" { rule "r" { prompt "p" } } }"#,
            "missing the required property `target`",
        );
    }

    #[test]
    fn rejects_an_empty_target() {
        // "the whole surface" is API discovery, which the design avoids.
        rejects(
            r#"project "rust" "." { boundary "b" target="" }"#,
            "empty `target`",
        );
    }

    #[test]
    fn rejects_a_rule_without_a_prompt() {
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" } }"#,
            "`rule` needs a `prompt`",
        );
    }

    #[test]
    fn rejects_an_unknown_node() {
        rejects(
            r#"project "rust" "." { boundry "b" target="x" }"#,
            "unknown node `boundry`",
        );
        rejects(
            r#"boundary "b" target="x""#,
            "unknown node `boundary` in the document",
        );
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { sides "p" } }"#,
            "unknown node `sides`",
        );
    }

    /// The walk never noticed a misspelled property — it only ever asked for
    /// the ones it wanted. The schema closes that hole.
    #[test]
    fn rejects_an_unknown_property() {
        rejects(
            r#"project "rust" "." { boundary "b" targt="x" }"#,
            "unknown property `targt` on `boundary`",
        );
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" whn="always" { prompt "p" } } }"#,
            "unknown property `whn` on `rule`",
        );
    }

    #[test]
    fn rejects_a_second_prompt_or_description() {
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" { prompt "one"; prompt "two" } } }"#,
            "at most 1 `prompt`",
        );
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { description "one"; description "two" } }"#,
            "at most 1 `description`",
        );
    }

    #[test]
    fn rejects_a_mistyped_context_lines() {
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" { expand context-lines="four"; prompt "p" } } }"#,
            "must be an integer",
        );
    }

    #[test]
    fn rejects_an_unknown_expansion() {
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" { expand each="callers"; prompt "p" } } }"#,
            "must be one of `references`",
        );
    }

    /// Every problem at once. Fixing a config one error per run is miserable.
    #[test]
    fn reports_every_problem_at_once() {
        let err = cfg(r#"project "rust" "." { boundry "a" ; boundary "b" }"#)
            .expect_err("should be rejected")
            .to_string();
        assert!(err.contains("unknown node `boundry`"), "got: {err}");
        assert!(
            err.contains("missing the required property `target`"),
            "got: {err}"
        );
        assert!(err.starts_with("ha-ha.kdl: 2 problems"), "got: {err}");
    }

    #[test]
    fn rejects_an_unknown_predicate() {
        rejects(
            r#"project "rust" "." { boundary "b" target="x" { rule "r" when="interface.shrank" { prompt "p" } } }"#,
            "`when` on `rule` must be one of",
        );
    }

    #[test]
    fn rejects_a_duplicate_boundary_id() {
        // Ids are diff keys; a collision would silently merge two histories.
        rejects(
            r#"
project "rust" "a" { boundary "dup" target="x" }
project "go" "b"   { boundary "dup" target="y" }
"#,
            "duplicate boundary id `dup`",
        );
    }

    #[test]
    fn rejects_a_project_without_a_root() {
        rejects(r#"project "rust""#, "`project` needs 2 values");
    }

    #[test]
    fn reports_the_line_of_a_syntax_error() {
        // kdl's span granularity varies by error class — some point at a
        // character, some at the enclosing node — so we render whatever it
        // gives rather than inventing precision. This class points precisely.
        let err = cfg("project \"rust\" \".\" {\n  boundary 1.\n}\n")
            .expect_err("should be rejected")
            .to_string();
        assert!(err.contains("invalid KDL"), "got: {err}");
        assert!(
            err.contains("ha-ha.kdl:2:12:"),
            "error lacks a location: {err}"
        );
    }
}
