//! Operator structure builder: real source adapters, filters, Git states and bounded reads.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_ide::project::structure::*;

/// Owns one isolated fixture under the explicitly prepared project temporary directory.
struct Fixture(PathBuf);
static NEXT: AtomicU64 = AtomicU64::new(0);

impl Fixture {
    /// Creates an empty directory without using the developer's project or Git configuration.
    fn new() -> Self {
        agent_ide::languages::install();
        let path = std::env::temp_dir().join(format!(
            "structure-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    /// Writes a fixture file, creating only its parent directories.
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    /// Runs Git only inside this fixture with explicit local identity and disabled signing/hooks.
    fn git(&self, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .current_dir(&self.0)
            .args([
                "-c",
                "user.name=Structure Test",
                "-c",
                "user.email=structure@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    /// Seeds a private Git repository for stage/worktree comparisons.
    fn init(&self) {
        self.git(&["init", "-q"]);
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "baseline"]);
    }
    /// Builds the fixture using the real public entrypoint.
    fn build(&self, options: &StructureOptions) -> ProjectStructure {
        build_structure(&self.0, options).unwrap()
    }
}
impl Drop for Fixture {
    /// Removes only this test's owned fixture.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Finds one exact relative node without parsing its rendered presentation.
fn find<'a>(node: &'a StructureNode, path: &str) -> Option<&'a StructureNode> {
    if node.path == Path::new(path) {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, path))
}

/// Real adapters return source docs, individual compact imports and declarations without bodies.
#[test]
fn structure_source_details_are_real_and_optional() {
    let f = Fixture::new();
    f.write("src/lib.rs", "//! Pricing entry points.\n//! No inferred description.\n\nuse std::path::Path;\nuse std::{\n    fmt,\n    io,\n};\n\npub fn answer(_path: &Path) -> u32 { 123456 }\n");
    let bare = f.build(&StructureOptions {
        respect_gitignore: false,
        ..Default::default()
    });
    assert_eq!(bare.summary.source_bytes_read, 0);
    assert_eq!(
        find(&bare.tree, "src/lib.rs").unwrap().header,
        Detail::Disabled
    );
    let options = StructureOptions {
        respect_gitignore: false,
        include_header: true,
        include_imports: true,
        include_skeleton: true,
        include_line_count: true,
        include_size: true,
        ..Default::default()
    };
    let tree = f.build(&options);
    let node = find(&tree.tree, "src/lib.rs").unwrap();
    let Detail::Available(header) = &node.header else {
        panic!("missing header")
    };
    assert!(header.text.starts_with("Pricing"));
    assert!(!header.text.contains("use std"));
    let Detail::Available(imports) = &node.imports else {
        panic!("missing imports")
    };
    assert_eq!(imports.statements.len(), 2);
    assert_eq!(imports.statements[1].text, "use std::{ fmt, io, };");
    assert_eq!(
        (
            imports.statements[1].start_line,
            imports.statements[1].end_line
        ),
        (5, 8)
    );
    let Detail::Available(skeleton) = &node.skeleton else {
        panic!("missing skeleton")
    };
    assert!(skeleton.text.contains("pub fn answer"));
    assert!(
        skeleton
            .text
            .lines()
            .next()
            .unwrap()
            .starts_with("10  pub fn answer")
    );
    assert!(!skeleton.text.contains("123456"));
    assert!(!skeleton.text.contains("lib.rs  (10 lines, rust)"));
    assert!(node.source_hash.is_some());
    assert_eq!(node.line_count, Some(10));
    assert_eq!(
        tree.summary.source_bytes_read as u64,
        node.size_bytes.unwrap()
    );
    let rendered = render_tree(&tree);
    assert!(
        !rendered.contains("//!"),
        "header comment markers should not be rendered"
    );
    assert!(rendered.contains("Pricing entry points."));
    assert!(
        !rendered.contains("lib.rs  (10 lines, rust)"),
        "duplicate skeleton heading"
    );
    assert!(rendered.find("header L").unwrap() < rendered.find("imports:").unwrap());
    assert!(rendered.find("imports:").unwrap() < rendered.find("skeleton:").unwrap());
    let encoded = serde_json::to_string(&tree).unwrap();
    assert!(!encoded.contains("//!"));
    assert!(!encoded.contains("lib.rs  (10 lines, rust)"));
    assert!(encoded.find("\"header\"").unwrap() < encoded.find("\"imports\"").unwrap());
    assert!(encoded.find("\"imports\"").unwrap() < encoded.find("\"skeleton\"").unwrap());
    let decoded: ProjectStructure = serde_json::from_str(&encoded).unwrap();
    assert_eq!(render_tree(&tree), render_tree(&decoded));
}

/// Independent switches preserve root-relative paths, exact exclusions and collapsed branches.
#[test]
fn structure_depth_expansion_filters_and_symlinks() {
    let f = Fixture::new();
    f.write("a/src/lib.rs", "pub fn a() {}\n");
    f.write("b/other.rs", "pub fn b() {}\n");
    f.write(".hidden/config.rs", "");
    f.write("target/large.rs", "");
    f.write("a/Cargo.toml", "");
    std::os::unix::fs::symlink(&f.0, f.0.join("loop")).unwrap();
    let tree = f.build(&StructureOptions {
        respect_gitignore: false,
        max_depth: 4,
        expand_paths: vec!["a".into()],
        include_hidden: false,
        extensions: vec!["rs".into()],
        ..Default::default()
    });
    assert!(find(&tree.tree, "a/src/lib.rs").is_some());
    assert!(find(&tree.tree, "a/Cargo.toml").is_none());
    assert!(find(&tree.tree, "b").unwrap().collapsed);
    assert!(find(&tree.tree, "b/other.rs").is_none());
    assert!(find(&tree.tree, ".hidden").is_none());
    assert!(find(&tree.tree, "target").is_none());
    let links = f.build(&StructureOptions {
        respect_gitignore: false,
        max_depth: 8,
        ..Default::default()
    });
    assert_eq!(find(&links.tree, "loop").unwrap().kind, NodeKind::Symlink);
    assert!(find(&links.tree, "loop").unwrap().children.is_empty());
    let shallow = f.build(&StructureOptions {
        respect_gitignore: false,
        max_depth: 1,
        include_files: false,
        ..Default::default()
    });
    assert!(find(&shallow.tree, "a").unwrap().collapsed);
    assert!(find(&shallow.tree, "a/src").is_none());
}

/// Limits have explicit states and malformed options fail before returning a misleading tree.
#[test]
fn structure_limits_and_missing_details_are_explicit() {
    let f = Fixture::new();
    f.write(
        "large.rs",
        "//! This source exceeds the tiny test budget.\npub fn x() {}\n",
    );
    f.write("note.bin", "x");
    let tree = f.build(&StructureOptions {
        respect_gitignore: false,
        include_header: true,
        include_imports: true,
        include_skeleton: true,
        max_file_bytes: 8,
        ..Default::default()
    });
    assert!(matches!(
        find(&tree.tree, "large.rs").unwrap().header,
        Detail::Unavailable { .. }
    ));
    assert!(matches!(
        find(&tree.tree, "note.bin").unwrap().skeleton,
        Detail::Unavailable { .. }
    ));
    assert!(!tree.warnings.is_empty());
    let limited = f.build(&StructureOptions {
        respect_gitignore: false,
        max_nodes: 1,
        ..Default::default()
    });
    assert!(limited.summary.truncated);
    assert!(limited.tree.children.is_empty());
    for options in [
        StructureOptions {
            max_nodes: 0,
            ..Default::default()
        },
        StructureOptions {
            expand_paths: vec!["../escape".into()],
            ..Default::default()
        },
    ] {
        assert!(build_structure(&f.0, &options).is_err());
    }
    assert!(build_structure(&f.0.join("large.rs"), &Default::default()).is_err());
    assert!(
        build_structure(
            &f.0,
            &StructureOptions {
                changed_only: true,
                ..Default::default()
            }
        )
        .is_err()
    );
}

/// Separate index/worktree counts keep a staged change even when the working bytes match HEAD.
#[test]
fn structure_changed_only_keeps_both_diff_sides_deleted_and_untracked() {
    let f = Fixture::new();
    f.write("src/a.rs", "pub fn a() -> u8 { 1 }\n");
    f.write("gone/sub/deleted.rs", "old\n");
    f.write("clean.rs", "unchanged\n");
    f.init();
    f.write("src/a.rs", "pub fn a() -> u8 { 2 }\n");
    f.git(&["add", "src/a.rs"]);
    f.write("src/a.rs", "pub fn a() -> u8 { 1 }\n");
    fs::remove_file(f.0.join("gone/sub/deleted.rs")).unwrap();
    fs::remove_dir_all(f.0.join("gone")).unwrap();
    f.write("new/added.rs", "pub fn added() {}\n");
    let options = StructureOptions {
        max_depth: 8,
        changed_only: true,
        include_diff: true,
        ..Default::default()
    };
    let tree = f.build(&options);
    assert!(find(&tree.tree, "clean.rs").is_none());
    assert!(find(&tree.tree, "src/a.rs").is_some());
    let Detail::Available(diff) = &find(&tree.tree, "src/a.rs").unwrap().diff else {
        panic!("no diff")
    };
    assert_eq!(diff.staged.added_lines, Some(1));
    assert_eq!(diff.staged.removed_lines, Some(1));
    assert_eq!(diff.staged.hunks, Some(1));
    assert_eq!(diff.unstaged.added_lines, Some(1));
    assert_eq!(diff.unstaged.hunks, Some(1));
    assert_eq!(
        find(&tree.tree, "gone/sub/deleted.rs").unwrap().kind,
        NodeKind::Deleted
    );
    let Detail::Available(new) = &find(&tree.tree, "new/added.rs").unwrap().diff else {
        panic!("no new diff")
    };
    assert_eq!(new.status, ChangeStatus::Untracked);
    assert_eq!(new.unstaged.added_lines, None);
}

/// NUL-safe Git names, subdirectory roots, ignored files and renamed paths retain their identity.
#[test]
fn structure_git_subroot_rename_binary_and_ignore() {
    let f = Fixture::new();
    f.write("part/original.rs", "pub fn old() {}\n");
    f.write("part/clean.rs", "clean\n");
    f.write(".gitignore", "*.ignored\n");
    f.init();
    f.git(&["mv", "part/original.rs", "part/renamed.rs"]);
    f.write("part/x.ignored", "secret");
    f.write("part/name\twith\ncontrols.rs", "pub fn x() {}\n");
    fs::write(f.0.join("part/blob.bin"), b"\0old\0").unwrap();
    f.git(&["add", "part/blob.bin"]);
    fs::write(f.0.join("part/blob.bin"), b"\0new\0").unwrap();
    let tree = build_structure(
        &f.0.join("part"),
        &StructureOptions {
            changed_only: true,
            include_diff: true,
            max_depth: 8,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(find(&tree.tree, "renamed.rs").is_some());
    assert!(find(&tree.tree, "part/renamed.rs").is_none());
    assert!(find(&tree.tree, "name\twith\ncontrols.rs").is_some());
    assert!(find(&tree.tree, "x.ignored").is_none());
    assert!(find(&tree.tree, "clean.rs").is_none());
    assert!(!render_tree(&tree).contains("with\ncontrols"));
    let Detail::Available(binary) = &find(&tree.tree, "blob.bin").unwrap().diff else {
        panic!("no binary diff")
    };
    assert_eq!(binary.unstaged.added_lines, None);
    let full = f.build(&StructureOptions {
        max_depth: 4,
        ..Default::default()
    });
    assert!(find(&full.tree, "part/x.ignored").is_none());
}

/// A new Git repository has changes without a HEAD; no false no-changes fallback is allowed.
#[test]
fn structure_unborn_repository_and_source_limits() {
    let f = Fixture::new();
    f.git(&["init", "-q"]);
    f.write(
        "new.rs",
        "//! New.\nuse std::fmt;\nuse std::io;\npub fn a() {}\n",
    );
    f.git(&["add", "new.rs"]);
    let tree = f.build(&StructureOptions {
        changed_only: true,
        include_diff: true,
        include_imports: true,
        max_imports: 1,
        ..Default::default()
    });
    assert!(tree.git.head.is_none());
    let node = find(&tree.tree, "new.rs").unwrap();
    let Detail::Available(diff) = &node.diff else {
        panic!("no staged addition")
    };
    assert_eq!(diff.staged.added_lines, Some(4));
    let Detail::Available(imports) = &node.imports else {
        panic!("no imports")
    };
    assert_eq!(imports.statements.len(), 1);
    assert!(imports.truncated);
    assert!(tree.summary.truncated);
    f.git(&["commit", "-qm", "initial"]);
    let clean = f.build(&StructureOptions {
        changed_only: true,
        ..Default::default()
    });
    assert!(clean.tree.children.is_empty());
}

/// Compact imports retain literal target names and remove comments without swallowing members.
#[test]
fn structure_compact_imports_preserve_quoted_paths_and_members() {
    let f = Fixture::new();
    f.write("imports.ts", "// Module imports.\nimport { readFile } from './a  b';\nimport {\n  /* member */ one, // explanation\n  two,\n} from './other';\nexport function work() {}\n");
    f.write("imports.rs", "//! Imports.\nuse std::{\n    /* outer /* nested */ done */ fmt, // member\n    io,\n};\npub fn work() {}\n");
    f.write("imports.py", "\"\"\"Path helpers.\"\"\"\nfrom os.path import \\\n    join, dirname\nimport os\nimport os\n");
    let tree = f.build(&StructureOptions {
        respect_gitignore: false,
        include_imports: true,
        ..Default::default()
    });
    let Detail::Available(imports) = &find(&tree.tree, "imports.ts").unwrap().imports else {
        panic!("no imports")
    };
    assert_eq!(imports.statements.len(), 2);
    assert!(imports.statements[0].text.contains("'./a  b'"));
    assert!(imports.statements[1].text.contains("one,"));
    assert!(imports.statements[1].text.contains("two,"));
    assert!(!imports.statements[1].text.contains("explanation"));
    let Detail::Available(imports) = &find(&tree.tree, "imports.py").unwrap().imports else {
        panic!("no imports")
    };
    assert_eq!(imports.statements.len(), 2);
    assert_eq!(
        imports.statements[0].text,
        "from os.path import join, dirname"
    );
    let Detail::Available(imports) = &find(&tree.tree, "imports.rs").unwrap().imports else {
        panic!("no imports")
    };
    assert_eq!(imports.statements[0].text, "use std::{ fmt, io, };");
}

/// Changed-only exclusions do not leave empty directory stubs or inflate shown-node counts.
#[test]
fn structure_changed_only_exclusions_prune_empty_ancestors() {
    let f = Fixture::new();
    f.write("src/a.rs", "pub fn a() {}\n");
    f.init();
    f.write("src/a.rs", "pub fn b() {}\n");
    let tree = f.build(&StructureOptions {
        changed_only: true,
        exclude_paths: vec!["src/a.rs".into()],
        ..Default::default()
    });
    assert!(tree.tree.children.is_empty());
    assert_eq!(tree.summary.directories_shown, 1);
    assert_eq!(tree.summary.files_shown, 0);
}

/// Metadata collection never runs configured clean/process filters or changes their settings.
#[test]
fn structure_git_never_executes_configured_content_filters() {
    let f = Fixture::new();
    f.write(".gitattributes", "*.rs filter=sentinel\n");
    f.write("code.rs", "pub fn old() {}\n");
    f.init();
    let marker = f.0.join("filter-invoked");
    let command = format!("/bin/sh -c 'echo invoked >> {}; cat'", marker.display());
    f.git(&["config", "filter.sentinel.clean", &command]);
    f.git(&["config", "filter.sentinel.process", &command]);
    f.git(&["config", "filter.sentinel.required", "true"]);
    f.write("code.rs", "pub fn new() {}\n");
    let tree = f.build(&StructureOptions {
        changed_only: true,
        include_diff: true,
        ..Default::default()
    });
    assert!(!marker.exists(), "the utility executed a configured filter");
    let Detail::Available(diff) = &find(&tree.tree, "code.rs").unwrap().diff else {
        panic!("no diff")
    };
    assert_eq!(diff.unstaged.added_lines, Some(1));
    assert_eq!(diff.unstaged.removed_lines, Some(1));
    let setting = Command::new("/usr/bin/git")
        .current_dir(&f.0)
        .args(["config", "--get", "filter.sentinel.required"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(setting.stdout).unwrap().trim(), "true");
}

/// Unsupported formats are normal metadata entries, not repeated warning/detail noise.
#[test]
fn structure_unsupported_formats_have_one_notice_and_no_warning_spam() {
    let f = Fixture::new();
    f.write("package.json", "{}\n");
    f.write("settings.toml", "key = true\n");
    f.write("README.md", "# Example\n");
    f.write(
        "large.rs",
        "//! A supported source that exceeds this test's byte budget.\npub fn run() {}\n",
    );
    f.init();
    f.git(&["config", "filter.unused.clean", "/usr/bin/cat"]);
    let tree = f.build(&StructureOptions {
        include_diff: true,
        respect_gitignore: false,
        include_header: true,
        include_imports: true,
        include_skeleton: true,
        include_line_count: true,
        include_size: true,
        max_file_bytes: 8,
        ..Default::default()
    });
    let text = render_tree(&tree);
    assert!(text.starts_with("Details: header/imports/skeleton"));
    assert_eq!(text.matches("Details: header/imports/skeleton").count(), 1);
    assert!(!text.contains("header: unavailable"));
    assert!(!text.contains("imports: unavailable"));
    assert!(!text.contains("skeleton: unavailable"));
    assert!(!text.contains("source language not registered"));
    assert!(text.contains("package.json [bytes:"));
    assert!(text.contains("README.md [bytes:"));
    assert_eq!(tree.warnings.len(), 2); // Source limit plus the separately rendered Git policy.
    assert!(text.contains("Git: unfiltered comparisons; external content filters disabled."));
    assert!(!text.contains("warning: Git content filters disabled"));
    assert!(
        text.contains("warning: large.rs: source details unavailable: source byte limit reached")
    );
    // Absence of a rendered block must not become a fabricated empty/available machine fact.
    assert!(matches!(
        find(&tree.tree, "package.json").unwrap().header,
        Detail::Unavailable { .. }
    ));
}

/// Compact rendering preserves directory depth; documentation is independent of skeletons.
#[test]
fn structure_compact_layout_and_skeleton_comments_are_independent() {
    let f = Fixture::new();
    f.write(
        "src/nested/lib.rs",
        "//! File purpose.\n\n/// Useful symbol explanation.\npub fn value() -> u32 { 1 }\n",
    );
    let options = StructureOptions {
        include_header: true,
        include_skeleton: true,
        ..Default::default()
    };
    let normal = render_tree(&f.build(&options));
    assert!(normal.contains("Useful symbol explanation."));
    assert!(normal.contains("\u{2514} src/"));
    assert!(!normal.contains("\u{2500}"));
    let tree = f.build(&StructureOptions {
        compact: true,
        include_skeleton_comments: false,
        ..options
    });
    let compact = render_tree(&tree);
    assert!(compact.contains("\n src/\n  nested/\n   lib.rs\n"));
    assert!(!compact.contains(['\u{2502}', '\u{251c}', '\u{2514}', '\u{2500}']));
    assert!(compact.contains("\n    header L"));
    assert!(compact.contains("\n     File purpose."));
    assert!(compact.contains("pub fn value"));
    let Detail::Available(skeleton) = &find(&tree.tree, "src/nested/lib.rs").unwrap().skeleton
    else {
        panic!("missing skeleton");
    };
    assert!(!skeleton.text.contains("Useful symbol explanation."));
}

/// Suppression is limited to confirmed zeros; missing Git observations never become clean facts.
#[test]
fn structure_git_render_omits_clean_counts_but_preserves_unknowns() {
    let f = Fixture::new();
    f.write("clean.txt", "baseline\n");
    f.init();
    let plain = render_tree(&f.build(&StructureOptions::default()));
    assert!(!plain.contains("Git:"));
    let mut tree = f.build(&StructureOptions {
        include_diff: true,
        ..Default::default()
    });
    let clean = render_tree(&tree);
    assert!(clean.contains("Git: omitted changes/counters are zero"));
    assert!(!clean.contains("Unchanged"));
    assert!(!clean.contains("staged +0/-0"));
    let child = tree
        .tree
        .children
        .iter_mut()
        .find(|n| n.path == Path::new("clean.txt"))
        .unwrap();
    let Detail::Available(diff) = &mut child.diff else {
        panic!("missing diff");
    };
    diff.unstaged.hunks = None;
    let unknown = render_tree(&tree);
    assert!(unknown.contains("unstaged +0/-0 (hunks: ?)"));
    tree.tree.children[0].diff = Detail::Unavailable {
        reason: "fixture unavailable".into(),
    };
    assert!(render_tree(&tree).contains("changes: unavailable"));
    f.write("clean.txt", "baseline\nnew\n");
    let changed = render_tree(&f.build(&StructureOptions {
        include_diff: true,
        ..Default::default()
    }));
    assert!(changed.contains("changes: Modified; unstaged +1/-0 (hunks: 1)"));
    assert!(!changed.contains("; staged"));
}

/// Source-only Python and stub files expose declarations, docs and imports without a server.
#[test]
fn structure_python_details_include_scoped_skeletons() {
    let f = Fixture::new();
    f.write("service.py", "\"\"\"Service facade.\"\"\"\nfrom pathlib import Path\n\nclass Service:\n    \"\"\"Runs service operations.\"\"\"\n    value: int = 3\n    async def run(self, path: Path) -> str:\n        \"\"\"Reads a resource.\"\"\"\n        local = str(path)\n        return local\n");
    f.write("api.pyi", "from typing import overload\n@overload\ndef load(x: int) -> int: ...\n@overload\ndef load(x: str) -> str: ...\n");
    let tree = f.build(&StructureOptions {
        include_header: true,
        include_imports: true,
        include_skeleton: true,
        ..Default::default()
    });
    let node = find(&tree.tree, "service.py").unwrap();
    let Detail::Available(skeleton) = &node.skeleton else {
        panic!("missing Python skeleton");
    };
    assert!(skeleton.text.contains("class Service"));
    assert!(skeleton.text.contains("value: int"));
    assert!(
        skeleton
            .text
            .contains("async def run(self, path: Path) -> str")
    );
    assert!(skeleton.text.contains("Reads a resource."));
    assert!(!skeleton.text.contains("local ="));
    let Detail::Available(imports) = &node.imports else {
        panic!("missing Python imports");
    };
    assert_eq!(imports.statements[0].text, "from pathlib import Path");
    let Detail::Available(stub) = &find(&tree.tree, "api.pyi").unwrap().skeleton else {
        panic!("missing stub skeleton");
    };
    assert!(stub.text.contains("def load(x: int) -> int"));
    assert!(stub.text.contains("def load(x: str) -> str"));
    let without = f.build(&StructureOptions {
        include_skeleton: true,
        include_skeleton_comments: false,
        ..Default::default()
    });
    let Detail::Available(skeleton) = &find(&without.tree, "service.py").unwrap().skeleton else {
        panic!("missing Python skeleton");
    };
    assert!(!skeleton.text.contains("Reads a resource."));
    assert!(skeleton.text.contains("async def run"));
    let encoded = serde_json::to_string(&tree).unwrap();
    assert_eq!(
        render_tree(&tree),
        render_tree(&serde_json::from_str(&encoded).unwrap())
    );
}

/// R3: a directory-only or empty non-Git root cannot imply a successful zero-change observation.
#[test]
fn structure_global_git_unavailable_is_visible_without_files() {
    let f = Fixture::new();
    for include_files in [false, true] {
        let tree = f.build(&StructureOptions {
            include_diff: true,
            include_files,
            respect_gitignore: false,
            ..Default::default()
        });
        let text = render_tree(&tree);
        assert!(text.contains("Git: unavailable"), "{text}");
        assert!(!text.contains("omitted changes/counters are zero"));
    }
}

/// R2: a changed gitlink is a change to the directory itself, not invented descendant changes.
#[test]
fn structure_changed_gitlink_keeps_its_own_diff() {
    let f = Fixture::new();
    f.write("baseline.txt", "parent\n");
    f.init();
    f.write("sub/value.txt", "child\n");
    let child = Fixture(f.0.join("sub"));
    child.init();
    let oid = Command::new("/usr/bin/git")
        .args(["-C"])
        .arg(&child.0)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let oid = String::from_utf8(oid.stdout).unwrap();
    f.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},sub", oid.trim()),
    ]);
    f.git(&["commit", "-qm", "record child"]);
    child.write("value.txt", "changed\n");
    child.git(&["add", "value.txt"]);
    child.git(&["commit", "-qm", "advance child"]);
    let tree = f.build(&StructureOptions {
        changed_only: true,
        include_diff: true,
        respect_gitignore: false,
        ..Default::default()
    });
    let sub = find(&tree.tree, "sub").expect("dirty gitlink retained");
    assert_eq!(sub.kind, NodeKind::Directory);
    assert!(sub.children.is_empty());
    assert!(matches!(&sub.diff, Detail::Available(d) if d.status == ChangeStatus::Modified));
    assert!(render_tree(&tree).contains("sub/ [changes: Modified"));
}

/// R4: imports inside literal text, nested templates, regexes and raw strings are not source imports.
#[test]
fn structure_imports_are_backed_by_code_tokens() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "TEXT = \"\"\"\nimport invented\n\"\"\"\nimport real\n",
    );
    f.write(
        "b.ts",
        "const t = `start ${ `\nimport invented from 'bad';\n` } end`;\nimport real from 'good';\n",
    );
    f.write("c.rs", "const TEXT: &str = r###\"\nuse invented::bad;\n\"###;\nfn borrow<'a>(x: &'a str) {}\nuse real::good;\n");
    f.write(
        "d.css",
        "a { content: \"first\\\n@import 'bad';\\\nlast\"; }\n@import 'good';\n",
    );
    let tree = f.build(&StructureOptions {
        include_imports: true,
        respect_gitignore: false,
        ..Default::default()
    });
    for path in ["a.py", "b.ts", "c.rs", "d.css"] {
        let Detail::Available(imports) = &find(&tree.tree, path).unwrap().imports else {
            panic!("missing real import {path}");
        };
        assert_eq!(imports.statements.len(), 1, "{path}: {imports:?}");
        assert!(
            !imports.statements[0].text.contains("invented")
                && !imports.statements[0].text.contains("bad")
        );
    }
}

