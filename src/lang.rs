//! The per-language adapter table.
//!
//! This is the entire language-specific surface of ha-ha: which server to run,
//! what a directory means, and how to read visibility off a declaration. If
//! adding a language needs more than a row here, the thin-adapter bet is in
//! trouble — which is exactly what we want to find out early.

use std::path::{Path, PathBuf};

use crate::model::Visibility;

pub struct Lang {
    pub name: &'static str,
    /// Command line, `argv[0]` resolved at runtime against PATH and fallbacks.
    pub argv: &'static [&'static str],
    /// Files that mark a project root for this language.
    pub root_markers: &'static [&'static str],
    /// LSP `languageId` for `didOpen`.
    pub language_id: &'static str,
    /// Tag written into `Member.detail_lang`.
    pub detail_lang: &'static str,
    pub extensions: &'static [&'static str],
    pub dir: DirMode,
    /// Where to look for the server besides PATH, relative to `$HOME` unless
    /// the entry starts with `./`, in which case it is relative to the project.
    pub server_hints: &'static [&'static str],
    /// How to install it, shown when lookup fails.
    pub install: &'static str,
    /// The one-token visibility rule. `None` means the language gives us no
    /// honest signal — recorded as such rather than guessed.
    pub visibility: fn(&Decl) -> Option<Visibility>,
}

/// What a visibility rule gets to look at. Deliberately narrow: a name, the
/// declaration hover returned, the source line it sits on, and whether it is
/// nested inside another member. No types, no AST.
pub struct Decl<'a> {
    pub name: &'a str,
    pub declaration: &'a str,
    pub source_line: &'a str,
    pub nested: bool,
}

/// What a directory nomination resolves to.
pub enum DirMode {
    /// A module has one entry file; the first name that exists wins.
    Entry(&'static [&'static str]),
    /// A package spans every matching file in the directory.
    Package {
        ext: &'static str,
        exclude_suffix: &'static [&'static str],
    },
}

pub const LANGS: &[Lang] = &[
    Lang {
        name: "rust",
        argv: &["rust-analyzer"],
        root_markers: &["Cargo.toml"],
        language_id: "rust",
        detail_lang: "rust",
        extensions: &["rs"],
        dir: DirMode::Entry(&["mod.rs", "lib.rs", "main.rs"]),
        server_hints: &[".cargo/bin"],
        install: "rustup component add rust-analyzer",
        visibility: rust_visibility,
    },
    Lang {
        name: "typescript",
        argv: &["tsc", "--lsp", "--stdio"],
        root_markers: &["tsconfig.json", "package.json", "jsconfig.json"],
        language_id: "typescript",
        detail_lang: "typescript",
        extensions: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        dir: DirMode::Entry(&[
            "index.ts", "index.tsx", "index.mts", "index.cts", "index.js", "index.jsx",
            "index.mjs", "index.cjs",
        ]),
        server_hints: &["./node_modules/.bin", "bin", ".local/bin"],
        install: "npm install -g typescript",
        visibility: typescript_visibility,
    },
    Lang {
        name: "go",
        argv: &["gopls"],
        root_markers: &["go.mod", "go.work"],
        language_id: "go",
        detail_lang: "go",
        extensions: &["go"],
        // A Go package is the directory: every non-test file contributes.
        dir: DirMode::Package {
            ext: "go",
            exclude_suffix: &["_test.go"],
        },
        server_hints: &["go/bin", ".local/bin"],
        install: "go install golang.org/x/tools/gopls@latest",
        visibility: go_visibility,
    },
    Lang {
        name: "scala",
        argv: &["metals"],
        root_markers: &["build.sbt", "build.sc", "build.mill", "project/build.properties", ".scala-build"],
        language_id: "scala",
        detail_lang: "scala",
        extensions: &["scala", "sc", "sbt"],
        // Like Go, a Scala package is the directory: `package storage` is
        // declared per file and spans all of them.
        dir: DirMode::Package { ext: "scala", exclude_suffix: &[] },
        server_hints: &[
            "Library/Application Support/Coursier/bin",
            ".local/share/coursier/bin",
            "bin",
        ],
        install: "brew install metals (or: cs install metals)",
        visibility: scala_visibility,
    },
    Lang {
        name: "python",
        argv: &["pyright-langserver", "--stdio"],
        root_markers: &["pyproject.toml", "setup.py", "setup.cfg"],
        language_id: "python",
        detail_lang: "python",
        extensions: &["py", "pyi"],
        dir: DirMode::Entry(&["__init__.py"]),
        server_hints: &[".local/bin", "bin", "./node_modules/.bin"],
        install: "npm install -g pyright",
        visibility: python_visibility,
    },
];

