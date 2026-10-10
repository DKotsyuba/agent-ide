//! The TypeScript/JavaScript module against the in-process path on one tree holding every script
//! extension (`.ts .tsx .js .jsx .mts .cts .mjs .cjs`): the same MCP transcript in module mode
//! (the shipped default) and with `AGENT_IDE_LANGUAGE_MODE=typescript=in_process`, edit forms with
//! their resulting file text, the cross-language name rows the scripts emit, and the process tree
//! (one analyzer module, no language server as a daemon child, nothing surviving its daemon).
//!
//! No provider is configured here, so every extension answers from its source (lexically); the
//! per-extension differences of a real bridge session (context enrichment only for js/jsx/ts/tsx)
//! are the real-provider test's, which needs the accepted `AGENT_IDE_NODE`,
//! `AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER` and `AGENT_IDE_TSSERVER`.

#[path = "support/parity.rs"]
mod parity;

use std::path::{Path, PathBuf};

use parity::{Daemon, Fixture, LANGUAGE_MODE, Node, Session, line_for};
use serde_json::{Value, json};

/// An accepted program entry of the launcher configuration.
fn accepted_program(path: &str, identity: &str) -> Value {
    json!({"path":path,"identity":identity,"blake3":blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()})
}

/// A bundle member's exact path, length and digest.
fn accepted_file(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap();
    json!({"path":path,"blake3":blake3::hash(&bytes).to_hex().to_string(),"bytes":bytes.len()})
}