/// R7: an over-budget directory is omitted as a whole rather than an arbitrary filtered prefix.
#[test]
fn structure_directory_overflow_does_not_expose_an_arbitrary_subset() {
    let f = Fixture::new();
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        f.write(name, "pub fn f() {}\n");
    }
    let tree = f.build(&StructureOptions {
        max_nodes: 3,
        respect_gitignore: false,
        ..Default::default()
    });
    assert!(tree.summary.truncated);
    assert!(tree.tree.truncated);
    assert!(tree.tree.children.is_empty());
    let order: Vec<_> = fs::read_dir(&f.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let tree = f.build(&StructureOptions {
        max_nodes: 3,
        exclude_paths: order[..3].iter().map(PathBuf::from).collect(),
        respect_gitignore: false,
        ..Default::default()
    });
    assert!(tree.summary.truncated);
    assert!(tree.tree.children.is_empty());
}

/// R8/R9: parenthesized docs remain visible; budget exhaustion is unavailable, never absent docs.
#[test]
fn structure_doc_parentheses_and_doc_budget_are_honest() {
    let f = Fixture::new();
    f.write(
        "parent.py",
        "def f():\n    (\"Useful \"\n     \"documentation.\")\n    pass\n",
    );
    f.write(
        "large.py",
        &format!("def f():\n    {}\n    pass\n", "'x' ".repeat(20_001)),
    );
    let tree = f.build(&StructureOptions {
        include_skeleton: true,
        respect_gitignore: false,
        ..Default::default()
    });
    let Detail::Available(sk) = &find(&tree.tree, "parent.py").unwrap().skeleton else {
        panic!("missing parenthesized docs");
    };
    assert!(sk.text.contains("Useful documentation."));
    assert!(matches!(
        &find(&tree.tree, "large.py").unwrap().skeleton,
        Detail::Unavailable { .. }
    ));
    assert!(
        tree.warnings
            .iter()
            .any(|w| w.path.as_deref() == Some(Path::new("large.py"))
                && w.reason.contains("source-only outline unavailable"))
    );
}

