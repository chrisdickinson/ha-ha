//! Nomination → [`Snapshot`]: drive a language server and normalize what it
//! says into the skeleton the diff tool works on.
//!
//! The judgment calls all live here, and they are deliberately few:
//! `documentSymbol` gives structure, `hover` gives the declaration text, and
//! everything else is derived from those two without parsing types.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::lang::{self, Decl, Lang};
use crate::lsp::Client;
use crate::model::{
    Member, MemberKind, Provenance, SNAPSHOT_SCHEMA, Snapshot, SourceInfo, Visibility,
};
use crate::Result;

pub struct Options {
    pub project: PathBuf,
    pub nomination: String,
    pub server: Option<String>,
    pub public_only: bool,
    /// How many levels of nesting to include. `usize::MAX` for all.
    pub depth: usize,
    pub timeout: Duration,
}

// ---------------------------------------------------------------------------
// Nomination
// ---------------------------------------------------------------------------

/// `path` or `path#Symbol` or `path#Outer::Inner`.
#[derive(Debug, PartialEq)]
pub struct Nomination {
    pub path: String,
    pub symbol: Vec<String>,
}

pub fn parse_nomination(s: &str) -> Nomination {
    match s.split_once('#') {
        Some((path, symbol)) => Nomination {
            path: path.to_string(),
            symbol: symbol
                .split("::")
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        },
        None => Nomination {
            path: s.to_string(),
            symbol: Vec::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

pub fn run(opts: &Options) -> Result<Snapshot> {
    let project = std::fs::canonicalize(&opts.project)
        .map_err(|e| format!("bad project path {}: {e}", opts.project.display()))?;
    let nom = parse_nomination(&opts.nomination);

    let target_path = {
        let p = Path::new(&nom.path);
        if p.is_absolute() { p.to_path_buf() } else { project.join(p) }
    };

    let lang = pick_lang(opts, &project, &target_path)?;
    let binary = lang::resolve_server(lang, &project).ok_or_else(|| {
        format!(
            "`{}` not found on PATH — install it with: {}",
            lang.argv[0], lang.install
        )
    })?;
    let mut argv: Vec<String> = lang.argv.iter().map(|s| s.to_string()).collect();
    argv[0] = binary.to_string_lossy().into_owned();

    let files = lang::files_for(lang, &target_path)?;

    let mut client = Client::spawn(&argv, &project)?;
    client.initialize(&project, opts.timeout)?;
    client.wait_ready(opts.timeout);

    // Collect the symbol tree of every file in the surface (a Go package is
    // many files; a Rust module is one).
    let mut trees: Vec<(PathBuf, Vec<Sym>)> = Vec::new();
    let mut sources: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for file in &files {
        client.did_open(file, lang.language_id)?;
        let raw = client.document_symbol(file, opts.timeout)?;
        trees.push((file.clone(), parse_symbols(&raw)));
        let text = std::fs::read_to_string(file).unwrap_or_default();
        sources.insert(file.clone(), text.lines().map(str::to_string).collect());
    }

    // Pick the surface: a named target's children, or the files' top level.
    let mut surface: Vec<(PathBuf, String, Vec<Sym>)> = Vec::new();
    if nom.symbol.is_empty() {
        for (file, syms) in trees {
            surface.push((file, String::new(), syms));
        }
    } else {
        let mut found = false;
        // The target's surface is the union over everything matching the path:
        // in Rust a type's interface is its declaration *and* its impl blocks,
        // which documentSymbol reports as siblings.
        let parent = nom.symbol[..nom.symbol.len() - 1].join("::");
        for (file, syms) in &trees {
            let mut matches = Vec::new();
            find_matches(syms, &nom.symbol, &mut matches);
            if matches.is_empty() {
                continue;
            }
            found = true;
            let children: Vec<Sym> = matches.iter().flat_map(|m| m.children.clone()).collect();
            if children.is_empty() {
                // A leaf target is its own surface, qualified by its parent.
                let leaves: Vec<Sym> = matches
                    .iter()
                    .filter(|m| !is_transparent(m.kind))
                    .map(|m| (*m).clone())
                    .collect();
                surface.push((file.clone(), parent.clone(), leaves));
            } else {
                surface.push((file.clone(), nom.symbol.join("::"), children));
            }
        }
        if !found {
            return Err(format!(
                "symbol `{}` not found in {}",
                nom.symbol.join("::"),
                display_files(&files, &project)
            )
            .into());
        }
    }

    let mut members = Vec::new();
    for (file, prefix, syms) in &surface {
        let lines = sources.get(file).cloned().unwrap_or_default();
        collect(
            &mut client, lang, file, &project, &lines, syms, prefix, opts, 1, None, false,
            &mut members,
        )?;
    }

    members.sort_by(|a, b| a.id.cmp(&b.id));
    warn_on_duplicate_ids(&members);
    if opts.public_only {
        members.retain(|m| m.visibility != Some(Visibility::Private));
    }

    let metrics = metrics(&members);
    Ok(Snapshot {
        schema: SNAPSHOT_SCHEMA.to_string(),
        boundary: nom.path.clone(),
        target: opts.nomination.clone(),
        revision: revision(&project),
        generated_at: now_rfc3339(),
        source: SourceInfo {
            kind: "lsp".into(),
            server: lang.argv[0].to_string(),
        },
        members,
        metrics,
    })
}

fn pick_lang(opts: &Options, project: &Path, target: &Path) -> Result<&'static Lang> {
    if let Some(name) = &opts.server {
        return lang::by_name(name).ok_or_else(|| {
            let known: Vec<&str> = lang::LANGS.iter().map(|l| l.name).collect();
            format!("unknown server `{name}` — known: {}", known.join(", ")).into()
        });
    }
    lang::by_extension(target)
        .or_else(|| lang::by_root_markers(project))
        .ok_or_else(|| {
            format!(
                "cannot tell what language {} is — pass --server",
                target.display()
            )
            .into()
        })
}

/// Walk the symbol tree into members, one hover per emitted symbol.
#[allow(clippy::too_many_arguments)]
fn collect(
    client: &mut Client,
    lang: &Lang,
    file: &Path,
    project: &Path,
    lines: &[String],
    syms: &[Sym],
    prefix: &str,
    opts: &Options,
    depth: usize,
    parent_vis: Option<Visibility>,
    nested: bool,
    out: &mut Vec<Member>,
) -> Result<()> {
    for sym in syms {
        // Organizational containers (Rust `impl` blocks, TS namespaces) are not
        // interface members themselves — they only qualify what is inside them.
        if is_transparent(sym.kind) {
            let inner = join_id(prefix, last_ident(&sym.name));
            collect(
                client, lang, file, project, lines, &sym.children, &inner, opts, depth, parent_vis,
                nested, out,
            )?;
            continue;
        }

        let id = join_id(prefix, &sym.name);
        let hover = client
            .hover(file, sym.selection.0, sym.selection.1, opts.timeout)
            .ok()
            .and_then(|h| hover_text(&h));

        let source_line = lines
            .get(sym.selection.0 as usize)
            .map_or("", String::as_str);

        // Fallback chain. metals returns no hover at all for a `case class` and
        // an *empty string* (not an absent field) for documentSymbol detail, so
        // both need filtering, and the source line is the honest last resort —
        // still language-native text we don't parse.
        let declaration = hover
            .as_deref()
            .and_then(|text| declaration_of(text, &sym.name))
            .or_else(|| non_empty(sym.detail.clone().unwrap_or_default()))
            .or_else(|| non_empty(strip_annotation(source_line).trim().to_string()))
            .unwrap_or_default();

        let kind = member_kind(sym.kind);
        // A callable's children are its parameters and locals, never interface
        // surface — pyright reports them, rust-analyzer and gopls don't. Stop
        // at the signature regardless of server.
        let descend = depth < opts.depth
            && !sym.children.is_empty()
            && !matches!(kind, MemberKind::Function | MemberKind::Method);

        // Truncate to the opening line only when the members below really are
        // emitted separately; otherwise they'd be reported twice. If we are not
        // descending, this symbol's declaration is the whole story.
        let detail = if descend {
            declaration.lines().next().unwrap_or("").to_string()
        } else {
            declaration.clone()
        };

        // Enum variants have no visibility of their own in any language we
        // target — they are exactly as visible as the enum. Asking the
        // declaration would call every variant private.
        let visibility = if kind == MemberKind::Variant {
            parent_vis
        } else {
            let decl = Decl {
                name: &sym.name,
                declaration: &declaration,
                source_line,
                // Enclosure by a *member*, not by a package: `package storage`
                // is a transparent container, and being in it is not nesting.
                nested,
            };
            // No signal from the adapter means the container's answer is the
            // best one available — an interface's methods are as visible as it is.
            (lang.visibility)(&decl).or(parent_vis)
        };
        out.push(Member {
            id,
            kind,
            container: (!prefix.is_empty()).then(|| prefix.to_string()),
            arity: arity(&detail, &sym.name),
            detail_hash: fnv1a_hex(&normalize(&detail)),
            detail,
            detail_lang: lang.detail_lang.to_string(),
            docs: hover.as_deref().and_then(docs_of),
            visibility,
            provenance: Provenance {
                file: relative(file, project),
                range: sym.range,
            },
        });

        if descend {
            let inner = join_id(prefix, &sym.name);
            collect(
                client, lang, file, project, lines, &sym.children, &inner, opts, depth + 1,
                visibility, true, out,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// documentSymbol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Sym {
    pub name: String,
    pub kind: u8,
    pub detail: Option<String>,
    /// `selectionRange.start` — where to point `hover`.
    pub selection: (u32, u32),
    pub range: [u32; 4],
    pub children: Vec<Sym>,
}

/// Accepts hierarchical `DocumentSymbol[]`; falls back to flat
/// `SymbolInformation[]`, which nests one level via `containerName`.
pub fn parse_symbols(raw: &Value) -> Vec<Sym> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    let flat = items.first().is_some_and(|s| s.get("location").is_some());
    if !flat {
        return items.iter().map(parse_hierarchical).collect();
    }

    let mut roots: Vec<Sym> = Vec::new();
    let mut nested: Vec<(String, Sym)> = Vec::new();
    for item in items {
        let range = item.get("location").and_then(|l| l.get("range"));
        let sym = Sym {
            name: string_at(item, "name"),
            kind: item["kind"].as_u64().unwrap_or(0) as u8,
            detail: item.get("detail").and_then(Value::as_str).map(str::to_string),
            selection: range.map(range_start).unwrap_or((0, 0)),
            range: range.map(range_quad).unwrap_or([0; 4]),
            children: Vec::new(),
        };
        match item.get("containerName").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => nested.push((c.to_string(), sym)),
            _ => roots.push(sym),
        }
    }
    for (container, sym) in nested {
        match roots.iter_mut().find(|r| r.name == container) {
            Some(parent) => parent.children.push(sym),
            None => roots.push(sym),
        }
    }
    roots
}

fn parse_hierarchical(v: &Value) -> Sym {
    Sym {
        name: string_at(v, "name"),
        kind: v["kind"].as_u64().unwrap_or(0) as u8,
        detail: v.get("detail").and_then(Value::as_str).map(str::to_string),
        selection: v.get("selectionRange").map(range_start).unwrap_or((0, 0)),
        range: v.get("range").map(range_quad).unwrap_or([0; 4]),
        children: v
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.iter().map(parse_hierarchical).collect())
            .unwrap_or_default(),
    }
}

fn string_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn range_start(r: &Value) -> (u32, u32) {
    let p = &r["start"];
    (
        p["line"].as_u64().unwrap_or(0) as u32,
        p["character"].as_u64().unwrap_or(0) as u32,
    )
}

fn range_quad(r: &Value) -> [u32; 4] {
    let (sl, sc) = range_start(r);
    let e = &r["end"];
    [
        sl,
        sc,
        e["line"].as_u64().unwrap_or(0) as u32,
        e["character"].as_u64().unwrap_or(0) as u32,
    ]
}

/// Collect every symbol matching a `::` path.
///
/// Organizational containers match on their last identifier (`impl User` on
/// `User`) and are also transparent to a path that doesn't name them, so both
/// `#User::new` and `#new` can find a method inside an impl block.
fn find_matches<'a>(syms: &'a [Sym], path: &[String], out: &mut Vec<&'a Sym>) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    for sym in syms {
        let transparent = is_transparent(sym.kind);
        let name = if transparent { last_ident(&sym.name) } else { sym.name.as_str() };
        if name == head {
            if rest.is_empty() {
                out.push(sym);
            } else {
                find_matches(&sym.children, rest, out);
            }
        } else if transparent {
            find_matches(&sym.children, path, out);
        }
    }
}

