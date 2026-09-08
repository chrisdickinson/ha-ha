//! Validating a `ha-ha.kdl` against a KDL Schema document.
//!
//! <https://github.com/kdl-org/kdl/blob/main/SCHEMA-SPEC.md>
//!
//! The schema in `ha-ha.schema.kdl` is the normative description of the
//! nomination format, and it is embedded here so a config is checked against
//! the same text that documents it.
//!
//! This implements the part of KDL Schema that `ha-ha.schema.kdl` uses — node
//! vocabulary, required properties, value counts and types, and closed `enum`
//! vocabularies — and **refuses to compile a schema using anything else**. A
//! validator that quietly skipped a constraint it didn't understand would let
//! the schema promise a check nobody performs, which is worse than not having
//! the schema at all.

use kdl::{KdlDocument, KdlNode, KdlValue};
use std::collections::BTreeMap;

use crate::Result;

/// The schema shipped with the tool.
pub const BUILTIN: &str = include_str!("../ha-ha.schema.kdl");

pub fn builtin() -> Result<Schema> {
    Schema::compile(BUILTIN).map_err(|e| format!("ha-ha.schema.kdl is not usable: {e}").into())
}

// ---------------------------------------------------------------------------
// The compiled schema
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Schema {
    root: Children,
}

#[derive(Debug, Default)]
struct Children {
    nodes: Vec<NodeDef>,
    other_allowed: bool,
}

#[derive(Debug)]
struct NodeDef {
    name: String,
    min: Option<usize>,
    max: Option<usize>,
    values: ValueDef,
    props: Vec<PropDef>,
    other_props_allowed: bool,
    children: Children,
}

#[derive(Debug, Default)]
struct ValueDef {
    ty: Option<Ty>,
    min: Option<usize>,
    max: Option<usize>,
    allowed: Option<Vec<String>>,
}

#[derive(Debug)]
struct PropDef {
    key: String,
    ty: Option<Ty>,
    required: bool,
    allowed: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    String,
    Integer,
    Boolean,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        match s {
            "string" => Some(Ty::String),
            "integer" => Some(Ty::Integer),
            "boolean" => Some(Ty::Boolean),
            _ => None,
        }
    }

    fn matches(self, v: &KdlValue) -> bool {
        match self {
            Ty::String => v.as_string().is_some(),
            Ty::Integer => v.as_integer().is_some(),
            Ty::Boolean => v.as_bool().is_some(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Ty::String => "a string",
            Ty::Integer => "an integer",
            Ty::Boolean => "a boolean",
        }
    }
}

// ---------------------------------------------------------------------------
// Compiling
// ---------------------------------------------------------------------------

impl Schema {
    pub fn compile(text: &str) -> Result<Schema> {
        let doc = KdlDocument::parse(text).map_err(|e| format!("{e:?}"))?;

        let document = doc
            .nodes()
            .iter()
            .find(|n| n.name().value() == "document")
            .ok_or("a schema needs exactly one top-level `document` node")?;
        let body = document.children().ok_or("`document` has no body")?;

        // `definitions` is resolved by `ref=[id="…"]`, so index it first.
        let mut defs: BTreeMap<String, &KdlNode> = BTreeMap::new();
        for node in body.nodes() {
            if node.name().value() == "definitions" {
                for def in node.iter_children() {
                    if let Some(id) = string_prop(def, "id") {
                        defs.insert(id, def);
                    }
                }
            }
        }

        let mut root = Children::default();
        for node in body.nodes() {
            match node.name().value() {
                // Metadata about the schema, not a constraint on the document.
                "info" | "definitions" => {}
                "other-nodes-allowed" => root.other_allowed = bool_arg(node)?,
                "node" => root.nodes.push(compile_node(node, &defs, 0)?),
                other => return Err(unsupported(other, "document").into()),
            }
        }
        Ok(Schema { root })
    }
}

