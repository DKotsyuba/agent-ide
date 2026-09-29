//! Lexical reachability of a Rust source file from its package's build targets, so an edit to a
//! file no `mod` declaration reaches answers `not_analysed` instead of a clean `cargo check`.
//!
//! A pure function of file text, walked bottom-up from the edited file: a crate root follows
//! Cargo's target auto-discovery (`src/lib.rs`, `src/main.rs`, `src/bin/*`, `tests/*`,
//! `examples/*`, `benches/*`, `build.rs`) or is a target `path` the manifest names, and a module
//! file is reached when its parent module file — `dir/mod.rs`, `dir.rs`, or a crate root in `dir` —
//! declares `mod name;` and is itself reached.
//!
//! Ceilings, each answered "unreached" (a redundant notice, never a false clean): a `mod` with a
//! `#[path]` attribute; a `mod` gated by any `cfg` or `cfg_attr` other than exactly
//! `#[cfg(test)]`, since the check may not meet the condition (`cfg(windows)` on macOS); a
//! `#[cfg(test)]` one whose chain ends at a root `cargo check --all-targets` does not build as a
//! unit or integration test ([`unit_test_root`]); a module declared inside an inline
//! `mod x { … }` block or a macro; and a module file larger than [`MAX_FILE_BYTES`]. A `mod` line
//! inside a block comment counts as a declaration. A file outside any package in the worktree is
//! left to the check (reached).

use std::path::{Path, PathBuf};

/// Largest parent module file read while walking; a larger one declares nothing.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// Upper bound on walk steps; each step moves to the declaring parent module file.
const MAX_STEPS: usize = 64;

/// Reason reported for a file no build target reaches.
pub const UNREACHED: &str = "rust check did not compile this file — not declared with `mod`";

/// How a `mod name;` declaration is gated by its attributes, among gates proven compiled.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Gate {
    /// Exactly `#[cfg(test)]`: compiled only where the root is built as a test.
    Test,
    /// No `cfg`, `cfg_attr` or `path` attribute: compiled wherever its parent module is.
    Always,
}

/// Reports whether the worktree-relative `path` is reached from a build target of its nearest
/// package; a file under no package manifest inside `worktree` counts as reached. A chain with a
/// `#[cfg(test)]` declaration counts only when its root is a [`unit_test_root`].
pub fn reached(worktree: &Path, path: &Path) -> bool {
    let file = worktree.join(path);
    let Some(package) = file
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(worktree))
        .find(|dir| dir.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
    else {
        return true;
    };
    let manifest = std::fs::read_to_string(package.join("Cargo.toml")).unwrap_or_default();
    let mut current = file;
    let mut test_only = false;
    for _ in 0..MAX_STEPS {
        if is_root(&package, &manifest, &current) {
            return !test_only || unit_test_root(&package, &manifest, &current);
        }
        let Some((name, declarers)) = parent_module(&package, &manifest, &current) else {
            return false;
        };
        match declarers
            .into_iter()
            .find_map(|declarer| Some((declares(&declarer, &name)?, declarer)))
        {
            Some((gate, declarer)) => {
                test_only |= gate == Gate::Test;
                current = declarer;
            }
            None => return false,
        }
    }
    false
}

/// Reports whether `cargo check --all-targets` builds the crate root `file` with `cfg(test)`: the
/// auto-discovered library, a binary or an integration test, unless any manifest line turns a
/// target's `test` or `harness` off. Examples, benches, the build script and manifest-named
/// paths are not proven.
fn unit_test_root(package: &Path, manifest: &str, file: &Path) -> bool {
    let Ok(relative) = file.strip_prefix(package) else {
        return false;
    };
    let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
    matches!(
        parts.as_slice(),
        ["src", "lib.rs"] | ["src", "main.rs"] | ["src", "bin", ..] | ["tests", ..]
    ) && !manifest.lines().any(|line| {
        let line: String = line.split_whitespace().collect();
        line.starts_with("test=false") || line.starts_with("harness=false")
    })
}

/// Reports whether `file` is a crate root of `package`: an auto-discovered target or a target
/// `path` quoted in the manifest.
fn is_root(package: &Path, manifest: &str, file: &Path) -> bool {
    let Ok(relative) = file.strip_prefix(package) else {
        return false;
    };
    let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
    let discovered = match parts.as_slice() {
        ["build.rs"] | ["src", "lib.rs"] | ["src", "main.rs"] | ["src", "bin", _, "main.rs"] => {
            true
        }
        ["src", "bin", name] => name.ends_with(".rs"),
        [dir, name] if matches!(*dir, "tests" | "examples" | "benches") => name.ends_with(".rs"),
        [dir, _, "main.rs"] => matches!(*dir, "tests" | "examples" | "benches"),
        _ => false,
    };
    let relative = relative.display();
    discovered
        || manifest.contains(&format!("\"{relative}\""))
        || manifest.contains(&format!("'{relative}'"))
        || manifest.contains(&format!("\"./{relative}\""))
}

