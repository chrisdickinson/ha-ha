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
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Nomination — human-authored (KDL). Tool-1 input.
// ---------------------------------------------------------------------------

/// A file of boundary nominations (`ha-ha.kdl`), built by [`crate::config`].
#[derive(Debug, Clone)]
pub struct Config {
    pub projects: Vec<Project>,
}

/// One extraction source and everything that shares it.
///
/// The grouping is load-bearing: a language server is spawned *once* per
/// project and reused across every boundary beneath it, which is the whole
/// reason boundaries nest rather than sitting in a flat list. `rules` here
/// apply to all of those boundaries.
#[derive(Debug, Clone)]
pub struct Project {
    pub source: Source,
    pub rules: Vec<Rule>,
    pub boundaries: Vec<Nomination>,
}

/// One nominated boundary: the interface, its sides, and its rules.
#[derive(Debug, Clone)]
pub struct Nomination {
    /// Stable id. Becomes `Snapshot.boundary` — the diff key.
    pub id: String,
    pub description: String,
    /// What to extract, in [`crate::extract::parse_nomination`] form:
    /// `<path>`, `<path>#<Symbol>`, or `<path>#<Outer>::<Inner>`.
    pub target: String,
    pub sides: Sides,
    pub rules: Vec<Rule>,
}

/// How to extract shape for a project.
#[derive(Debug, Clone)]
pub enum Source {
    /// Drive a language server: the uniform path (rust-analyzer, gopls, pyright…).
    Lsp {
        server: String,
        /// Workspace root the server should index.
        root: PathBuf,
    },
    /// Native adapter for an already-reified IDL (openapi, sqlx…). Later.
    Native { adapter: String, root: PathBuf },
}

/// Path globs identifying each side of the boundary, for usage expansion.
#[derive(Debug, Clone, Default)]
pub struct Sides {
    pub provider: Vec<String>,
    pub consumer: Vec<String>,
}

// ---- Rules ----

/// A rule attached to a boundary: either a deterministic gate or an LLM judge.
///
/// `ha-ha.kdl` only writes [`Rule::Judge`] today — a `rule` node. `Gate` is the
/// deterministic half the check tool will need, kept here so the predicate
/// vocabulary has one home.
#[derive(Debug, Clone)]
pub enum Rule {
    /// Deterministic pass/fail over the diff. No LLM.
    Gate {
        id: String,
        /// Delta predicate that trips the gate, e.g. `member.removed`.
        when: Predicate,
        severity: Severity,
    },
    /// Nondeterministic judgment. A deterministic `when` triggers it; `signals`
    /// and optional `expand` assemble the context handed to the model.
    Judge {
        id: String,
        /// Deterministic trigger; if absent, runs on any change.
        when: Option<Predicate>,
        /// Precomputed signal names to feed the model alongside the diff.
        signals: Vec<String>,
        /// Optional lazy usage expansion (pull call sites at check time).
        expand: Option<Expand>,
        /// The rule, in natural language.
        prompt: String,
    },
}

/// A predicate over the diff. A small string DSL for now; see [`PREDICATES`].
pub type Predicate = String;

/// The closed predicate vocabulary. Closed on purpose: a `when` the evaluator
/// does not know would silently never fire, so [`crate::config`] rejects
/// anything outside this list at load time.
pub const PREDICATES: &[&str] = &[
    "member.added",
    "member.removed",
    "member.signature_changed",
    "interface.grew",
    "interface.changed",
    "always",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
    Info,
}

/// Lazy usage expansion: at check time, pull references on the named sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expand {
    /// Which sides to inspect: `provider`, `consumer`.
    pub sides: Vec<String>,
    /// What to gather per side. Today: `references`.
    pub each: String,
    /// Lines of surrounding source to include with each hit.
    pub context_lines: u32,
}

impl Default for Expand {
    fn default() -> Self {
        Expand {
            sides: Vec::new(),
            each: "references".into(),
            context_lines: 3,
        }
    }
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