fn compile_node(
    node: &KdlNode,
    defs: &BTreeMap<String, &KdlNode>,
    depth: usize,
) -> Result<NodeDef> {
    if depth > 16 {
        return Err("schema `ref` chain is too deep — is there a cycle?".into());
    }
    // A `ref` stands in for a definition; anything on the referring node wins.
    let resolved = match node.get("ref") {
        None => None,
        Some(v) => {
            let query = v.as_string().ok_or("`ref` must be a string")?;
            let id = ref_id(query)?;
            Some(
                *defs
                    .get(&id)
                    .ok_or_else(|| format!("`ref` points at unknown definition `{id}`"))?,
            )
        }
    };
    if let Some(target) = resolved {
        return compile_node(target, defs, depth + 1);
    }

    let name = node
        .get(0usize)
        .and_then(|v| v.as_string())
        .ok_or("a `node` in this schema needs a name")?
        .to_string();

    let mut def = NodeDef {
        name,
        min: None,
        max: None,
        values: ValueDef::default(),
        props: Vec::new(),
        other_props_allowed: false,
        children: Children::default(),
    };

    for child in node.iter_children() {
        match child.name().value() {
            "description" => {}
            "min" => def.min = Some(usize_arg(child)?),
            "max" => def.max = Some(usize_arg(child)?),
            "other-props-allowed" => def.other_props_allowed = bool_arg(child)?,
            "value" => def.values = compile_value(child)?,
            "prop" => def.props.push(compile_prop(child)?),
            "children" => {
                for grandchild in child.iter_children() {
                    match grandchild.name().value() {
                        "other-nodes-allowed" => def.children.other_allowed = bool_arg(grandchild)?,
                        "node" => {
                            def.children
                                .nodes
                                .push(compile_node(grandchild, defs, depth + 1)?)
                        }
                        other => return Err(unsupported(other, "children").into()),
                    }
                }
            }
            other => return Err(unsupported(other, "node").into()),
        }
    }
    Ok(def)
}

fn compile_value(node: &KdlNode) -> Result<ValueDef> {
    let mut def = ValueDef::default();
    for child in node.iter_children() {
        match child.name().value() {
            "description" => {}
            "min" => def.min = Some(usize_arg(child)?),
            "max" => def.max = Some(usize_arg(child)?),
            "type" => def.ty = Some(ty_arg(child)?),
            "enum" => def.allowed = Some(string_args(child)?),
            other => return Err(unsupported(other, "value").into()),
        }
    }
    Ok(def)
}

fn compile_prop(node: &KdlNode) -> Result<PropDef> {
    let key = node
        .get(0usize)
        .and_then(|v| v.as_string())
        .ok_or("a `prop` in this schema needs a key")?
        .to_string();
    let mut def = PropDef {
        key,
        ty: None,
        required: false,
        allowed: None,
    };
    for child in node.iter_children() {
        match child.name().value() {
            "description" => {}
            "required" => def.required = bool_arg(child)?,
            "type" => def.ty = Some(ty_arg(child)?),
            "enum" => def.allowed = Some(string_args(child)?),
            other => return Err(unsupported(other, "prop").into()),
        }
    }
    Ok(def)
}

/// Everything in KDL Schema that this validator does not enforce. Naming them
/// individually beats a generic "unknown node": the schema author learns that
/// the constraint would be ignored, rather than that they misspelled something.
fn unsupported(node: &str, within: &str) -> String {
    const KNOWN_BUT_UNENFORCED: &[&str] = &[
        "pattern",
        "format",
        "min-length",
        "max-length",
        "node-names",
        "prop-names",
        "tag",
        "tag-names",
        "other-tags-allowed",
        "%",
        ">",
        ">=",
        "<",
        "<=",
    ];
    if KNOWN_BUT_UNENFORCED.contains(&node) {
        format!(
            "schema uses `{node}` in `{within}`, which ha-ha does not enforce — \
             remove it rather than let the schema promise a check nobody runs"
        )
    } else {
        format!("unknown schema node `{node}` in `{within}`")
    }
}

