//! The cross-language linkage join over the bundled front-end languages (HTML, CSS, TS/JS):
//! the existing class/id/style-variable bridge, the first-release `file-ref/v1` and CSS-module
//! member kinds, and the equality of a language computed in its module with the same language in
//! process. Positive, negative (dynamic, templated, unresolved, escaping) and invalidation cases;
//! the real-daemon rows live in `module_web_parity.rs`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use agent_ide::{
    intelligence::{
        anchors::AnchorSource,
        names::{IndexState, NameIndex},
    },
    lang::{
        Language,
        names::{Certainty, NameKey, Role, ns},
    },
    languages::{CSS, HTML, TYPESCRIPT},
};
use agent_ide_core::{
    modules::{
        adapter::SupportServer,
        contract::{Capability, ModuleUnavailable},
        payload::{
            AnalyzeSource, AnchorBatch, Field, SourceAnalysis, SourceField, SourceRef, SourceText,
            decode, encode,
        },
        serve::Incoming,
    },
    workspace::authority::WorktreeRef,
};

/// A scratch worktree removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// A fresh canonical scratch directory.
fn scratch(tag: &str) -> Scratch {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "agent-ide-linkage-{tag}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    Scratch(std::fs::canonicalize(root).unwrap())
}

/// Writes `files` below `root`.
fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

/// An index over `root`, optionally taking `routed` languages from the module stand-in.
fn index(root: &Path, routed: &[Language]) -> NameIndex {
    agent_ide::languages::install();
    let worktree = WorktreeRef::from_discovery(root.into(), root.into(), ".git".into(), 1).unwrap();
    let mut index = NameIndex::new(worktree);
    if !routed.is_empty() {
        index = index.with_anchor_source(Arc::new(Module(routed.to_vec())));
    }
    assert_eq!(
        index.refresh(Instant::now() + Duration::from_secs(60)),
        IndexState::Ready
    );
    index
}

/// Refreshes `index` to completion.
fn refresh(index: &mut NameIndex) {
    assert_eq!(
        index.refresh(Instant::now() + Duration::from_secs(60)),
        IndexState::Ready
    );
}

/// A module stand-in: the real support adapter answers `analyze_source` anchors for the routed
/// languages, exactly as `agent-ide module <language>` would.
struct Module(Vec<Language>);

impl AnchorSource for Module {
    fn routes(&self, language: Language) -> bool {
        self.0.contains(&language)
    }

    fn anchors(
        &self,
        _worktree: &Path,
        language: Language,
        path: &Path,
        text: &str,
    ) -> Result<AnchorBatch, ModuleUnavailable> {
        let incoming = Incoming {
            fence: Default::default(),
            capability: Capability::AnalyzeSource,
            budget_ms: 1000,
            payload: encode(&AnalyzeSource {
                source: SourceRef {
                    path: path.into(),
                    revision: "r".into(),
                    text: SourceText::Inline(text.into()),
                },
                fields: vec![SourceField::Anchors],
            }),
            attachments: Vec::new(),
        };
        let value = SupportServer::new(language, "1").answer(&incoming).unwrap();
        match decode::<SourceAnalysis>(value).unwrap().anchors {
            Field::Available(batch) => Ok(batch),
            other => panic!("{other:?}"),
        }
    }
}

/// A global key.
fn key(namespace: agent_ide::lang::names::Namespace, name: &str) -> NameKey {
    NameKey::global(namespace, name)
}

/// `(file, line, role)` of every site of `key`.
fn sites(index: &NameIndex, key: &NameKey) -> Vec<(String, u32, Role)> {
    index
        .sites(key)
        .iter()
        .map(|site| {
            (
                site.file.display().to_string(),
                site.fact.line,
                site.fact.role,
            )
        })
        .collect()
}

/// The front-end fixture every join test reads.
fn fixture(root: &Path) {
    write(
        root,
        &[
            (
                "web/index.html",
                "<link rel=\"stylesheet\" href=\"css/site.css\">\n\
                 <script src=\"js/app.js\"></script>\n\
                 <main id=\"app\" class=\"card btn\"><a href=\"#app\">x</a></main>\n",
            ),
            (
                "web/css/site.css",
                ":root { --brand: red; }\n.btn { color: var(--brand); }\n.card {}\n#app {}\n",
            ),
            (
                "web/js/app.js",
                "import { run } from './lib/run';\n\
                 import './polyfill.js';\n\
                 document.getElementById('app').className = 'btn';\n",
            ),
            ("web/js/lib/run.ts", "export function run() {}\n"),
            ("web/js/polyfill.ts", "export {};\n"),
            (
                "web/ui/Button.tsx",
                "import styles from './Button.module.css';\n\
                 export const B = () => <b className={styles.primary} />;\n",
            ),
            (
                "web/ui/Button.module.css",
                ".primary { color: blue; }\n.other {}\n",
            ),
            ("web/ui/Card.module.css", ".primary { color: green; }\n"),
        ],
    );
}

