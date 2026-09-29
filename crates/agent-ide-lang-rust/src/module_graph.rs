//! Lexical reachability of a Rust source file from its package's build targets, so an edit to a
//! file no `mod` declaration reaches answers `not_analysed` instead of a clean `cargo check`.
//!
//! A pure function of file text, walked bottom-up from the edited file: a crate root follows
//! Cargo's target auto-discovery (`src/lib.rs`, `src/main.rs`, `src/bin/*`, `tests/*`,
//! `examples/*`, `benches/*`, `build.rs`) or is a target `path` the manifest names, and a module
//! file is reached when its parent module file — `dir/mod.rs`, `dir.rs`, or a crate root in `dir` —
//! declares `mod name;` unconditionally and is itself reached. Files are judged on code only:
//! comments and string/char literals are blanked first ([`code`]).
//!
//! Ceilings, each answered "unreached" (a redundant notice, never a false clean): a `mod` with a
//! `#[path]` attribute; a `mod` gated by any `cfg` or `cfg_attr`, `#[cfg(test)]` included, since
//! the check may not meet the condition (`cfg(windows)` on macOS, a target built without its test
//! harness); a file on the chain with an inner `#![cfg…]` attribute, which makes that whole file
//! conditional; a module declared inside an inline `mod x { … }` block or a macro; and a module
//! file larger than [`MAX_FILE_BYTES`]. A file outside any package in the worktree is left to the
//! check (reached).

use std::path::{Path, PathBuf};

/// Largest module file read while walking; a larger one declares nothing and proves nothing.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// Upper bound on walk steps; each step moves to the declaring parent module file.
const MAX_STEPS: usize = 64;

/// Reason reported for a file no build target reaches.
pub const UNREACHED: &str =
    "rust check may not have compiled this file — no unconditional `mod` declaration reaches it";

/// Reports whether the worktree-relative `path` is reached from a build target of its nearest
/// package; a file under no package manifest inside `worktree` counts as reached. The edited file
/// itself and every file on its chain must be readable and free of inner `cfg` attributes.
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
    if code(&file).is_none_or(|text| conditional_file(&text)) {
        return false;
    }
    let manifest = std::fs::read_to_string(package.join("Cargo.toml")).unwrap_or_default();
    let mut current = file;
    for _ in 0..MAX_STEPS {
        if is_root(&package, &manifest, &current) {
            return true;
        }
        let Some((name, declarers)) = parent_module(&package, &manifest, &current) else {
            return false;
        };
        match declarers
            .into_iter()
            .find(|declarer| declares(declarer, &name))
        {
            Some(declarer) => current = declarer,
            None => return false,
        }
    }
    false
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

/// Reports whether `file` declares `mod name;` unconditionally: any visibility, with outer
/// attributes on the same or preceding lines (blank and comment lines between them keep them in
/// force), none of which is a `cfg`, `cfg_attr` or `path` — an attribute continued over several
/// lines is judged by its first line. A file that is unreadable, too large, or conditional as a
/// whole ([`conditional_file`]) declares nothing.
fn declares(file: &Path, name: &str) -> bool {
    let Some(text) = code(file) else {
        return false;
    };
    if conditional_file(&text) {
        return false;
    }
    // An attribute read so far for the next item makes it conditional or redirects it.
    let mut unproven = false;
    // Bracket depth of an attribute continued from an earlier line; zero outside one.
    let mut depth = 0;
    for line in text.lines() {
        let mut rest = line.trim();
        if depth > 0 {
            let Some(end) = attribute_end(rest, &mut depth) else {
                continue;
            };
            rest = rest[end..].trim_start();
        }
        while let Some(attribute) = rest
            .strip_prefix('#')
            .map(str::trim_start)
            .and_then(|after| after.strip_prefix('['))
        {
            let head: String = attribute.split_whitespace().collect();
            unproven |= head.starts_with("cfg") || head.starts_with("path");
            depth = 1;
            let Some(end) = attribute_end(attribute, &mut depth) else {
                rest = "";
                break;
            };
            rest = attribute[end..].trim_start();
        }
        if rest.is_empty() {
            continue;
        }
        let gated = std::mem::take(&mut unproven);
        let Some(declaration) = without_visibility(rest).strip_prefix("mod ") else {
            continue;
        };
        let Some((ident, _)) = declaration.split_once(';') else {
            continue;
        };
        let ident = ident.trim();
        if !gated && ident.strip_prefix("r#").unwrap_or(ident) == name {
            return true;
        }
    }
    false
}

