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
//! A `#[path = "…"]` declaration reaches the file its literal names relative to the declaring
//! file's directory, when that file sits in the same directory as the target.
//!
//! Ceilings, each answered "unreached" (a redundant notice, never a false clean): any other
//! `#[path]` declaration (another directory, `..`, a literal continued on a later line, one inside
//! an inline `mod` block); a `mod` gated by any `cfg` or `cfg_attr`, `#[cfg(test)]` included, since
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

/// Reason reported for a file no `mod` declaration reaches from a build target at all.
pub const UNREACHED: &str = "rust check may not have compiled this file — no unconditional `mod` declaration reaches it; declare it, then edit again";

/// Reason reported for a file reached only through a `cfg`/`cfg_attr`/`path` gate: an inner
/// `#![cfg]` (an integration test behind `feature = "…"`) or a gated `mod` on its chain. Declaring
/// it again would not help; a build that meets the condition does.
pub const GATED: &str = "rust check may not have compiled this file — it is built only under a `cfg` condition (feature, platform or test) or through a `#[path]` module; check it with a build or ide.test command that enables it";

/// Reports whether the worktree-relative `path` is reached from a build target of its nearest
/// package (see [`unreached`]).
#[cfg(test)]
fn reached(worktree: &Path, path: &Path) -> bool {
    unreached(worktree, path).is_none()
}

/// Returns why the worktree-relative `path` may not be compiled — [`GATED`] when a `cfg`/`path`
/// gate stands on its chain, [`UNREACHED`] otherwise — or `None` when a build target of its nearest
/// package reaches it; a file under no package manifest inside `worktree` counts as reached. The
/// edited file itself and every file on its chain must be readable and free of inner `cfg`
/// attributes.
pub fn unreached(worktree: &Path, path: &Path) -> Option<&'static str> {
    let file = worktree.join(path);
    let package = file
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(worktree))
        .find(|dir| dir.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)?;
    match code(&file) {
        None => return Some(UNREACHED),
        Some(text) if conditional_file(&text) => return Some(GATED),
        Some(_) => {}
    }
    let manifest = std::fs::read_to_string(package.join("Cargo.toml")).unwrap_or_default();
    let mut current = file;
    'walk: for _ in 0..MAX_STEPS {
        if is_root(&package, &manifest, &current) {
            return None;
        }
        let (name, declarers) = parent_module(&package, &manifest, &current)?;
        // Files beside `current` may name it through `#[path = "…"]` only, never by name.
        let mut siblings: Vec<PathBuf> = current
            .parent()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path != &current
                    && path.extension().is_some_and(|extension| extension == "rs")
                    && !declarers.contains(path)
                    && path.is_file()
            })
            .collect();
        siblings.sort();
        let candidates = declarers
            .into_iter()
            .map(|declarer| (declarer, Some(name.as_str())))
            .chain(siblings.into_iter().map(|sibling| (sibling, None)));
        let mut gated = false;
        for (declarer, by_name) in candidates {
            match declares(&declarer, by_name, &current) {
                Declared::Yes => {
                    current = declarer;
                    continue 'walk;
                }
                Declared::Gated => gated = true,
                Declared::No => {}
            }
        }
        return Some(if gated { GATED } else { UNREACHED });
    }
    Some(UNREACHED)
}