/// Only `[id="…"]`, the one KDL Query shape the schema needs.
fn ref_id(query: &str) -> Result<String> {
    let inner = query
        .strip_prefix("[id=\"")
        .and_then(|q| q.strip_suffix("\"]"))
        .ok_or_else(|| {
            format!("ha-ha understands only `ref=[id=\"…\"]`, not the query `{query}`")
        })?;
    Ok(inner.to_string())
}

fn bool_arg(node: &KdlNode) -> Result<bool> {
    node.get(0usize)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("`{}` needs a boolean", node.name().value()).into())
}

fn usize_arg(node: &KdlNode) -> Result<usize> {
    node.get(0usize)
        .and_then(|v| v.as_integer())
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| format!("`{}` needs a non-negative integer", node.name().value()).into())
}

fn ty_arg(node: &KdlNode) -> Result<Ty> {
    let raw = node.get(0usize).and_then(|v| v.as_string()).unwrap_or("");
    Ty::parse(raw).ok_or_else(|| {
        format!("ha-ha understands the types string, integer, and boolean — not `{raw}`").into()
    })
}

fn string_args(node: &KdlNode) -> Result<Vec<String>> {
    node.entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(|e| {
            e.value()
                .as_string()
                .map(str::to_string)
                .ok_or_else(|| format!("`{}` takes string values", node.name().value()).into())
        })
        .collect()
}

fn string_prop(node: &KdlNode, key: &str) -> Option<String> {
    node.get(key)
        .and_then(|v| v.as_string())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Validating
// ---------------------------------------------------------------------------

impl Schema {
    /// Check a document, reporting *every* problem rather than the first.
    ///
    /// Someone fixing a config wants the whole list, not one error per run.
    pub fn validate(&self, doc: &KdlDocument, at: &dyn Fn(&KdlNode) -> String) -> Vec<String> {
        let mut errors = Vec::new();
        check_children(&self.root, doc.nodes(), "the document", "", at, &mut errors);
        errors
    }
}

fn check_children(
    schema: &Children,
    nodes: &[KdlNode],
    within: &str,
    // Location of the node holding these children, so "needs a `prompt`" can
    // point at the rule that is missing one.
    parent: &str,
    at: &dyn Fn(&KdlNode) -> String,
    errors: &mut Vec<String>,
) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();

    for node in nodes {
        let name = node.name().value();
        let Some(def) = schema.nodes.iter().find(|d| d.name == name) else {
            if !schema.other_allowed {
                let mut known: Vec<&str> = schema.nodes.iter().map(|d| d.name.as_str()).collect();
                known.sort_unstable();
                errors.push(format!(
                    "{}: unknown node `{name}` in {within} — expected {}",
                    at(node),
                    or_list(&known)
                ));
            }
            continue;
        };
        *counts.entry(def.name.as_str()).or_default() += 1;
        check_node(def, node, at, errors);
    }

    for def in &schema.nodes {
        let seen = counts.get(def.name.as_str()).copied().unwrap_or(0);
        if let Some(min) = def.min
            && seen < min
        {
            errors.push(format!(
                "{parent}{within} needs {} `{}`{}, and has {seen}",
                if min == 1 {
                    "a".into()
                } else {
                    min.to_string()
                },
                def.name,
                if min == 1 { "" } else { " of them" }
            ));
        }
        if let Some(max) = def.max
            && seen > max
        {
            // Point at the surplus, not the first legitimate one.
            let surplus = nodes
                .iter()
                .filter(|n| n.name().value() == def.name)
                .nth(max);
            let where_ = surplus
                .map(|n| format!("{}: ", at(n)))
                .unwrap_or_else(|| parent.to_string());
            errors.push(format!(
                "{where_}{within} allows at most {max} `{}`, and has {seen}",
                def.name
            ));
        }
    }
}

