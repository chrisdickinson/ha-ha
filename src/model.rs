//! ha-ha data model: the contract between the two tools.
//!
//! Tool 1 (`ha-ha extract`) reads [`Nomination`]s and emits a [`Snapshot`]
//! per boundary per revision. Tool 2 (`ha-ha check`) diffs two snapshots into
//! a [`BoundaryDiff`] and evaluates the nomination's [`Rule`]s against it.
//!
//! Design commitments:
//! - The *skeleton* (`id`, `kind`, `container`, `arity`) is normalized and
//!   diffable; it is the only part every language extractor must agree on.
//! - Rich type info rides along as opaque, language-native `detail` text for
//!   the LLM. We never parse it.
//! - Deterministic `metrics` are precomputed into the snapshot, so the check
//!   tool never recomputes and the metrics-over-time series is just the
//!   sequence of committed snapshots.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Nomination — human-authored (TOML). Tool-1 input.
// ---------------------------------------------------------------------------

/// A file of boundary nominations (`ha-ha.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nominations {
    #[serde(rename = "boundary")]
    pub boundaries: Vec<Nomination>,
}

/// One nominated boundary: the interface, its sides, and its rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nomination {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub source: Source,
    /// Anchor symbols whose members constitute the interface surface.
    #[serde(rename = "target")]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub sides: Sides,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

/// How to extract shape for this boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    /// Drive a language server: the uniform path (rust-analyzer, tsserver, pyright…).
    Lsp {
        server: String,
        /// Workspace root the server should index.
        root: String,
    },
    /// Native adapter for an already-reified IDL (openapi, sqlx…). Later.
    Native { adapter: String, root: String },
}

/// An anchor into source: a symbol resolved within a file via `documentSymbol`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub file: String,
    /// Container-qualified symbol name, e.g. `Repository` or `mod::Trait`.
    pub symbol: String,
    /// If false, only the symbol itself is the surface; if true (default),
    /// its members become the surface.
    #[serde(default = "default_true")]
    pub members: bool,
}

/// Path globs identifying each side of the boundary, for usage expansion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sides {
    #[serde(default)]
    pub provider: Vec<String>,
    #[serde(default)]
    pub consumer: Vec<String>,
}

// ---- Rules ----

/// A rule attached to a boundary: either a deterministic gate or an LLM judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Rule {
    /// Deterministic pass/fail over the diff. No LLM.
    Gate {
        id: String,
        /// Delta predicate that trips the gate, e.g. `member.removed`.
        when: Predicate,
        #[serde(default = "default_error")]
        severity: Severity,
    },
    /// Nondeterministic judgment. A deterministic `when` triggers it; `signals`
    /// and optional `expand` assemble the context handed to the model.
    Judge {
        id: String,
        /// Deterministic trigger; if absent, runs on any change.
        #[serde(default)]
        when: Option<Predicate>,
        /// Precomputed signal names to feed the model alongside the diff.
        #[serde(default)]
        signals: Vec<String>,
        /// Optional lazy usage expansion (pull call sites at check time).
        #[serde(default)]
        expand: Option<Expand>,
        /// The rule, in natural language.
        prompt: String,
    },
}

/// A predicate over the diff. A small string DSL for now:
/// `member.added`, `member.removed`, `member.signature_changed`,
/// `interface.grew`, `interface.changed`, `always`.
pub type Predicate = String;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
    Info,
}

/// Lazy usage expansion: at check time, pull references on the named sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expand {
    /// Which sides to inspect: `provider`, `consumer`.
    pub sides: Vec<String>,
    /// What to gather per side. Today: `references`.
    #[serde(default = "default_references")]
    pub each: String,
    /// Lines of surrounding source to include with each hit.
    #[serde(default = "default_context_lines")]
    pub context_lines: u32,
}

// ---------------------------------------------------------------------------
// Snapshot — machine-emitted (JSON, git-committed). Tool-1 output / tool-2 input.
// ---------------------------------------------------------------------------

pub const SNAPSHOT_SCHEMA: &str = "haha.snapshot/v1";