/// Whether one file declares a module: unconditionally, only behind a gate, or not at all.
enum Declared {
    /// An unconditional `mod name;` in an unconditional file.
    Yes,
    /// `mod name;` is there, but behind a `cfg`/`cfg_attr`/`path` attribute or in a file with an
    /// inner `#![cfg]`.
    Gated,
    /// No `mod name;` at all, or the file is unreadable or too large.
    No,
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

/// Reports whether `file` declares `target`: by `mod name;` (when `name` is given) or by a
/// `#[path = "…"] mod x;` whose literal, relative to `file`'s directory, is `target`. Any
/// visibility, with outer attributes on the same or preceding lines (blank and comment lines
/// between them keep them in force) — an attribute continued over several lines is judged by its
/// first line. A declaration under a `cfg`/`cfg_attr` attribute, a by-name declaration redirected
/// by `path`, or any declaration in a file that is conditional as a whole ([`conditional_file`])
/// is [`Declared::Gated`]; an unreadable or too large file declares nothing.
fn declares(file: &Path, name: Option<&str>, target: &Path) -> Declared {
    let Some((raw, text)) = source(file) else {
        return Declared::No;
    };
    let conditional = conditional_file(&text);
    let dir = file.parent().unwrap_or(Path::new(""));
    let mut found_gated = false;
    // A `cfg`/`cfg_attr` attribute read so far for the next item makes it conditional.
    let mut unproven = false;
    // A `path` attribute read so far for the next item: its literal when on the attribute's line.
    let mut redirect: Option<Option<String>> = None;
    // Bracket depth of an attribute continued from an earlier line; zero outside one.
    let mut depth = 0;
    for (line, raw_line) in text.lines().zip(raw.lines()) {
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
            unproven |= head.starts_with("cfg");
            if head.starts_with("path") {
                redirect = Some(path_literal(raw_line));
            }
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
        let redirect = redirect.take();
        let Some(declaration) = without_visibility(rest).strip_prefix("mod ") else {
            continue;
        };
        let Some((ident, _)) = declaration.split_once(';') else {
            continue;
        };
        let ident = ident.trim();
        let named = name.is_some_and(|name| ident.strip_prefix("r#").unwrap_or(ident) == name);
        let declared = match &redirect {
            None => named,
            Some(Some(literal)) => dir.join(literal) == target,
            Some(None) => false,
        };
        if declared && !gated && !conditional {
            return Declared::Yes;
        }
        // A by-name declaration redirected elsewhere leaves the default file unproven, too.
        found_gated |= declared || named;
    }
    if found_gated {
        Declared::Gated
    } else {
        Declared::No
    }
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
    source(file).map(|(_, code)| code)
}

/// `file`'s text and its [`code`] view, line for line.
fn source(file: &Path) -> Option<(String, String)> {
    if std::fs::metadata(file).map_or(true, |meta| meta.len() > MAX_FILE_BYTES) {
        return None;
    }
    let raw = std::fs::read_to_string(file).ok()?;
    let text: Vec<char> = raw.chars().collect();
    Some((raw, blanked_code(&text).into_iter().collect()))
}

/// The literal of the first `path = "…"` on one source line (no escapes), `None` when the line
/// holds none — an attribute whose literal continues on a later line stays unproven.
fn path_literal(line: &str) -> Option<String> {
    line.match_indices("path").find_map(|(index, _)| {
        let rest = line[index + 4..].trim_start().strip_prefix('=')?;
        let literal = rest.trim_start().strip_prefix('"')?;
        literal
            .split_once('"')
            .map(|(literal, _)| literal.to_owned())
    })
}

/// `text` as code only: every comment and string or char literal replaced by spaces, every line
/// break and every other character kept, so offsets and line numbers stay those of the source.
/// Shared by the module graph and the lexical outline ([`crate::lexical`]).
pub(crate) fn blanked_code(text: &[char]) -> Vec<char> {
    let mut code = text.to_vec();
    let mut index = 0;
    while index < text.len() {
        match blank_end(text, index) {
            Some(end) => {
                for position in &mut code[index..end] {
                    if *position != '\n' {
                        *position = ' ';
                    }
                }
                index = end;
            }
            None => index += 1,
        }
    }
    code
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

    /// A `#[path]` declaration reaches the file its literal names beside the declaring file (a
    /// `./` prefix allowed, other attributes on the line too); the default-named file it shadows,
    /// a `cfg`-gated one and a by-name `mod` in an unrelated sibling do not. A manifest target
    /// `path` is a root.
    #[test]
    fn path_attributes_reach_their_literal_and_manifest_paths_are_roots() {
        let root = package(
            "path-attribute",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"p\"\n[[bin]]\nname = \"extra\"\npath = \"tools/extra.rs\"\n",
                ),
                (
                    "src/lib.rs",
                    "#[path = \"elsewhere.rs\"]\nmod shadowed;\n#[cfg(windows)] #[path = \"gated.rs\"] mod gated_one;\nmod sub;\n",
                ),
                (
                    "src/sub.rs",
                    "#[allow(dead_code)] #[path = \"./also.rs\"] mod also;\nmod plain;\n",
                ),
                ("src/also.rs", ""),
                ("src/sub/plain.rs", "mod same_name;\n"),
                ("src/sub/same_name.rs", ""),
                ("src/gated.rs", ""),
                ("src/shadowed.rs", ""),
                ("src/elsewhere.rs", ""),
                ("tools/extra.rs", "mod util;\n"),
                ("tools/util.rs", ""),
            ],
        );
        assert!(!reached(&root, Path::new("src/shadowed.rs")));
        assert!(reached(&root, Path::new("src/elsewhere.rs")));
        assert!(reached(&root, Path::new("src/also.rs")));
        assert!(reached(&root, Path::new("src/sub/plain.rs")));
        assert!(!reached(&root, Path::new("src/gated.rs")));
        assert!(!reached(&root, Path::new("src/sub/same_name.rs")));
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

    /// A gate names itself: a feature-gated integration test, a module under it and a `cfg`-gated
    /// `mod` answer [`GATED`], never the "declare it" advice; an undeclared file keeps
    /// [`UNREACHED`].
    #[test]
    fn gated_files_are_told_apart_from_undeclared_ones() {
        let root = package(
            "gated-reason",
            &[
                ("Cargo.toml", "[package]\nname = \"g\"\n"),
                ("src/lib.rs", "#[cfg(windows)]\nmod win;\n"),
                ("src/win.rs", ""),
                ("src/orphan.rs", ""),
                (
                    "tests/fake_engine.rs",
                    "#![cfg(feature = \"test-fixtures\")]\nmod common;\n",
                ),
                ("tests/common/mod.rs", ""),
            ],
        );
        for gated in ["tests/fake_engine.rs", "tests/common/mod.rs", "src/win.rs"] {
            assert_eq!(unreached(&root, Path::new(gated)), Some(GATED), "{gated}");
        }
        assert_eq!(
            unreached(&root, Path::new("src/orphan.rs")),
            Some(UNREACHED)
        );
        assert_eq!(unreached(&root, Path::new("src/lib.rs")), None);
    }
}