/// R10: coarse physical ranges merge before joins, preserving distinct adjacent import lines.
#[test]
fn structure_same_line_imports_are_materialized_once() {
    let f = Fixture::new();
    let line = (0..128)
        .map(|i| format!("import a{i}"))
        .collect::<Vec<_>>()
        .join("; ");
    let source = format!(
        "{line} # {}\nfrom a import (\n    b); import c\nimport next\n",
        "x".repeat(16_384)
    );
    f.write("imports.py", &source);
    let spans = agent_ide::languages::PYTHON
        .support()
        .source_import_spans(Path::new("imports.py"), &source)
        .unwrap();
    assert_eq!(spans.len(), 3);
    let tree = f.build(&StructureOptions {
        include_imports: true,
        respect_gitignore: false,
        ..Default::default()
    });
    let Detail::Available(imports) = &find(&tree.tree, "imports.py").unwrap().imports else {
        panic!("missing imports");
    };
    assert_eq!(imports.statements.len(), 3);
    assert_eq!(
        (
            imports.statements[0].start_line,
            imports.statements[0].end_line
        ),
        (1, 1)
    );
    assert!(imports.statements[0].text.contains("import a127"));
    assert_eq!(
        (
            imports.statements[1].start_line,
            imports.statements[1].end_line
        ),
        (2, 3)
    );
    assert_eq!(
        (
            imports.statements[2].start_line,
            imports.statements[2].end_line
        ),
        (4, 4)
    );
}