/// The shape of one boundary at one revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Schema tag for forward-compat, e.g. `haha.snapshot/v1`.
    pub schema: String,
    pub boundary: String,
    /// The nomination this snapshot came from, e.g. `src/storage/mod.rs#Repository`.
    #[serde(default)]
    pub target: String,
    /// Provenance of the code state, e.g. `git:9f2c…`.
    pub revision: String,
    /// RFC 3339 timestamp.
    pub generated_at: String,
    pub source: SourceInfo,
    pub members: Vec<Member>,
    /// Deterministic, precomputed. The over-time series is these across snapshots.
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    /// `lsp` | `native`.
    pub kind: String,
    /// Server or adapter name.
    pub server: String,
}

/// One member of the interface surface. Skeleton is normalized; `detail` is opaque.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    /// Stable, nomination-relative id and the diff key, e.g. `Repository::find_user`.
    pub id: String,
    pub kind: MemberKind,
    #[serde(default)]
    pub container: Option<String>,
    /// Parameter/field count where meaningful; used by cheap metrics.
    #[serde(default)]
    pub arity: Option<u32>,
    /// Hash of the normalized `detail`; lets the differ spot "modified" without parsing.
    pub detail_hash: String,
    /// Rich signature, language-native and unparsed. For the LLM.
    pub detail: String,
    /// Language tag for `detail`, e.g. `rust`, `typescript`.
    pub detail_lang: String,
    #[serde(default)]
    pub docs: Option<String>,
    /// Minted by the language adapter from one documented, one-token rule.
    /// `None` means the language gave no honest signal — recorded, not guessed.
    #[serde(default)]
    pub visibility: Option<Visibility>,
    pub provenance: Provenance,
}

/// Normalized symbol kinds (a small projection of LSP `SymbolKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Function,
    Method,
    Record,
    Enum,
    Variant,
    Interface,
    Field,
    Const,
    TypeAlias,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub file: String,
    /// `[start_line, start_col, end_line, end_col]`, 0-based. Anchors usage expansion.
    pub range: [u32; 4],
}

// ---------------------------------------------------------------------------
// Diff — derived by tool-2. The substrate rules evaluate against.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundaryDiff {
    pub boundary: String,
    pub from: String,
    pub to: String,
    pub added: Vec<Member>,
    pub removed: Vec<Member>,
    pub modified: Vec<Modified>,
    /// Per-metric `[from, to]`.
    pub metrics_delta: BTreeMap<String, [f64; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Modified {
    pub id: String,
    pub detail_from: String,
    pub detail_to: String,
}

// ---- serde defaults ----
fn default_true() -> bool {
    true
}
fn default_error() -> Severity {
    Severity::Error
}
fn default_references() -> String {
    "references".into()
}
fn default_context_lines() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    // The riskiest bit of the contract is that internally-tagged enums
    // (`Source`, `Rule`) round-trip through TOML. Pin it with a test.
    #[test]
    fn parses_example_nomination() {
        let src = r#"
[[boundary]]
id = "storage-domain"
description = "Repository trait consumed by the domain layer"

[boundary.source]
kind = "lsp"
server = "rust-analyzer"
root = "crates/app"

[[boundary.target]]
file = "crates/app/src/storage/mod.rs"
symbol = "Repository"

[boundary.sides]
provider = ["crates/app/src/storage/**"]
consumer = ["crates/app/src/domain/**"]

[[boundary.rule]]
id = "no-member-removed"
kind = "gate"
when = "member.removed"
severity = "error"

[[boundary.rule]]
id = "keep-it-small"
kind = "judge"
when = "interface.grew"
signals = ["member.count", "member.added"]
prompt = "This boundary is meant to stay small. It grew — is the addition earning its place, or can a caller reuse an existing member?"

[[boundary.rule]]
id = "usage-on-change"
kind = "judge"
when = "member.signature_changed"
expand = { sides = ["provider", "consumer"], context_lines = 4 }
prompt = "A signature changed. Inspect call sites on both sides for work that belongs behind the interface (e.g. a looped single-item call that wants a batch method)."
"#;
        let noms: Nominations = toml::from_str(src).expect("parse nomination");
        assert_eq!(noms.boundaries.len(), 1);
        let b = &noms.boundaries[0];
        assert!(matches!(&b.source, Source::Lsp { .. }));
        assert_eq!(b.targets.len(), 1);
        assert!(b.targets[0].members); // defaulted true
        assert_eq!(b.rules.len(), 3);
        assert!(matches!(&b.rules[0], Rule::Gate { .. }));
        assert!(matches!(&b.rules[2], Rule::Judge { expand: Some(_), .. }));
    }
}