// ---------------------------------------------------------------------------
// hover → declaration, docs
// ---------------------------------------------------------------------------

/// Flatten the several shapes `Hover.contents` is allowed to take.
pub fn hover_text(hover: &Value) -> Option<String> {
    let contents = hover.get("contents")?;
    let text = match contents {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value")?.as_str()?.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("value")?.as_str().map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Pull the symbol's own declaration out of hover markdown.
///
/// Servers put context ahead of the declaration — rust-analyzer emits the
/// containing path as its own fence, then `pub trait Repository` above the
/// method. We take the *last* fence and drop lines before the one naming the
/// symbol, so `detail_hash` tracks the member and not its container.
pub fn declaration_of(hover: &str, name: &str) -> Option<String> {
    let fence = code_fences(hover).pop()?;
    let start = fence.lines().position(|l| mentions(l, name)).unwrap_or(0);
    let decl: Vec<String> = fence.lines().skip(start).map(strip_annotation).collect();
    let decl = decl.join("\n").trim().to_string();
    (!decl.is_empty()).then_some(decl)
}

fn non_empty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

/// Drop a trailing `// size=…`-style comment. gopls annotates declarations with
/// struct layout; that is the server talking, not the source, and it would
/// otherwise churn `detail_hash` and mislead the model.
fn strip_annotation(line: &str) -> String {
    match line.find("//") {
        Some(at) => {
            let comment = line[at + 2..].trim_start();
            if HOVER_NOISE.iter().any(|n| comment.starts_with(n)) {
                line[..at].trim_end().to_string()
            } else {
                line.to_string()
            }
        }
        None => line.to_string(),
    }
}

fn code_fences(markdown: &str) -> Vec<String> {
    let mut fences = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            match current.take() {
                Some(body) => fences.push(body.join("\n")),
                None => current = Some(Vec::new()),
            }
        } else if let Some(body) = current.as_mut() {
            body.push(line);
        }
    }
    fences
}