/// Scans `text` for the `]` that closes an attribute at bracket `depth` (1 right after `#[`),
/// updating `depth`; returns the byte index just past it, or `None` when `text` ends first.
fn attribute_end(text: &str, depth: &mut usize) -> Option<usize> {
    for (index, character) in text.char_indices() {
        match character {
            '[' => *depth += 1,
            ']' => {
                *depth -= 1;
                if *depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Reports whether code-only `text` carries an inner `cfg` or `cfg_attr` attribute (`#![cfg…]`,
/// whitespace between the tokens allowed) anywhere, which makes the file conditional.
fn conditional_file(text: &str) -> bool {
    text.lines().any(|line| {
        line.split_whitespace()
            .collect::<String>()
            .contains("#![cfg")
    })
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

/// Reads `file` as code only: at most [`MAX_FILE_BYTES`] of UTF-8, with every comment and
/// string or char literal replaced by spaces and every line break kept, so a `mod`, `//`, `]` or
/// `#![cfg` inside one is not syntax. `None` when the file is missing, larger, or not UTF-8.
fn code(file: &Path) -> Option<String> {
    if std::fs::metadata(file).map_or(true, |meta| meta.len() > MAX_FILE_BYTES) {
        return None;
    }
    let text: Vec<char> = std::fs::read_to_string(file).ok()?.chars().collect();
    let mut code = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        match blank_end(&text, index) {
            Some(end) => {
                code.extend(
                    text[index..end]
                        .iter()
                        .map(|&character| if character == '\n' { '\n' } else { ' ' }),
                );
                index = end;
            }
            None => {
                code.push(text[index]);
                index += 1;
            }
        }
    }
    Some(code)
}

/// Returns the end (exclusive) of the comment or literal starting at `index` of `text`, or `None`
/// when none starts there. Handles `//` and nested `/* */` comments, escaped `"…"` strings,
/// raw `r#"…"#` strings (also `br`/`cr`), and `'x'`/`'\…'` char literals; a lifetime or label
/// (`'a`) is not a literal. An unterminated comment or literal runs to the end of `text`.
fn blank_end(text: &[char], index: usize) -> Option<usize> {
    let at = |offset: usize| text.get(index + offset).copied();
    let identifier = |position: Option<usize>| {
        position
            .and_then(|position| text.get(position))
            .is_some_and(|character| character.is_alphanumeric() || *character == '_')
    };
    let from = |start: usize, found: &dyn Fn(usize) -> Option<usize>| {
        (start..text.len()).find_map(found).unwrap_or(text.len())
    };
    match (text[index], at(1)) {
        ('/', Some('/')) => Some(from(index, &|at| (text[at] == '\n').then_some(at))),
        ('/', Some('*')) => {
            let mut depth = 0usize;
            let mut position = index;
            while position < text.len() {
                match (text[position], text.get(position + 1)) {
                    ('/', Some('*')) => {
                        depth += 1;
                        position += 2;
                    }
                    ('*', Some('/')) => {
                        depth -= 1;
                        position += 2;
                        if depth == 0 {
                            return Some(position);
                        }
                    }
                    _ => position += 1,
                }
            }
            Some(text.len())
        }
        ('"', _) => {
            let mut position = index + 1;
            while position < text.len() {
                match text[position] {
                    '\\' => position += 2,
                    '"' => return Some(position + 1),
                    _ => position += 1,
                }
            }
            Some(text.len())
        }
        ('r', Some('"' | '#')) => {
            let before = index.checked_sub(1);
            let prefixed = before.is_some_and(|before| matches!(text[before], 'b' | 'c'));
            let word = if prefixed {
                identifier(before.and_then(|before| before.checked_sub(1)))
            } else {
                identifier(before)
            };
            if word {
                return None;
            }
            let hashes = text[index + 1..]
                .iter()
                .take_while(|&&character| character == '#')
                .count();
            if at(1 + hashes) != Some('"') {
                return None;
            }
            let start = index + 2 + hashes;
            Some(from(start, &|at| {
                (text[at] == '"'
                    && text[at + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|&&c| c == '#')
                        .count()
                        == hashes)
                    .then_some(at + 1 + hashes)
            }))
        }
        ('\'', Some('\\')) => Some(from(index + 3, &|at| (text[at] == '\'').then_some(at + 1))),
        ('\'', Some(_)) if at(2) == Some('\'') => Some(index + 3),
        _ => None,
    }
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
                    "pub mod a;\npub(crate) mod b;\n#[allow(dead_code)] mod c;\n// mod commented;\n",
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
    /// a `cfg_attr`, `cfg(test)` from a library or an example — is unproven, so its file reads
    /// unreached.
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
            "src/tests.rs",
            "examples/demo/helper.rs",
        ] {
            assert!(!reached(&root, Path::new(unreached)), "{unreached}");
        }
    }

    /// Comments never end an attribute's reach nor count as code: a `cfg` followed by a trailing
    /// `//` note, a block comment or a doc comment still gates the next `mod`; a `mod` inside a
    /// block comment or a string declares nothing; `//` and `]` inside strings are not syntax.
    #[test]
    fn comments_and_literals_do_not_reset_or_fake_a_gate() {
        let root = package(
            "comments",
            &[
                ("Cargo.toml", "[package]\nname = \"k\"\n"),
                (
                    "src/lib.rs",
                    "#[cfg(windows)] // note\nmod windows_only;\n#[cfg(windows)]\n/* a\n   b */\n/// doc\nmod blocky;\n/* mod in_block; */\nconst S: &str = \"\nmod in_string;\n\";\n#[doc = \"http://x ]\"]\n#[cfg(windows)]\nmod after_doc;\nmod plain; // mod trailing;\n",
                ),
                ("src/windows_only.rs", ""),
                ("src/blocky.rs", ""),
                ("src/in_block.rs", ""),
                ("src/in_string.rs", ""),
                ("src/after_doc.rs", ""),
                ("src/plain.rs", ""),
                ("src/trailing.rs", ""),
            ],
        );
        for unreached in [
            "src/windows_only.rs",
            "src/blocky.rs",
            "src/in_block.rs",
            "src/in_string.rs",
            "src/after_doc.rs",
            "src/trailing.rs",
        ] {
            assert!(!reached(&root, Path::new(unreached)), "{unreached}");
        }
        assert!(reached(&root, Path::new("src/plain.rs")));
    }

    /// An inner `#![cfg(…)]`/`#![cfg_attr(…)]` makes its own file conditional: that file and every
    /// module reached only through it read unreached, a crate root included.
    #[test]
    fn inner_cfg_makes_a_file_and_its_modules_unproven() {
        let root = package(
            "inner",
            &[
                ("Cargo.toml", "[package]\nname = \"i\"\n"),
                ("src/lib.rs", "mod gated;\nmod plain;\n"),
                (
                    "src/gated.rs",
                    "//! Windows only.\n#![cfg(windows)]\nmod inner;\n",
                ),
                ("src/gated/inner.rs", ""),
                ("src/plain.rs", "#! [ cfg_attr(unix, path = \"x\") ]\n"),
                (
                    "src/bin/win/main.rs",
                    "#![cfg(target_os = \"windows\")]\nmod helper;\n",
                ),
                ("src/bin/win/helper.rs", ""),
                ("src/bin/other.rs", "#![allow(dead_code)]\n"),
            ],
        );
        for unreached in [
            "src/gated.rs",
            "src/gated/inner.rs",
            "src/plain.rs",
            "src/bin/win/main.rs",
            "src/bin/win/helper.rs",
        ] {
            assert!(!reached(&root, Path::new(unreached)), "{unreached}");
        }
        assert!(reached(&root, Path::new("src/bin/other.rs")));
    }

    /// `#[cfg(test)]` is unproven like every other gate: whether the check builds a target's test
    /// harness depends on manifest forms (`[lib] test = false`, `lib = { test = false }`, a
    /// custom harness) a line scan cannot judge with certainty, so its module reads unreached.
    #[test]
    fn cfg_test_declarations_are_unproven() {
        let inline = package(
            "cfg-test-inline",
            &[
                (
                    "Cargo.toml",
                    "lib = { path = \"src/lib.rs\", test = false }\n[package]\nname = \"t\"\n",
                ),
                ("src/lib.rs", "#[cfg(test)]\nmod tests;\n"),
                ("src/tests.rs", ""),
            ],
        );
        assert!(!reached(&inline, Path::new("src/tests.rs")));
        let example = package(
            "cfg-test-example",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"e\"\n[[example]]\nname = \"x\"\ntest = false\n",
                ),
                ("src/lib.rs", "#[cfg(test)]\nmod tests;\nmod always;\n"),
                ("src/tests.rs", ""),
                ("src/always.rs", ""),
            ],
        );
        assert!(!reached(&example, Path::new("src/tests.rs")));
        assert!(reached(&example, Path::new("src/always.rs")));
    }
}