// ---- visibility rules: one token each, no type parsing ----

fn rust_visibility(d: &Decl) -> Option<Visibility> {
    let decl = d.declaration.trim_start();
    Some(if decl.starts_with("pub ") || decl.starts_with("pub(") {
        Visibility::Public
    } else {
        Visibility::Private
    })
}

/// TypeScript hover never carries the `export` keyword, so the signal has to
/// come from the declaration's own source line.
///
/// Known limitation: a symbol exported by a separate `export { x }` statement
/// reads as private. Following those would mean resolving export semantics —
/// the per-language divergence this design exists to avoid.
fn typescript_visibility(d: &Decl) -> Option<Visibility> {
    let line = d.source_line.trim_start();
    if line.starts_with("export") {
        return Some(Visibility::Public);
    }
    if line.starts_with("private ") || line.starts_with("protected ") {
        return Some(Visibility::Private);
    }
    // Interface and class members carry no marker of their own; they are as
    // visible as what contains them.
    (!d.nested).then_some(Visibility::Private)
}

/// Go's visibility rule really is the identifier's first letter.
fn go_visibility(d: &Decl) -> Option<Visibility> {
    let first = d.name.chars().next()?;
    Some(if first.is_uppercase() {
        Visibility::Public
    } else {
        Visibility::Private
    })
}

/// Scala members are public unless marked; `private[pkg]` is still private.
/// Checks the source line as well as hover, because metals renders a method's
/// signature without its modifiers.
fn scala_visibility(d: &Decl) -> Option<Visibility> {
    let marked = |text: &str| {
        let text = text.trim_start();
        text.starts_with("private") || text.starts_with("protected")
    };
    if marked(d.declaration) || marked(d.source_line) {
        return Some(Visibility::Private);
    }
    // An unmarked member is as visible as what encloses it — a public method
    // of a private class is not reachable.
    (!d.nested).then_some(Visibility::Public)
}

/// Python has no enforcement, only the leading-underscore convention.
fn python_visibility(d: &Decl) -> Option<Visibility> {
    Some(if d.name.starts_with('_') {
        Visibility::Private
    } else {
        Visibility::Public
    })
}

// ---- selection ----

pub fn by_name(name: &str) -> Option<&'static Lang> {
    LANGS
        .iter()
        .find(|l| l.name == name || l.argv[0] == name)
}

pub fn by_extension(path: &Path) -> Option<&'static Lang> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

/// Fall back to whatever the project root looks like, for directory nominations.
pub fn by_root_markers(project: &Path) -> Option<&'static Lang> {
    LANGS
        .iter()
        .find(|l| l.root_markers.iter().any(|m| project.join(m).exists()))
}

// ---- directory resolution ----

/// Expand a nomination path into the set of files that make up its surface.
pub fn files_for(lang: &Lang, path: &Path) -> crate::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(format!("no such file or directory: {}", path.display()).into());
    }
    match lang.dir {
        DirMode::Entry(names) => {
            for name in names {
                let candidate = path.join(name);
                if candidate.is_file() {
                    return Ok(vec![candidate]);
                }
            }
            Err(format!(
                "{} has no {} module entry (looked for {})",
                path.display(),
                lang.name,
                names.join(", ")
            )
            .into())
        }
        DirMode::Package {
            ext,
            exclude_suffix,
        } => {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some(ext))
                .filter(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    !exclude_suffix.iter().any(|s| name.ends_with(s))
                })
                .collect();
            if files.is_empty() {
                return Err(format!(
                    "{} contains no .{ext} files",
                    path.display()
                )
                .into());
            }
            // Sorted so the snapshot doesn't depend on readdir order.
            files.sort();
            Ok(files)
        }
    }
}