/// Whole-word match, so `find_user` doesn't match inside `find_user_by_id`.
fn mentions(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(at) = line[from..].find(name) {
        let at = from + at;
        let before = line[..at].chars().next_back().is_some_and(is_word);
        let after = line[at + name.len()..].chars().next().is_some_and(is_word);
        if !before && !after {
            return true;
        }
        from = at + name.len();
    }
    false
}

/// Server metadata that rides alongside docs in hover output. Dropping these is
/// cosmetic — they are noise to a reader and to an LLM alike.
const HOVER_NOISE: &[&str] = &[
    "size = ",
    "size=",
    "Implements notable traits",
    "Is dyn-compatible",
    "Not dyn-compatible",
];

/// The prose part of hover: `---`-separated sections that aren't code or noise.
pub fn docs_of(hover: &str) -> Option<String> {
    let prose: Vec<&str> = hover
        .split("\n---\n")
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && !s.starts_with("```")
                && !HOVER_NOISE.iter().any(|n| s.starts_with(n))
        })
        .collect();
    (!prose.is_empty()).then(|| prose.join("\n\n"))
}

// ---------------------------------------------------------------------------
// Skeleton derivation
// ---------------------------------------------------------------------------

/// LSP `SymbolKind` → our normalized projection.
pub fn member_kind(kind: u8) -> MemberKind {
    match kind {
        5 | 23 => MemberKind::Record,      // Class, Struct
        6 | 9 | 25 => MemberKind::Method,  // Method, Constructor, Operator
        7 | 8 => MemberKind::Field,        // Property, Field
        10 => MemberKind::Enum,
        11 => MemberKind::Interface,
        12 => MemberKind::Function,
        13 | 14 => MemberKind::Const, // Variable, Constant
        22 => MemberKind::Variant,
        26 => MemberKind::TypeAlias, // rust-analyzer reports type aliases here
        _ => MemberKind::Other,
    }
}