fn check_node(
    def: &NodeDef,
    node: &KdlNode,
    at: &dyn Fn(&KdlNode) -> String,
    errors: &mut Vec<String>,
) {
    let args: Vec<&KdlValue> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .map(|e| e.value())
        .collect();

    if let Some(min) = def.values.min
        && args.len() < min
    {
        errors.push(format!(
            "{}: `{}` needs {min} value{}, and has {}",
            at(node),
            def.name,
            plural(min),
            args.len()
        ));
    }
    if let Some(max) = def.values.max
        && args.len() > max
    {
        errors.push(format!(
            "{}: `{}` takes at most {max} value{}, and has {}",
            at(node),
            def.name,
            plural(max),
            args.len()
        ));
    }
    for arg in &args {
        if let Some(ty) = def.values.ty
            && !ty.matches(arg)
        {
            errors.push(format!(
                "{}: `{}` takes {} — got {}",
                at(node),
                def.name,
                ty.name(),
                show(arg)
            ));
        }
        if let Some(allowed) = &def.values.allowed
            && let Some(s) = arg.as_string()
            && !allowed.iter().any(|a| a == s)
        {
            errors.push(format!(
                "{}: `{}` does not accept `{s}` — expected {}",
                at(node),
                def.name,
                or_list_owned(allowed)
            ));
        }
    }

    for prop in &def.props {
        match node.get(prop.key.as_str()) {
            None => {
                if prop.required {
                    errors.push(format!(
                        "{}: `{}` is missing the required property `{}`",
                        at(node),
                        def.name,
                        prop.key
                    ));
                }
            }
            Some(value) => {
                if let Some(ty) = prop.ty
                    && !ty.matches(value)
                {
                    errors.push(format!(
                        "{}: `{}` on `{}` must be {} — got {}",
                        at(node),
                        prop.key,
                        def.name,
                        ty.name(),
                        show(value)
                    ));
                }
                if let Some(allowed) = &prop.allowed
                    && let Some(s) = value.as_string()
                    && !allowed.iter().any(|a| a == s)
                {
                    errors.push(format!(
                        "{}: `{}` on `{}` must be one of {} — got `{s}`",
                        at(node),
                        prop.key,
                        def.name,
                        or_list_owned(allowed)
                    ));
                }
            }
        }
    }

    if !def.other_props_allowed {
        for entry in node.entries() {
            let Some(key) = entry.name() else { continue };
            if !def.props.iter().any(|p| p.key == key.value()) {
                let mut known: Vec<&str> = def.props.iter().map(|p| p.key.as_str()).collect();
                known.sort_unstable();
                let expected = if known.is_empty() {
                    "no properties at all".to_string()
                } else {
                    or_list(&known)
                };
                errors.push(format!(
                    "{}: unknown property `{}` on `{}` — expected {expected}",
                    at(node),
                    key.value(),
                    def.name
                ));
            }
        }
    }

    let within = format!("`{}`", def.name);
    let empty: &[KdlNode] = &[];
    let children = node.children().map(|d| d.nodes()).unwrap_or(empty);
    check_children(
        &def.children,
        children,
        &within,
        &format!("{}: ", at(node)),
        at,
        errors,
    );
}