/// The existing bridge joins HTML, CSS and scripts by namespace, domain and name exactly as at
/// 0.10.8: classes, ids, style variables, DOM queries, and a CSS module's classes stay in their
/// file's domain (a global `.primary` never meets `styles.primary`).
#[test]
fn the_bridge_joins_by_namespace_domain_and_name() {
    let root = scratch("bridge");
    fixture(&root);
    let index = index(&root, &[]);
    assert_eq!(
        sites(&index, &key(ns::CLASS, "btn")),
        [
            ("web/css/site.css".to_owned(), 2, Role::Define),
            ("web/index.html".to_owned(), 3, Role::Use),
            ("web/js/app.js".to_owned(), 3, Role::Use),
        ]
    );
    assert_eq!(
        sites(&index, &key(ns::ELEMENT_ID, "app")),
        [
            ("web/css/site.css".to_owned(), 4, Role::Use),
            ("web/index.html".to_owned(), 3, Role::Define),
            ("web/index.html".to_owned(), 3, Role::Use),
            ("web/js/app.js".to_owned(), 3, Role::Use),
        ]
    );
    assert_eq!(
        sites(&index, &key(ns::STYLE_VARIABLE, "brand")),
        [
            ("web/css/site.css".to_owned(), 1, Role::Define),
            ("web/css/site.css".to_owned(), 2, Role::Use),
        ]
    );
    let module = |name: &str, path: &str| NameKey {
        domain: path.into(),
        ..key(ns::CLASS, name)
    };
    assert_eq!(
        sites(&index, &module("primary", "web/ui/Button.module.css")),
        [
            ("web/ui/Button.module.css".to_owned(), 1, Role::Define),
            ("web/ui/Button.tsx".to_owned(), 2, Role::Use),
        ]
    );
    // The same class name in another module, and the global namespace, stay apart.
    assert_eq!(
        sites(&index, &module("primary", "web/ui/Card.module.css")),
        [("web/ui/Card.module.css".to_owned(), 1, Role::Define)]
    );
    assert!(sites(&index, &key(ns::CLASS, "primary")).is_empty());
}

/// Identical module bytes at two paths are two extractions: each file's classes carry their own
/// path as domain even when the content cache already holds the other file's facts.
#[test]
fn identical_module_bytes_at_two_paths_keep_their_own_domains() {
    let root = scratch("domains");
    write(
        &root,
        &[
            ("a/x.module.css", ".same { color: red; }\n"),
            ("b/x.module.css", ".same { color: red; }\n"),
        ],
    );
    let index = index(&root, &[]);
    for path in ["a/x.module.css", "b/x.module.css"] {
        let domain = NameKey {
            domain: path.into(),
            ..key(ns::CLASS, "same")
        };
        assert_eq!(sites(&index, &domain), [(path.to_owned(), 1, Role::Define)]);
    }
}

/// A file reference reaches the file it spells or the file its language's probe finds, labelled;
/// directories, missing files, packages, aliases, dynamic and templated specifiers, URLs, a
/// `<base>`-rebased document and paths leaving the worktree reach nothing.
#[test]
fn file_references_join_files_and_refuse_everything_else() {
    let root = scratch("file-refs");
    fixture(&root);
    write(
        &root,
        &[
            (
                "web/js/dynamic.js",
                "import react from 'react';\nimport a from '@/alias/a';\n\
                 const m = await import(name);\nconst t = require(`./lib/${x}`);\n\
                 import up from '../../../../outside';\nimport d from './lib';\n\
                 import missing from './nope';\n",
            ),
            (
                "web/rebased.html",
                "<base href=\"/sub/\"><script src=\"js/app.js\"></script>\n",
            ),
            (
                "web/urls.html",
                "<script src=\"https://cdn.example/x.js\"></script>\n\
                 <script src=\"/root.js\"></script><script src=\"js/app.js?v=3#x\"></script>\n",
            ),
        ],
    );
    let index = index(&root, &[]);
    let reach = |language: Language, name: &str| {
        index
            .file_target(language, &key(ns::FILE_REF, name))
            .map(|target| (target.file.display().to_string(), target.certainty))
    };
    // HTML references name their file exactly.
    assert_eq!(
        reach(HTML, "web/css/site.css"),
        Some(("web/css/site.css".into(), Certainty::Exact))
    );
    assert_eq!(
        reach(HTML, "web/js/app.js"),
        Some(("web/js/app.js".into(), Certainty::Exact))
    );
    // Scripts probe extensions (`./lib/run` -> run.ts) and swaps (`./polyfill.js` -> polyfill.ts).
    assert_eq!(
        reach(TYPESCRIPT, "web/js/lib/run"),
        Some((
            "web/js/lib/run.ts".into(),
            Certainty::Heuristic("extension probe")
        ))
    );
    assert_eq!(
        reach(TYPESCRIPT, "web/js/polyfill.js"),
        Some((
            "web/js/polyfill.ts".into(),
            Certainty::Heuristic("extension swap")
        ))
    );
    // A directory, a miss and an exact-only language reach nothing.
    assert_eq!(reach(TYPESCRIPT, "web/js/lib"), None);
    assert_eq!(reach(TYPESCRIPT, "web/js/nope"), None);
    assert_eq!(reach(HTML, "web/js/lib/run"), None);

    // What was extracted: only static relative references, never the refused forms.
    let uses = |file: &str| -> Vec<String> {
        let all = index.facts_in(Path::new(file), agent_ide::lang::LineRange::new(1, 1000));
        all.iter()
            .filter(|site| site.fact.key.namespace == ns::FILE_REF)
            .map(|site| site.fact.key.name.to_string())
            .collect()
    };
    assert_eq!(uses("web/js/dynamic.js"), ["web/js/lib", "web/js/nope"]);
    assert!(uses("web/rebased.html").is_empty());
    assert_eq!(uses("web/urls.html"), ["web/js/app.js"]);
    assert_eq!(
        uses("web/index.html"),
        ["web/css/site.css", "web/js/app.js"]
    );

    // File-reference keys never answer a bare-name lookup.
    assert!(index.keys_named("web/js/app.js", None).is_empty());
}

