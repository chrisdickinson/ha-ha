//! `ha-ha` — boundary nomination.
//!
//! Tool 1 (`extract`) turns a nomination into a snapshot. Tool 2 (`check`) will
//! diff snapshots and run the nomination's rules against the delta.

mod extract;
mod lang;
mod lsp;
mod model;

use std::path::PathBuf;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "\
ha-ha — name the interfaces the compiler can't see

usage:
  ha-ha extract [options] <project> <nomination>

  <nomination> is a path, optionally with a symbol after `#`:
    src/storage/mod.rs              top-level surface of the file
    src/storage                     the module or package (dir, per language)
    src/storage/mod.rs#Repository   that symbol's members
    src/storage/mod.rs#User::new    a nested symbol

options:
  --server <name>   force a language adapter (rust, typescript, go, python)
  --depth <n>       how deep to descend; 1 is the top level only
  --public          drop members the adapter knows to be private
  --pretty          indent the JSON
  --timeout <secs>  per-request budget, and how long to await indexing (60)
  -h, --help        this
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
    if command != "extract" {
        return Err(format!("unknown command `{command}`\n\n{USAGE}").into());
    }

    let mut positional: Vec<String> = Vec::new();
    let mut server = None;
    let mut public_only = false;
    let mut pretty = false;
    let mut depth = usize::MAX;
    let mut timeout = Duration::from_secs(60);

    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value").into())
        };
        match arg.as_str() {
            "--server" => server = Some(value("--server")?),
            "--depth" => {
                depth = value("--depth")?
                    .parse()
                    .map_err(|_| "--depth needs a number")?
            }
            "--timeout" => {
                let secs: u64 = value("--timeout")?
                    .parse()
                    .map_err(|_| "--timeout needs a number of seconds")?;
                timeout = Duration::from_secs(secs);
            }
            "--public" => public_only = true,
            "--pretty" => pretty = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option `{other}`\n\n{USAGE}").into());
            }
            other => positional.push(other.to_string()),
        }
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
        public_only,
        depth,
        timeout,
    })?;

    let json = if pretty {
        serde_json::to_string_pretty(&snapshot)?
    } else {
        serde_json::to_string(&snapshot)?
    };
    println!("{json}");
    Ok(())
}