/// Containers that structure code without being part of the interface:
/// Module, Namespace, Package, and Object (rust-analyzer's `impl` blocks).
fn is_transparent(kind: u8) -> bool {
    matches!(kind, 2 | 3 | 4 | 19)
}

/// `impl User` → `User`; `impl Repository for SqlRepo` → `SqlRepo`.
/// The qualifying name of an organizational container is its last identifier.
fn last_ident(name: &str) -> &str {
    name.split_whitespace().last().unwrap_or(name)
}

fn join_id(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

/// Parameter count: the balanced `(...)` group that immediately follows the
/// symbol's name, counting commas at nesting level zero.
///
/// Anchoring to the name rather than the first paren is what makes this work
/// across languages — Go writes the receiver first (`func (R) List()`), and a
/// declaration with no parameter list at all (`type User struct`) must report
/// nothing rather than grabbing an unrelated group.
pub fn arity(signature: &str, name: &str) -> Option<u32> {
    let after = after_name(signature, name)?;
    // Skip a generic parameter list: `fn foo<T>(x: T)`.
    let after = match after.strip_prefix('<') {
        Some(rest) => &rest[balanced_end(rest, '<', '>')? + 1..],
        None => after,
    };
    let inner = after.strip_prefix('(')?;
    let end = balanced_end(inner, '(', ')')?;
    let body = &inner[..end];
    if body.trim().is_empty() {
        return Some(0);
    }
    let mut depth = 0i32;
    let mut commas = 0u32;
    for c in body.chars() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    Some(commas + 1)
}

/// The slice just past a whole-word occurrence of `name`.
fn after_name<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let at = from + at;
        let end = at + name.len();
        let before = text[..at].chars().next_back().is_some_and(is_word);
        let after = text[end..].chars().next().is_some_and(is_word);
        if !before && !after {
            return Some(&text[end..]);
        }
        from = end;
    }
    None
}