/// Parser scheduling cannot change byte-budget winners, warnings, hashes or rendered order.
#[test]
fn structure_parallel_workers_preserve_serial_results_and_limits() {
    let f = Fixture::new();
    for n in 0..24 {
        f.write(&format!("pkg/f{n:02}.py"),
            "\"\"\"Module docs.\"\"\"\nimport os\nclass Worker:\n    def run(self, x=(1,)):\n        return x\n");
    }
    f.write("a_bad.py", "def broken(:\n");
    f.write("b_bad.rs", "pub fn broken( {\n");
    f.write("c_empty.py", "");
    f.write("d.md", "metadata only");
    for budget in [64, 256, 4096] {
        let mut options = StructureOptions {
            max_depth: 8,
            respect_gitignore: false,
            include_header: true,
            include_imports: true,
            include_skeleton: true,
            include_line_count: true,
            include_size: true,
            max_total_read_bytes: budget,
            workers: 1,
            ..Default::default()
        };
        let serial = f.build(&options);
        let expected_text = render_tree(&serial);
        let mut expected = serde_json::to_value(&serial).unwrap();
        expected["options"]["workers"] = serde_json::json!(0);
        for workers in [2, 4, 16] {
            options.workers = workers;
            let parallel = f.build(&options);
            assert_eq!(
                render_tree(&parallel),
                expected_text,
                "budget={budget}, workers={workers}"
            );
            let mut actual = serde_json::to_value(&parallel).unwrap();
            actual["options"]["workers"] = serde_json::json!(0);
            assert_eq!(actual, expected, "budget={budget}, workers={workers}");
        }
    }
    for workers in [0, 17] {
        assert!(
            build_structure(
                &f.0,
                &StructureOptions {
                    workers,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}

/// Shared Python parse keeps imports available when outline-specific representation refuses.
#[test]
fn structure_combined_python_consumers_remain_independent() {
    let f = Fixture::new();
    f.write(
        "module.py",
        "import os\ndef run(x=\"\"\"one\ntwo\"\"\"):\n    pass\n",
    );
    let options = StructureOptions {
        respect_gitignore: false,
        workers: 1,
        include_imports: true,
        include_skeleton: true,
        ..Default::default()
    };
    let both = f.build(&options);
    let node = find(&both.tree, "module.py").unwrap();
    assert!(matches!(node.imports, Detail::Available(_)));
    assert!(matches!(node.skeleton, Detail::Unavailable { .. }));
    let imports = f.build(&StructureOptions {
        include_skeleton: false,
        ..options.clone()
    });
    assert_eq!(
        node.imports,
        find(&imports.tree, "module.py").unwrap().imports
    );
    let skeleton = f.build(&StructureOptions {
        include_imports: false,
        ..options
    });
    assert_eq!(
        node.skeleton,
        find(&skeleton.tree, "module.py").unwrap().skeleton
    );
}