/// Returns the module name `file` defines and the existing files that may declare it: the
/// owning directory's `mod.rs`, the sibling `dir.rs` (unless that is a crate root, which owns its
/// own directory's modules instead), and every crate root directly in the owning directory.
fn parent_module(package: &Path, manifest: &str, file: &Path) -> Option<(String, Vec<PathBuf>)> {
    let dir = file.parent()?;
    let (name, owner) = if file.file_name()? == "mod.rs" {
        (dir.file_name()?.to_str()?.to_owned(), dir.parent()?)
    } else {
        (file.file_stem()?.to_str()?.to_owned(), dir)
    };
    let mut declarers = vec![owner.join("mod.rs")];
    if let (Some(parent), Some(stem)) = (owner.parent(), owner.file_name()) {
        let mut sibling = stem.to_os_string();
        sibling.push(".rs");
        let sibling = parent.join(sibling);
        if sibling.starts_with(package) && !is_root(package, manifest, &sibling) {
            declarers.push(sibling);
        }
    }
    if let Ok(entries) = std::fs::read_dir(owner) {
        let mut roots: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|candidate| is_root(package, manifest, candidate))
            .collect();
        roots.sort();
        declarers.extend(roots);
    }
    declarers.retain(|declarer| declarer != file && declarer.is_file());
    Some((name, declarers))
}