/// Edits and deletions change the join on the next query: a shown target line follows the
/// target's bytes and a deleted target removes the edge; a changed CSS module moves its classes.
#[test]
fn links_follow_edits_and_deletions() {
    let root = scratch("invalidate");
    fixture(&root);
    let mut index = index(&root, &[]);
    let target = key(ns::FILE_REF, "web/js/lib/run");
    let shown = index.proven_definitions(TYPESCRIPT, &target, 2);
    assert_eq!(shown.sites[0].text, "export function run() {}");

    write(&root, &[("web/js/lib/run.ts", "export const run = 1;\n")]);
    assert_eq!(
        index.proven_definitions(TYPESCRIPT, &target, 2).sites[0].text,
        "export const run = 1;"
    );
    std::fs::remove_file(root.join("web/js/lib/run.ts")).unwrap();
    refresh(&mut index);
    assert!(index.file_target(TYPESCRIPT, &target).is_none());
    assert!(
        index
            .proven_definitions(TYPESCRIPT, &target, 2)
            .sites
            .is_empty()
    );

    // The module's class is renamed: the use no longer meets a definition.
    let primary = NameKey {
        domain: "web/ui/Button.module.css".into(),
        ..key(ns::CLASS, "primary")
    };
    assert_eq!(index.sites(&primary).len(), 2);
    write(
        &root,
        &[("web/ui/Button.module.css", ".renamed { color: blue; }\n")],
    );
    refresh(&mut index);
    let remaining = sites(&index, &primary);
    assert_eq!(remaining, [("web/ui/Button.tsx".to_owned(), 2, Role::Use)]);
}

/// A language computed in its module (anchors over `linkage/0`) and the same language in process
/// join with identical sites, in every combination of the three front-end languages.
#[test]
fn mixed_module_and_in_process_settings_join_identically() {
    let root = scratch("mixed");
    fixture(&root);
    let reference = index(&root, &[]);
    let keys = [
        key(ns::CLASS, "btn"),
        key(ns::CLASS, "card"),
        key(ns::ELEMENT_ID, "app"),
        key(ns::STYLE_VARIABLE, "brand"),
        key(ns::FILE_REF, "web/css/site.css"),
        key(ns::FILE_REF, "web/js/lib/run"),
        NameKey {
            domain: "web/ui/Button.module.css".into(),
            ..key(ns::CLASS, "primary")
        },
    ];
    let languages = [HTML, CSS, TYPESCRIPT];
    for mask in 1..8u8 {
        let routed: Vec<Language> = languages
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, language)| *language)
            .collect();
        let mixed = index(&root, &routed);
        for key in &keys {
            assert_eq!(mixed.sites(key), reference.sites(key), "{routed:?} {key:?}");
        }
        assert_eq!(mixed.summary(), reference.summary(), "{routed:?}");
    }
}

/// A file with more facts than the per-file limit is capped, not silently complete.
#[test]
fn the_per_file_fact_limit_is_reported() {
    let root = scratch("capped");
    let many: String = (0..5_100).map(|n| format!(".c{n} {{}}\n")).collect();
    write(&root, &[("big.css", &many)]);
    let index = index(&root, &[]);
    assert_eq!(index.capped_files(), 1);
    assert_eq!(index.summary().1, 5_000);
}