/// The release-pinned TypeScript provider from the three accepted environment paths
/// (`AGENT_IDE_NODE`, `AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER`, `AGENT_IDE_TSSERVER`).
fn typescript_provider() -> Value {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let label = format!(
        "module-parity-typescript-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let node = PathBuf::from(std::env::var("AGENT_IDE_NODE").unwrap());
    let bridge = PathBuf::from(std::env::var("AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER").unwrap());
    let tsserver = PathBuf::from(std::env::var("AGENT_IDE_TSSERVER").unwrap());
    let typescript_root = tsserver.parent().unwrap().parent().unwrap();
    let bridge_root = bridge.parent().unwrap().parent().unwrap();
    let mut closure = [
        bridge_root.join("package.json"),
        typescript_root.join("lib/_tsserver.js"),
        typescript_root.join("lib/typescript.js"),
        typescript_root.join("package.json"),
    ];
    closure.sort();
    let mut provider = json!({
        "executable":accepted_program(bridge.to_str().unwrap(),"6.0.0"),
        "settings":"typescript_defaults_v1",
        "toolchain":"24.4.0",
        "node":accepted_program(node.to_str().unwrap(),"24.4.0"),
        "typescript":{
            "bridge_bytes":std::fs::metadata(&bridge).unwrap().len(),
            "bridge_version":"6.0.0",
            "tsserver":accepted_file(&tsserver),
            "typescript_version":"5.9.3",
            "closure":closure.iter().map(|path| accepted_file(path)).collect::<Vec<_>>(),
            "codex_macos_evidence":"macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r3-2026-09-14",
            "claude_macos_evidence":null
        },
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":label
    });
    use agent_ide::intelligence::typescript_backend::TypeScriptLaunch;
    agent_ide::languages::install();
    let unbound: agent_ide::assistance::launcher::ProviderLaunch =
        serde_json::from_value(provider.clone()).unwrap();
    provider["typescript"]["codex_macos_evidence"] =
        json!(unbound.expected_typescript_codex_macos_evidence().unwrap());
    provider["typescript"]["claude_macos_evidence"] =
        json!(unbound.expected_typescript_claude_macos_evidence().unwrap());
    provider
}

/// The in-process fallback of the TypeScript module.
const IN_PROCESS: (&str, &str) = (LANGUAGE_MODE, "typescript=in_process");

/// Every script extension with a function, a class with a method and an exported constant, plus a
/// stylesheet and a page the scripts' class and element-id uses join, configured with `providers`.
fn fixture(providers: Value) -> Fixture {
    let code = |name: &str| {
        format!(
            "/** {name} greets 😀. */ export function greet_{name}(who: string) {{\n  return 'hi ' + who;\n}}\n\nexport class Box_{name} {{\n  open() {{\n    return 1;\n  }}\n}}\n\nexport const LIMIT_{name} = 3;\n"
        )
    };
    let plain = |name: &str| {
        format!(
            "/** {name} greets. */\nexport function greet_{name}(who) {{\n  return 'hi ' + who;\n}}\n\nexport class Box_{name} {{\n  open() {{\n    return 1;\n  }}\n}}\n\nexport const LIMIT_{name} = 3;\n"
        )
    };
    let tsx = "export function Panel() {\n  return <p className=\"btn\" id=\"panel\">x</p>;\n}\n";
    let jsx = "export const Card = () => <section className=\"btn card\">x</section>;\n";
    let cjs = "function read_cjs() {\n  return document.getElementById('main');\n}\nmodule.exports = { read_cjs };\n";
    Fixture::new(
        &[
            (
                "package.json",
                "{\"name\":\"parity\",\"type\":\"module\"}\n",
            ),
            (
                "tsconfig.json",
                "{\"compilerOptions\":{\"allowJs\":true,\"jsx\":\"preserve\",\"moduleResolution\":\"bundler\",\"types\":[]},\"include\":[\"src\"]}\n",
            ),
            ("src/a.ts", &code("ts")),
            ("src/panel.tsx", tsx),
            ("src/c.js", &plain("js")),
            ("src/card.jsx", jsx),
            ("src/e.mts", &code("mts")),
            ("src/f.cts", &code("cts")),
            ("src/g.mjs", &plain("mjs")),
            ("src/h.cjs", cjs),
            (
                "styles.css",
                ".btn { color: red; }\n.card { margin: 0; }\n#main { display: block; }\n",
            ),
            (
                "index.html",
                "<main id=\"main\" class=\"card\"><button class=\"btn\">x</button></main>\n",
            ),
        ],
        providers,
    )
}

/// The extension-specific files whose outlines and reads are compared.
const FILES: [&str; 8] = [
    "src/a.ts",
    "src/panel.tsx",
    "src/c.js",
    "src/card.jsx",
    "src/e.mts",
    "src/f.cts",
    "src/g.mjs",
    "src/h.cjs",
];

/// The symbol each extension's file declares first, for a position inside it.
const MARKERS: [&str; 8] = [
    "greet_ts",
    "Panel",
    "greet_js",
    "Card",
    "greet_mts",
    "greet_cts",
    "greet_mjs",
    "read_cjs",
];

/// The read-only calls whose replies must not depend on where TypeScript computes.
fn calls(fixture: &Fixture) -> Vec<(&'static str, Value)> {
    let mut calls: Vec<(&'static str, Value)> = FILES
        .iter()
        .map(|path| ("ide.outline", json!({"path": path})))
        .collect();
    for path in FILES {
        calls.push(("ide.context", json!({"path": path})));
    }
    // A position inside the first declaration: the bridge's semantic context where the
    // extension has one.
    for (path, marker) in FILES.iter().zip(MARKERS) {
        let text = std::fs::read_to_string(fixture.root.join(path)).unwrap();
        let at = text.find(marker).expect("the marker") + 2;
        calls.push(("ide.context", json!({"path": path, "byte_offset": at})));
    }
    calls.extend([
        ("ide.read", json!({"symbol":"src/a.ts#greet_ts"})),
        ("ide.read", json!({"symbol":"src/c.js#Box_js/open"})),
        ("ide.read", json!({"symbol":"src/e.mts#LIMIT_mts"})),
        ("ide.read", json!({"symbol":"src/f.cts#Box_cts"})),
        ("ide.read", json!({"symbol":"src/h.cjs#read_cjs"})),
        ("ide.read", json!({"path":"src/g.mjs","lines":"1-4"})),
        ("ide.symbol", json!({"symbol":"src/a.ts#greet_ts"})),
        ("ide.symbol", json!({"symbol":"src/panel.tsx#Panel"})),
        ("ide.symbol", json!({"symbol":"src/card.jsx#Card"})),
        ("ide.symbol", json!({"symbol":"src/h.cjs#read_cjs"})),
        ("ide.symbol", json!({"symbol":"styles.css#.btn"})),
        ("ide.symbol", json!({"symbol":".btn"})),
        ("ide.symbol", json!({"symbol":"##main"})),
        ("ide.symbol", json!({"symbol":"##panel"})),
        (
            "ide.graph",
            json!({"symbol":"styles.css#.btn","direction":"callers","depth":2}),
        ),
        (
            "ide.graph",
            json!({"symbol":"src/panel.tsx#Panel","direction":"callees","depth":2}),
        ),
    ]);
    calls
}

/// The calls on a fresh daemon with `env`, the TypeScript analyzer module seen, whether the bridge
/// ran as a direct daemon child, and the daemon.
async fn run(env: &[(&str, &str)], bridge: bool) -> (Vec<String>, Option<Node>, bool, Daemon) {
    let fixture = fixture(if bridge {
        json!([typescript_provider()])
    } else {
        json!([])
    });
    let mut daemon = Daemon::start(&fixture, env).await;
    let mut session = Session::start(&fixture).await;
    let mut replies = Vec::new();
    for (tool, arguments) in calls(&fixture) {
        let reply = session.call(&fixture, tool, arguments.clone()).await;
        replies.push(line_for(&fixture, tool, &arguments, &reply));
    }
    let module = daemon.tree().module("typescript", "analyzer").cloned();
    let direct = daemon.tree().direct_child_runs("cli.mjs");
    session.close(&fixture).await;
    (replies, module, direct, daemon)
}

/// The transcript on both paths with `providers`: the in-process daemon runs no module; the
/// module daemon runs one analyzer child that owns the bridge when `bridge` is set; nothing
/// survives either daemon. Returns both transcripts.
async fn transcripts(bridge: bool) -> (Vec<String>, Vec<String>) {
    let (in_process, module, direct, mut daemon) = run(&[IN_PROCESS], bridge).await;
    assert!(module.is_none(), "in process: no TypeScript module runs");
    assert_eq!(direct, bridge, "in process the bridge is a daemon child");
    let owned = daemon.tree().all();
    drop(daemon);
    let (moduled, module, moduled_direct, mut daemon) = run(&[], bridge).await;
    let module = module.expect("TypeScript is a module child");
    assert!(
        !moduled_direct,
        "no language server is a direct daemon child in module mode"
    );
    if bridge {
        assert!(
            matches!(module.children.as_slice(), [(_, command)]
                if command.contains("cli.mjs") && command.ends_with("--stdio")),
            "the analyzer module owns one bridge process: {module:?}"
        );
    }
    let owned: Vec<_> = owned.into_iter().chain(daemon.tree().all()).collect();
    drop(daemon);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived its daemon");
    }
    (in_process, moduled)
}

/// Without a provider every extension answers the same in module mode and in process: the
/// scripts' class and element-id uses reach the style sheet and the page, a context is lexical,
/// and what needs the bridge refuses identically.
#[tokio::test]
async fn typescript_module_matches_in_process_answers() {
    let (in_process, moduled) = transcripts(false).await;
    assert!(
        moduled
            .iter()
            .any(|reply| reply.contains("src/panel.tsx") && reply.contains("styles.css")),
        "the .btn card links the component"
    );
    assert!(moduled.iter().any(|reply| reply.contains("mode: lexical")));
    parity::assert_parity(&in_process, &moduled);
}

/// With the accepted bridge: outline, context, read, symbol cards and graph leaves of all eight
/// extensions are the same in module mode and in process, and the per-extension differences
/// hold: only js/jsx/ts/tsx contexts are semantic, the other four extensions answer from their
/// source.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environments"]
async fn typescript_module_matches_in_process_answers_with_the_bridge() {
    let (in_process, moduled) = transcripts(true).await;
    if let Ok(dir) = std::env::var("TS_DUMP") {
        std::fs::write(
            format!("{dir}/inproc.txt"),
            parity::normalized(&in_process.join("\n#####\n")),
        )
        .unwrap();
        std::fs::write(
            format!("{dir}/module.txt"),
            parity::normalized(&moduled.join("\n#####\n")),
        )
        .unwrap();
    }
    for path in ["src/a.ts", "src/panel.tsx", "src/c.js", "src/card.jsx"] {
        let context = format!("ide.context {{\"byte_offset\":");
        let reply = moduled.iter().find(|reply| {
            reply.starts_with(&context) && reply.lines().next().is_some_and(|l| l.contains(path))
        });
        assert!(
            reply.is_some_and(|reply| reply.contains("mode: semantic")),
            "{path} is a semantic context: {reply:?}"
        );
    }
    for path in ["src/e.mts", "src/f.cts", "src/g.mjs", "src/h.cjs"] {
        let context = format!("ide.context {{\"byte_offset\":");
        let reply = moduled.iter().find(|reply| {
            reply.starts_with(&context) && reply.lines().next().is_some_and(|l| l.contains(path))
        });
        assert!(
            reply.is_some_and(|reply| !reply.contains("mode: semantic")),
            "{path} has no semantic context: {reply:?}"
        );
    }
    parity::assert_parity(&in_process, &moduled);
}

/// Every edit form answers the same and leaves the same files, whichever extension: insertion
/// after a function, into a class, before an exported constant, symbol replacement and deletion,
/// and a line edit proven by a fresh read's reference.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environments"]
async fn typescript_edits_match_in_process_results() {
    edits_match(true).await;
}

/// The edit forms, with the accepted bridge when `bridge` is set.
async fn edits_match(bridge: bool) {
    let edits = [
        json!({"operation_id":"ts-after","op":"insert","symbol":"src/a.ts#greet_ts","where":"after","content":"export function extra_ts() {\n  return 2;\n}"}),
        json!({"operation_id":"js-class","op":"insert","symbol":"src/c.js#Box_js","where":"last","content":"close() {\n  return 0;\n}"}),
        json!({"operation_id":"mts-replace","op":"replace","symbol":"src/e.mts#LIMIT_mts","content":"export const LIMIT_mts = 4;"}),
        json!({"operation_id":"cts-delete","op":"delete","symbol":"src/f.cts#Box_cts"}),
        json!({"operation_id":"mjs-before","op":"insert","symbol":"src/g.mjs#greet_mjs","where":"before","content":"export const first_mjs = 0;"}),
        json!({"operation_id":"cjs-replace","op":"replace","symbol":"src/h.cjs#read_cjs","content":"function read_cjs() {\n  return null;\n}"}),
    ];
    let mut outcomes: Vec<(Vec<String>, Vec<String>, Vec<Value>)> = Vec::new();
    for env in [&[IN_PROCESS][..], &[][..]] {
        let fixture = fixture(if bridge {
            json!([typescript_provider()])
        } else {
            json!([])
        });
        let _daemon = Daemon::start(&fixture, env).await;
        let mut session = Session::start(&fixture).await;
        // Read first, as the workflow does, so symbol edits see a retained source.
        for path in FILES {
            session
                .call(&fixture, "ide.outline", json!({"path":path}))
                .await;
        }
        let (mut replies, mut raw) = (Vec::new(), Vec::new());
        for edit in &edits {
            let reply = session.call(&fixture, "ide.edit", edit.clone()).await;
            replies.push(line_for(&fixture, "ide.edit", edit, &reply));
            raw.push(reply);
        }
        let read = session
            .call(
                &fixture,
                "ide.read",
                json!({"path":"src/panel.tsx","lines":"1-1"}),
            )
            .await;
        let edit = json!({"operation_id":"tsx-lines","path":"src/panel.tsx","lines":"1-1",
            "source_ref":read["detail_ref"],"content":"export function Panel(): JSX.Element {"});
        let reply = session.call(&fixture, "ide.edit", edit.clone()).await;
        replies.push(line_for(&fixture, "ide.edit", &edit, &reply));
        raw.push(reply);
        let files = FILES
            .iter()
            .map(|name| std::fs::read_to_string(fixture.root.join(name)).unwrap())
            .collect();
        session.close(&fixture).await;
        outcomes.push((replies, files, raw));
    }
    parity::assert_parity(&outcomes[0].0, &outcomes[1].0);
    assert_eq!(outcomes[0].1, outcomes[1].1, "the edited files differ");
    for (_, files, raw) in &outcomes {
        for (at, reply) in raw.iter().enumerate() {
            assert_eq!(reply["state"], "edit", "edit {at}: {reply}");
        }
        assert!(files[0].contains("extra_ts"), "{}", files[0]);
        assert!(files[2].contains("close()"), "{}", files[2]);
        assert!(files[4].contains("LIMIT_mts = 4"), "{}", files[4]);
        assert!(!files[5].contains("Box_cts"), "{}", files[5]);
        assert!(files[6].contains("first_mjs"), "{}", files[6]);
        assert!(files[7].contains("return null"), "{}", files[7]);
        assert!(files[1].starts_with("export function Panel(): JSX.Element {"));
    }
}