/// Returns how `file` declares `mod name;` (any visibility, attributes on the same or preceding
/// lines), the least gated when declared more than once: [`Gate::Always`] without a condition,
/// [`Gate::Test`] under exactly `#[cfg(test)]`, and `None` when it is not declared or only under
/// an unproven attribute — any other `cfg`, a `cfg_attr` (which may also add a `path`), a
/// `path`, or a `cfg`/`path` attribute continued over several lines.
fn declares(file: &Path, name: &str) -> Option<Gate> {
    if std::fs::metadata(file).map_or(true, |meta| meta.len() > MAX_FILE_BYTES) {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?;
    let mut found = None;
    // Gate of the attributes read so far for the next item; `None` once one is unproven.
    let mut gate = Some(Gate::Always);
    // Inside an attribute continued from an earlier line, until its first `]`.
    let mut open = false;
    for line in text.lines() {
        let mut rest = line.trim();
        if open {
            let Some(end) = rest.find(']') else {
                continue;
            };
            open = false;
            rest = rest[end + 1..].trim_start();
        }
        if rest.starts_with("//") {
            continue;
        }
        while let Some(attribute) = rest.strip_prefix("#[") {
            let (body, after) = attribute.split_once(']').unwrap_or((attribute, ""));
            open = !attribute.contains(']');
            let body: String = body.split_whitespace().collect();
            if body == "cfg(test)" {
                gate = gate.and(Some(Gate::Test));
            } else if body.starts_with("cfg") || body.starts_with("path") {
                gate = None;
            }
            rest = after.trim_start();
        }
        if rest.is_empty() {
            continue;
        }
        let item_gate = std::mem::replace(&mut gate, Some(Gate::Always));
        let Some(declaration) = without_visibility(rest).strip_prefix("mod ") else {
            continue;
        };
        let Some((ident, _)) = declaration.split_once(';') else {
            continue;
        };
        let ident = ident.trim();
        if ident.strip_prefix("r#").unwrap_or(ident) == name {
            found = found.max(item_gate);
        }
    }
    found
}

/// Strips a leading `pub` or `pub(…)` visibility from one item line.
fn without_visibility(item: &str) -> &str {
    if let Some(scoped) = item.strip_prefix("pub(") {
        return scoped
            .split_once(')')
            .map_or(item, |(_, after)| after.trim_start());
    }
    item.strip_prefix("pub ").map_or(item, str::trim_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a scratch package from `(relative path, contents)` pairs.
    fn package(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agent-ide-module-graph-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        root
    }

    /// Declared modules — plain, `pub`, `pub(crate)`, attributed, nested through `mod.rs` and
    /// through `dir.rs` — are reached; an undeclared file is not.
    #[test]
    fn declared_modules_are_reached_and_undeclared_ones_are_not() {
        let root = package(
            "declared",
            &[
                ("Cargo.toml", "[package]\nname = \"fixture\"\n"),
                (
                    "src/lib.rs",
                    "pub mod a;\npub(crate) mod b;\n#[cfg(test)] mod c;\n// mod commented;\n",
                ),
                ("src/a.rs", "mod nested;\n"),
                ("src/a/nested.rs", ""),
                ("src/b/mod.rs", "mod inner;\n"),
                ("src/b/inner.rs", ""),
                ("src/c.rs", ""),
                ("src/commented.rs", ""),
                ("src/orphan.rs", ""),
                ("src/b/orphan.rs", ""),
            ],
        );
        for reached_path in [
            "src/lib.rs",
            "src/a.rs",
            "src/a/nested.rs",
            "src/b/mod.rs",
            "src/b/inner.rs",
            "src/c.rs",
        ] {
            assert!(reached(&root, Path::new(reached_path)), "{reached_path}");
        }
        for unreached in ["src/commented.rs", "src/orphan.rs", "src/b/orphan.rs"] {
            assert!(!reached(&root, Path::new(unreached)), "{unreached}");
        }
    }

    /// Auto-discovered targets and their modules are reached; `tests/common/mod.rs` is reached
    /// through any integration test that declares it; nested packages use their own manifest.
    #[test]
    fn targets_and_nested_packages_are_roots() {
        let root = package(
            "targets",
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/x\"]\n"),
                ("crates/x/Cargo.toml", "[package]\nname = \"x\"\n"),
                ("crates/x/src/main.rs", "mod cli;\n"),
                ("crates/x/src/cli.rs", ""),
                ("crates/x/src/bin/tool.rs", "mod helper;\n"),
                ("crates/x/src/bin/helper.rs", ""),
                ("crates/x/tests/it.rs", "mod common;\n"),
                ("crates/x/tests/common/mod.rs", ""),
                ("crates/x/build.rs", ""),
                ("crates/x/src/stray.rs", ""),
                ("scripts/loose.rs", ""),
            ],
        );
        for reached_path in [
            "crates/x/src/cli.rs",
            "crates/x/src/bin/helper.rs",
            "crates/x/tests/common/mod.rs",
            "crates/x/build.rs",
        ] {
            assert!(reached(&root, Path::new(reached_path)), "{reached_path}");
        }
        assert!(!reached(&root, Path::new("crates/x/src/stray.rs")));
        // The root manifest owns `scripts/`, and nothing declares the file there.
        assert!(!reached(&root, Path::new("scripts/loose.rs")));
    }

    /// A `#[path]`-redirected declaration is a documented ceiling: the default-named file it
    /// shadows reads unreached, never guessed clean; a manifest target `path` is a root.
    #[test]
    fn path_attributes_are_a_ceiling_and_manifest_paths_are_roots() {
        let root = package(
            "path-attribute",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"p\"\n[[bin]]\nname = \"extra\"\npath = \"tools/extra.rs\"\n",
                ),
                ("src/lib.rs", "#[path = \"elsewhere.rs\"]\nmod shadowed;\n"),
                ("src/shadowed.rs", ""),
                ("src/elsewhere.rs", ""),
                ("tools/extra.rs", "mod util;\n"),
                ("tools/util.rs", ""),
            ],
        );
        assert!(!reached(&root, Path::new("src/shadowed.rs")));
        assert!(!reached(&root, Path::new("src/elsewhere.rs")));
        assert!(reached(&root, Path::new("tools/extra.rs")));
        assert!(reached(&root, Path::new("tools/util.rs")));
    }

    /// A `mod` gated by a condition the check may not meet — `cfg(windows)`, a multi-line `cfg`,
    /// a `cfg_attr` — is unproven, so its file reads unreached; `cfg(test)` counts only where
    /// `--all-targets` compiles the root as a unit test (a library or binary with its test harness,
    /// not an example or a `test = false` target).
    #[test]
    fn conditional_declarations_are_unproven() {
        let root = package(
            "conditional",
            &[
                ("Cargo.toml", "[package]\nname = \"c\"\n"),
                (
                    "src/lib.rs",
                    "#[cfg(windows)] mod win;\n#[cfg(any(\n    windows,\n    target_os = \"ios\",\n))]\nmod multi;\n#[cfg_attr(unix, path = \"unix.rs\")]\nmod imp;\n#[cfg( test )]\nmod tests;\n",
                ),
                ("src/win.rs", ""),
                ("src/multi.rs", ""),
                ("src/imp.rs", ""),
                ("src/tests.rs", ""),
                ("examples/demo/main.rs", "#[cfg(test)]\nmod helper;\n"),
                ("examples/demo/helper.rs", ""),
            ],
        );
        for unreached in [
            "src/win.rs",
            "src/multi.rs",
            "src/imp.rs",
            "examples/demo/helper.rs",
        ] {
            assert!(!reached(&root, Path::new(unreached)), "{unreached}");
        }
        assert!(reached(&root, Path::new("src/tests.rs")));
        let untested = package(
            "untested",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"u\"\n[lib]\ntest = false\n",
                ),
                ("src/lib.rs", "#[cfg(test)]\nmod tests;\n"),
                ("src/tests.rs", ""),
            ],
        );
        assert!(!reached(&untested, Path::new("src/tests.rs")));
    }
}