/// Index of the closer matching an already-consumed opener. `None` if unbalanced.
fn balanced_end(rest: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// Collapse whitespace so reformatting alone doesn't churn the hash.
fn normalize(detail: &str) -> String {
    detail.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// FNV-1a 64. Hand-rolled because `DefaultHasher` is explicitly not stable
/// across Rust releases, and these hashes get committed to git.
pub fn fnv1a_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn metrics(members: &[Member]) -> BTreeMap<String, f64> {
    let arities: Vec<u32> = members.iter().filter_map(|m| m.arity).collect();
    let public = members
        .iter()
        .filter(|m| m.visibility == Some(Visibility::Public))
        .count();
    let mut m = BTreeMap::new();
    m.insert("member.count".into(), members.len() as f64);
    m.insert("member.public_count".into(), public as f64);
    m.insert(
        "arity.max".into(),
        arities.iter().copied().max().unwrap_or(0) as f64,
    );
    m.insert(
        "arity.mean".into(),
        if arities.is_empty() {
            0.0
        } else {
            (arities.iter().sum::<u32>() as f64 / arities.len() as f64 * 100.0).round() / 100.0
        },
    );
    m
}

/// Duplicate ids would silently corrupt a diff, so say something.
fn warn_on_duplicate_ids(members: &[Member]) {
    for pair in members.windows(2) {
        if pair[0].id == pair[1].id {
            eprintln!(
                "warning: duplicate member id `{}` ({} and {}) — keeping the first",
                pair[0].id, pair[0].provenance.file, pair[1].provenance.file
            );
        }
    }
}

// ---- provenance ----

fn relative(file: &Path, project: &Path) -> String {
    file.strip_prefix(project)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

fn display_files(files: &[PathBuf], project: &Path) -> String {
    files
        .iter()
        .map(|f| relative(f, project))
        .collect::<Vec<_>>()
        .join(", ")
}

fn revision(project: &Path) -> String {
    Command::new("git")
        .args(["-C", &project.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| format!("git:{}", String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| "unknown".into())
}

/// Shelling out beats a date dependency for one field.
fn now_rfc3339() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_path_from_symbol_path() {
        assert_eq!(
            parse_nomination("src/storage/mod.rs"),
            Nomination { path: "src/storage/mod.rs".into(), symbol: vec![] }
        );
        assert_eq!(
            parse_nomination("src/storage/mod.rs#Repository"),
            Nomination {
                path: "src/storage/mod.rs".into(),
                symbol: vec!["Repository".into()],
            }
        );
        assert_eq!(
            parse_nomination("src/s.rs#Outer::Inner"),
            Nomination {
                path: "src/s.rs".into(),
                symbol: vec!["Outer".into(), "Inner".into()],
            }
        );
        // A bare `#` means the file's own surface, same as omitting it.
        assert_eq!(parse_nomination("src/s.rs#").symbol, Vec::<String>::new());
    }

    #[test]
    fn declaration_drops_container_context_lines() {
        // The exact shape rust-analyzer returns for a trait method.
        let hover = "```rust\ndemo::storage::Repository\n```\n\n\
                     ```rust\npub trait Repository\npub fn find_user(&self, id: u64) -> Option<User>\n```\n\n\
                     ---\n\nFind a single user by id.";
        assert_eq!(
            declaration_of(hover, "find_user").unwrap(),
            "pub fn find_user(&self, id: u64) -> Option<User>"
        );
        // A struct's body belongs to the struct — keep it from its first line.
        let hover = "```rust\ndemo::storage\n```\n\n\
                     ```rust\npub struct User {\n    pub id: u64,\n}\n```";
        assert_eq!(
            declaration_of(hover, "User").unwrap(),
            "pub struct User {\n    pub id: u64,\n}"
        );
    }

    #[test]
    fn name_match_is_whole_word() {
        assert!(mentions("pub fn find_user(&self)", "find_user"));
        assert!(!mentions("pub fn find_user_by_id(&self)", "find_user"));
        assert!(mentions("pub struct User {", "User"));
    }

    #[test]
    fn docs_skip_server_layout_noise() {
        let hover = "```rust\npub struct User\n```\n\n\
                     ---\n\nsize = 32 (0x20), align = 0x8, needs Drop\n\n\
                     ---\n\nA user record.";
        assert_eq!(docs_of(hover).unwrap(), "A user record.");
        // Nothing but noise means no docs, not empty docs.
        let hover = "```rust\npub id: u64\n```\n\n---\n\nsize = 8, align = 0x8";
        assert_eq!(docs_of(hover), None);
    }

    #[test]
    fn arity_anchors_to_the_symbol_name() {
        assert_eq!(arity("fn f(a: Map<K, V>, b: u8)", "f"), Some(2));
        assert_eq!(arity("pub fn find_user(&self, id: u64) -> Option<User>", "find_user"), Some(2));
        assert_eq!(arity("fn f()", "f"), Some(0));
        assert_eq!(arity("fn f(a: (u8, u8))", "f"), Some(1));
        assert_eq!(arity("pub fn foo<T>(x: T, y: T)", "foo"), Some(2));

        // Go puts the receiver before the name; counting the first paren group
        // would report the receiver instead of the parameters.
        assert_eq!(arity("func (Repository) ListUsers() ([]User, error)", "ListUsers"), Some(0));
        assert_eq!(arity("func (Repository) FindUser(id uint64) (*User, error)", "FindUser"), Some(1));
        assert_eq!(arity("func OpenPool(url string, max int) string", "OpenPool"), Some(2));

        // No parameter list at all — including when an unrelated group follows.
        assert_eq!(arity("pub struct User", "User"), None);
        assert_eq!(arity("type Repository interface { (0x10) }", "Repository"), None);
        assert_eq!(arity("fn broken(a: u8", "broken"), None);
    }

    #[test]
    fn server_layout_annotations_are_stripped_from_declarations() {
        let hover = "```go\ntype User struct { // size=24 (0x18)\n```";
        assert_eq!(declaration_of(hover, "User").unwrap(), "type User struct {");
        // A comment that isn't a known annotation stays put.
        let hover = "```go\nfunc F(url string) // returns the url\n```";
        assert_eq!(
            declaration_of(hover, "F").unwrap(),
            "func F(url string) // returns the url"
        );
    }

    #[test]
    fn hash_is_pinned_and_ignores_reformatting() {
        // Pinned: a change here invalidates every committed snapshot.
        assert_eq!(fnv1a_hex(""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex("fn f()"), "8fbf7340ac8486b0");
        assert_eq!(
            fnv1a_hex("pub fn find_user(&self, id: u64) -> Option<User>"),
            "07c43df389bff027"
        );
        assert_eq!(normalize("fn  f(\n  a: u8,\n)"), "fn f( a: u8, )");
    }

    #[test]
    fn kind_map_covers_the_kinds_rust_analyzer_emits() {
        assert_eq!(member_kind(23), MemberKind::Record);
        assert_eq!(member_kind(11), MemberKind::Interface);
        assert_eq!(member_kind(6), MemberKind::Method);
        assert_eq!(member_kind(12), MemberKind::Function);
        assert_eq!(member_kind(8), MemberKind::Field);
        assert_eq!(member_kind(10), MemberKind::Enum);
        assert_eq!(member_kind(22), MemberKind::Variant);
        assert_eq!(member_kind(26), MemberKind::TypeAlias);
        assert_eq!(member_kind(19), MemberKind::Other); // impl block
    }

    #[test]
    fn impl_blocks_qualify_by_their_last_identifier() {
        assert_eq!(last_ident("impl User"), "User");
        assert_eq!(last_ident("impl Repository for SqlRepo"), "SqlRepo");
        assert_eq!(join_id("User", "new"), "User::new");
        assert_eq!(join_id("", "new"), "new");
    }

    #[test]
    fn falls_back_to_flat_symbol_information() {
        let raw = json!([
            { "name": "User", "kind": 23,
              "location": { "range": { "start": {"line": 1, "character": 0},
                                       "end": {"line": 3, "character": 1} } } },
            { "name": "id", "kind": 8, "containerName": "User",
              "location": { "range": { "start": {"line": 2, "character": 4},
                                       "end": {"line": 2, "character": 12} } } },
        ]);
        let syms = parse_symbols(&raw);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "User");
        assert_eq!(syms[0].children[0].name, "id");
        assert_eq!(syms[0].children[0].selection, (2, 4));
    }

    #[test]
    fn empty_strings_do_not_count_as_a_declaration() {
        // metals sends `detail: ""` rather than omitting the field.
        assert_eq!(non_empty(String::new()), None);
        assert_eq!(non_empty("   ".into()), None);
        assert_eq!(non_empty("x".into()), Some("x".into()));
    }

    #[test]
    fn hover_contents_shapes_all_flatten() {
        assert_eq!(hover_text(&json!({"contents": "x"})).unwrap(), "x");
        assert_eq!(hover_text(&json!({"contents": {"value": "x"}})).unwrap(), "x");
        assert_eq!(
            hover_text(&json!({"contents": [{"value": "a"}, "b"]})).unwrap(),
            "a\nb"
        );
        assert_eq!(hover_text(&json!({"contents": "  "})), None);
    }
}
