//! `ha-ha` — boundary nomination.
//!
//! Tool 1 (`extract`) turns a nomination into a snapshot. Tool 2 (`check`) will
//! diff snapshots and run the nomination's rules against the delta.

mod config;
mod extract;
mod lang;
mod lsp;
mod model;
mod schema;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{Snapshot, Source};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "\
ha-ha — name the interfaces the compiler can't see

usage:
  ha-ha extract  [options] <project> <nomination>
  ha-ha extract  [options] --config <file>
  ha-ha validate <file>

  <nomination> is a path, optionally with a symbol after `#`:
    src/storage/mod.rs              top-level surface of the file
    src/storage                     the module or package (dir, per language)
    src/storage/mod.rs#Repository   that symbol's members
    src/storage/mod.rs#User::new    a nested symbol

  --config reads boundaries from a `ha-ha.kdl`. Every boundary under a
  `project` node shares one language server, so its startup is paid once.
  Output is a JSON array of snapshots, one per boundary.

options:
  --config <file>   read boundaries from a ha-ha.kdl
  --server <name>   force a language adapter (rust, typescript, go, python)
  --depth <n>       how deep to descend; 1 is the top level only
  --public          drop members the adapter knows to be private
  --pretty          indent the JSON
  --timeout <secs>  per-request budget, and how long to await indexing (60)
  -h, --help        this

`validate` checks a config against the embedded KDL Schema
(ha-ha.schema.kdl) without starting a language server.
";

fn main() {
    if let Err(err) = run() {
        eprintln!("ha-ha: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    let (command, rest) = args.split_first().expect("non-empty");
    match command.as_str() {
        "extract" => extract(rest),
        "validate" => validate(rest),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}").into()),
    }
}

/// Check a config against the schema without spawning a single language server.
///
/// The point is the tight loop: finding a typo in a `ha-ha.kdl` should not cost
/// a four-minute metals startup.
fn validate(args: &[String]) -> Result<()> {
    let [path] = args else {
        return Err(format!(
            "expected exactly one <file>, got {}\n\n{USAGE}",
            args.len()
        )
        .into());
    };
    let path = PathBuf::from(path);
    let config = config::load(&path)?;

    let boundaries: usize = config.projects.iter().map(|p| p.boundaries.len()).sum();
    let rules: usize = config
        .projects
        .iter()
        .map(|p| p.rules.len() + p.boundaries.iter().map(|b| b.rules.len()).sum::<usize>())
        .sum();
    println!(
        "{}: ok — {}, {}, {}",
        path.display(),
        count(config.projects.len(), "project", "projects"),
        count(boundaries, "boundary", "boundaries"),
        count(rules, "rule", "rules"),
    );

    // Valid, but it will extract nothing — worth saying out loud rather than
    // letting someone wonder why their boundary never shows up.
    for project in &config.projects {
        if project.boundaries.is_empty() {
            let root = match &project.source {
                Source::Lsp { root, .. } | Source::Native { root, .. } => root,
            };
            println!("  note: project {} has no boundaries", root.display());
        }
    }
    Ok(())
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn extract(rest: &[String]) -> Result<()> {
    let mut positional: Vec<String> = Vec::new();
    let mut config = None;
    let mut server = None;
    let mut pretty = false;
    // The defaults live on `Shape`, so the CLI and any programmatic caller
    // start from the same place.
    let mut shape = extract::Shape::default();

    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value").into())
        };
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value("--config")?)),
            "--server" => server = Some(value("--server")?),
            "--depth" => {
                shape.depth = value("--depth")?
                    .parse()
                    .map_err(|_| "--depth needs a number")?
            }
            "--timeout" => {
                let secs: u64 = value("--timeout")?
                    .parse()
                    .map_err(|_| "--timeout needs a number of seconds")?;
                shape.timeout = Duration::from_secs(secs);
            }
            "--public" => shape.public_only = true,
            "--pretty" => pretty = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n\n{USAGE}").into());
            }
            other => positional.push(other.to_string()),
        }
    }

    if let Some(path) = config {
        if !positional.is_empty() {
            return Err(
                format!("--config names the boundaries; drop the positional arguments\n\n{USAGE}")
                    .into(),
            );
        }
        if server.is_some() {
            return Err("--server conflicts with --config: each `project` node names its own adapter".into());
        }
        // Always an array, even for one boundary — the shape of the output
        // should not depend on how many boundaries a config happens to hold.
        let (snapshots, failed) = from_config(&path, &shape)?;
        print_json(&snapshots, pretty)?;
        if failed > 0 {
            return Err(format!(
                "{failed} of {} boundaries failed",
                failed + snapshots.len()
            )
            .into());
        }
        return Ok(());
    }

    let [project, nomination] = positional.as_slice() else {
        return Err(format!(
            "expected <project> and <nomination>, got {}\n\n{USAGE}",
            positional.len()
        )
        .into());
    };

    let snapshot = extract::run(&extract::Options {
        project: PathBuf::from(project),
        nomination: nomination.clone(),
        server,
        public_only: shape.public_only,
        depth: shape.depth,
        timeout: shape.timeout,
    })?;
    print_json(&snapshot, pretty)
}

/// Walk a `ha-ha.kdl`: one session per project, every boundary beneath it.
///
/// A boundary that fails does not sink the run. Reaching boundary seven of
/// eight can have cost minutes of server startup, and throwing away the six
/// good snapshots to report the seventh helps nobody — so failures go to
/// stderr, the good snapshots still print, and the returned count becomes the
/// exit code's business. Only a config that won't load is fatal here.
fn from_config(path: &Path, shape: &extract::Shape) -> Result<(Vec<Snapshot>, usize)> {
    let config = config::load(path)?;
    let mut snapshots = Vec::new();
    let mut failed = 0;

    for project in &config.projects {
        let (server, root) = match &project.source {
            Source::Lsp { server, root } => (server, root),
            Source::Native { adapter, root } => {
                eprintln!(
                    "ha-ha: {}: native adapter `{adapter}` is not implemented yet — skipping",
                    root.display()
                );
                failed += project.boundaries.len();
                continue;
            }
        };
        if project.boundaries.is_empty() {
            continue; // Nothing to extract: don't pay for a server.
        }

        let mut session = match extract::Session::open(root, Some(server), None, shape.timeout) {
            Ok(session) => session,
            Err(err) => {
                eprintln!("ha-ha: {}: {err}", root.display());
                failed += project.boundaries.len();
                continue;
            }
        };

        for boundary in &project.boundaries {
            match session.snapshot(&boundary.target, Some(&boundary.id), shape) {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(err) => {
                    eprintln!("ha-ha: {}: {err}", boundary.id);
                    failed += 1;
                }
            }
        }
    }

    Ok((snapshots, failed))
}

fn print_json<T: serde::Serialize>(value: &T, pretty: bool) -> Result<()> {
    let json = if pretty {
        serde_json::to_string_pretty(value)?
    } else {
        serde_json::to_string(value)?
    };
    println!("{json}");
    Ok(())
}