/// Render a value the way it was written, so a string reads as a string.
fn show(v: &KdlValue) -> String {
    match v.as_string() {
        Some(s) => format!("\"{s}\""),
        None => v.to_string(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn or_list(items: &[&str]) -> String {
    match items {
        [] => "nothing".to_string(),
        [one] => format!("`{one}`"),
        [head @ .., last] => format!(
            "{} or `{last}`",
            head.iter()
                .map(|i| format!("`{i}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn or_list_owned(items: &[String]) -> String {
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    or_list(&refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PREDICATES;

    /// `BUILTIN` is embedded at compile time but compiled at run time, so
    /// without this test a broken schema ships and fails on the user's config.
    #[test]
    fn the_builtin_schema_compiles() {
        let schema = builtin().expect("ha-ha.schema.kdl should compile");
        let names: Vec<&str> = schema.root.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["project"]);
        assert!(!schema.root.other_allowed, "the top level should be closed");
    }

    /// The schema is the normative description of the format, so its `when`
    /// vocabulary and the one the evaluator will read must be the same list.
    /// This is the one place they could drift.
    #[test]
    fn the_when_vocabulary_matches_the_model() {
        let doc = KdlDocument::parse(BUILTIN).expect("parse");
        let document = doc.get("document").expect("document node");
        let definitions = document
            .children()
            .and_then(|d| d.get("definitions"))
            .expect("definitions");
        let rule = definitions
            .iter_children()
            .find(|n| string_prop(n, "id").as_deref() == Some("rule"))
            .expect("rule definition");
        let when = rule
            .iter_children()
            .find(|n| {
                n.name().value() == "prop"
                    && n.get(0usize).and_then(|v| v.as_string()) == Some("when")
            })
            .expect("when prop");
        let values = when
            .iter_children()
            .find(|n| n.name().value() == "enum")
            .map(|n| string_args(n).expect("enum values"))
            .expect("enum");
        assert_eq!(
            values, PREDICATES,
            "schema `when` enum drifted from PREDICATES"
        );
    }

    /// A validator that ignored a constraint it did not implement would let the
    /// schema document a check nobody runs. Refuse the schema instead.
    #[test]
    fn refuses_a_schema_it_cannot_enforce() {
        let err = Schema::compile(
            r##"document { node thing { prop name { type string; pattern #"\w+"# } } }"##,
        )
        .expect_err("should refuse")
        .to_string();
        assert!(err.contains("`pattern`"), "got: {err}");
        assert!(err.contains("does not enforce"), "got: {err}");
    }

    #[test]
    fn refuses_an_unresolvable_ref() {
        let err = Schema::compile(r##"document { node ref=#"[id="nope"]"# }"##)
            .expect_err("should refuse")
            .to_string();
        assert!(err.contains("unknown definition `nope`"), "got: {err}");

        let err = Schema::compile(r#"document { node ref="top() > thing" }"#)
            .expect_err("should refuse")
            .to_string();
        assert!(err.contains("only `ref=[id="), "got: {err}");
    }

    fn errors_for(schema: &str, doc: &str) -> Vec<String> {
        let schema = Schema::compile(schema).expect("schema compiles");
        let doc = KdlDocument::parse(doc).expect("document parses");
        schema.validate(&doc, &|_| "at".to_string())
    }

    #[test]
    fn a_ref_carries_the_definitions_rules() {
        // `rule` is defined once and referenced from two places; the reference
        // has to bring the whole definition with it, prompt requirement and all.
        let errors = errors_for(
            r##"document {
                 node holder { children { node ref=#"[id="r"]"# } }
                 definitions { node r id=r { children { node prompt { min 1 } } } }
               }"##,
            "holder { r { } }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("`r` needs a `prompt`"), "{errors:?}");
    }

    #[test]
    fn counts_values_and_checks_their_type() {
        let schema = r#"document { node n { value { type integer; min 1; max 2 } } }"#;
        assert!(errors_for(schema, "n 1 2").is_empty());
        assert!(
            errors_for(schema, "n")
                .iter()
                .any(|e| e.contains("needs 1 value"))
        );
        assert!(
            errors_for(schema, "n 1 2 3")
                .iter()
                .any(|e| e.contains("at most 2 values"))
        );
        assert!(
            errors_for(schema, r#"n "x""#)
                .iter()
                .any(|e| e.contains("takes an integer"))
        );
    }

    #[test]
    fn open_nodes_permit_what_is_not_listed() {
        let closed = r#"document { node n { other-props-allowed #false } }"#;
        let open = r#"document { node n { other-props-allowed #true } }"#;
        assert!(!errors_for(closed, "n extra=1").is_empty());
        assert!(errors_for(open, "n extra=1").is_empty());

        let open_children = r#"document { node n { children { other-nodes-allowed #true } } }"#;
        assert!(errors_for(open_children, "n { whatever }").is_empty());
    }
}