/// Find the server binary: PATH first, then the language's known install spots.
pub fn resolve_server(lang: &Lang, project: &Path) -> Option<PathBuf> {
    let bin = lang.argv[0];
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(bin);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for hint in lang.server_hints {
        let dir = match hint.strip_prefix("./") {
            Some(rel) => project.join(rel),
            None => home.as_ref()?.join(hint),
        };
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(name: &str) -> &'static Lang {
        by_name(name).expect("known language")
    }

    fn decl<'a>(name: &'a str, declaration: &'a str, source_line: &'a str, nested: bool) -> Decl<'a> {
        Decl { name, declaration, source_line, nested }
    }

    #[test]
    fn visibility_rules_are_one_token() {
        let rust = lang("rust").visibility;
        assert_eq!(rust(&decl("f", "pub fn f()", "", false)), Some(Visibility::Public));
        assert_eq!(rust(&decl("f", "pub(crate) fn f()", "", false)), Some(Visibility::Public));
        assert_eq!(rust(&decl("f", "fn f()", "", false)), Some(Visibility::Private));
        // `public` is not `pub` — don't match on a prefix alone.
        assert_eq!(rust(&decl("public_x", "fn public_x()", "", false)), Some(Visibility::Private));

        let go = lang("go").visibility;
        assert_eq!(go(&decl("Find", "func Find()", "", false)), Some(Visibility::Public));
        assert_eq!(go(&decl("find", "func find()", "", false)), Some(Visibility::Private));

        let py = lang("python").visibility;
        assert_eq!(py(&decl("_helper", "", "", false)), Some(Visibility::Private));
        assert_eq!(py(&decl("helper", "", "", false)), Some(Visibility::Public));
    }

    #[test]
    fn typescript_reads_export_from_the_source_line() {
        let ts = lang("typescript").visibility;
        // Hover gives `function openPool(...)` either way; the source line decides.
        let hover = "function openPool(url: string): string";
        assert_eq!(
            ts(&decl("openPool", hover, "export function openPool(url: string) {", false)),
            Some(Visibility::Public)
        );
        assert_eq!(
            ts(&decl("openPool", hover, "function openPool(url: string) {", false)),
            Some(Visibility::Private)
        );
        // Interface members have no marker — defer to the container.
        assert_eq!(ts(&decl("findUser", "", "  findUser(id: number): User;", true)), None);
        assert_eq!(
            ts(&decl("secret", "", "  private secret(): void;", true)),
            Some(Visibility::Private)
        );
    }

    #[test]
    fn scala_is_public_unless_marked() {
        let scala = lang("scala").visibility;
        assert_eq!(scala(&decl("f", "def f(): Unit", "  def f(): Unit", false)), Some(Visibility::Public));
        assert_eq!(
            scala(&decl("f", "def f(): Unit", "  private def f(): Unit", false)),
            Some(Visibility::Private)
        );
        // Qualified private is still private.
        assert_eq!(
            scala(&decl("f", "", "  private[storage] def f(): Unit", false)),
            Some(Visibility::Private)
        );
        assert_eq!(
            scala(&decl("f", "", "  protected def f(): Unit", false)),
            Some(Visibility::Private)
        );
        // Unmarked members defer to the enclosing type.
        assert_eq!(scala(&decl("f", "def f(): Unit", "  def f(): Unit", true)), None);
    }

    #[test]
    fn entry_mode_prefers_mod_over_lib() {
        let dir = tempdir("entry");
        std::fs::write(dir.join("lib.rs"), "").unwrap();
        std::fs::write(dir.join("mod.rs"), "").unwrap();
        let files = files_for(lang("rust"), &dir).unwrap();
        assert_eq!(files, vec![dir.join("mod.rs")]);
    }

    #[test]
    fn package_mode_collects_the_whole_dir_but_skips_tests() {
        let dir = tempdir("package");
        for name in ["b.go", "a.go", "a_test.go", "README.md"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let files = files_for(lang("go"), &dir).unwrap();
        assert_eq!(files, vec![dir.join("a.go"), dir.join("b.go")]);
    }

    #[test]
    fn missing_entry_names_what_it_looked_for() {
        let dir = tempdir("empty");
        let err = files_for(lang("rust"), &dir).unwrap_err().to_string();
        assert!(err.contains("mod.rs"), "unhelpful error: {err}");
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("haha-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
