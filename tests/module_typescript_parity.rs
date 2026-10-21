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

use parity::{source_query, source_query_tool};
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

/// Local adapter-parity stand-in that accepts only the exact stdin formatter argv and
/// deterministically formats the compact fixture function and import quote without a registry.
/// This checks adapter wiring, not compatibility with the real Prettier package.
const PRETTIER_STAND_IN: &str = r##"#!/usr/bin/env node
const fs = require('node:fs');
const args = process.argv.slice(2);
if (JSON.stringify(args) !== '["--stdin-filepath","src/index.js"]') process.exit(2);
const file = args[1];
if (!fs.existsSync(file)) process.exit(2);
const source = fs.readFileSync(0, 'utf8');
const compact = "export function hello(name){return shout('hello '+name)+legacy()}";
const formatted = source.replace(compact,
  "export function hello(name) {\n  return shout('hello ' + name) + legacy();\n}")
  .replace("import { shout } from 'dep';", 'import { shout } from "dep";');
process.stdout.write(formatted);
"##;

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
        calls.push(source_query(json!({"path": path})));
    }
    // A position inside the first declaration: the bridge's semantic context where the
    // extension has one.
    for (path, marker) in FILES.iter().zip(MARKERS) {
        let text = std::fs::read_to_string(fixture.root.join(path)).unwrap();
        let at = text.find(marker).expect("the marker") + 2;
        calls.push(source_query(json!({"path": path, "byte_offset": at})));
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
        let context = "ide.symbol {\"byte_offset\":";
        let reply = moduled.iter().find(|reply| {
            reply.starts_with(context) && reply.lines().next().is_some_and(|l| l.contains(path))
        });
        assert!(
            reply.is_some_and(|reply| reply.contains("mode: semantic")),
            "{path} is a semantic context: {reply:?}"
        );
    }
    for path in ["src/e.mts", "src/f.cts", "src/g.mjs", "src/h.cjs"] {
        let context = "ide.symbol {\"byte_offset\":";
        let reply = moduled.iter().find(|reply| {
            reply.starts_with(context) && reply.lines().next().is_some_and(|l| l.contains(path))
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

/// A Node back-end project with a fixture-local offline formatter adapter: ESM/CommonJS entries,
/// an imported package, `node:test` files checking cwd/PATH/argv, and a `tsconfig.json` for `tsc`.
/// Its formatter stand-in validates only adapter argv and is not real Prettier qualification.
fn node_fixture(bridge: bool) -> Fixture {
    let fixture = Fixture::new(
        &[
            (
                "package.json",
                "{\"name\":\"api\",\"version\":\"1.0.0\",\"type\":\"module\",\"main\":\"src/index.js\",\"exports\":{\".\":\"./src/index.js\"},\"scripts\":{\"test\":\"node --test\"},\"devDependencies\":{\"prettier\":\"1.0.0\"}}\n",
            ),
            (
                "tsconfig.json",
                "{\"compilerOptions\":{\"allowJs\":true,\"checkJs\":true,\"noEmit\":true,\"module\":\"esnext\",\"target\":\"es2022\",\"moduleResolution\":\"bundler\",\"types\":[]},\"include\":[\"src\"]}\n",
            ),
            (
                "src/index.js",
                "import { shout } from 'dep';\nimport { legacy } from './legacy.cjs';\n\nexport function hello(name) {\n  return shout('hello ' + name) + legacy();\n}\n\nexport const PORT = 8080;\n\nexport function broken() {\n  /** @type {number} */\n  const n = 'text';\n  return n;\n}\n",
            ),
            (".prettierrc", "{\"semi\":true}\n"),
            (".npmrc", "offline=true\nyes=false\n"),
            (".gitignore", "node_modules/.bin/prettier\n"),
            (
                "node_modules/prettier/package.json",
                "{\"name\":\"prettier\",\"version\":\"1.0.0\",\"bin\":{\"prettier\":\"bin/prettier.cjs\"}}\n",
            ),
            ("node_modules/prettier/bin/prettier.cjs", PRETTIER_STAND_IN),
            ("node_modules/.bin/.keep", ""),
            (
                "src/legacy.cjs",
                "const util = require('./util.cjs');\nfunction legacy() {\n  return util.tag();\n}\nmodule.exports = { legacy };\n",
            ),
            (
                "src/util.cjs",
                "exports.tag = function tag() {\n  return '!';\n};\n",
            ),
            (
                "node_modules/dep/package.json",
                "{\"name\":\"dep\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
            ),
            (
                "node_modules/dep/index.js",
                "exports.shout = function shout(text) {\n  return text.toUpperCase();\n};\n",
            ),
            (
                "test/pass.test.mjs",
                "import test from 'node:test';\nimport assert from 'node:assert';\nimport { delimiter, dirname, resolve } from 'node:path';\nimport { fileURLToPath } from 'node:url';\n\ntest('runs from the project recipe', () => {\n  assert.equal(process.cwd(), resolve(dirname(fileURLToPath(import.meta.url)), '..'));\n  assert.equal(process.env.PATH?.split(delimiter)[0], dirname(process.execPath));\n  assert.ok(process.argv.some((arg) => arg.endsWith('/test/pass.test.mjs') || arg === 'test/pass.test.mjs'));\n});\n\ntest('adds', () => {\n  assert.equal(1 + 1, 2);\n});\n",
            ),
            (
                "test/fail.test.mjs",
                "import test from 'node:test';\nimport assert from 'node:assert';\n\ntest('subtracts', () => {\n  assert.equal(2 - 1, 3);\n});\n",
            ),
        ],
        if bridge {
            json!([typescript_provider()])
        } else {
            json!([])
        },
    );
    let prettier_link = fixture.root.join("node_modules/.bin/prettier");
    std::os::unix::fs::symlink("../prettier/bin/prettier.cjs", &prettier_link).unwrap();
    let prettier_script = fixture.root.join("node_modules/prettier/bin/prettier.cjs");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(prettier_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let node = std::env::var("AGENT_IDE_NODE").unwrap();
    let tsc = Path::new(&std::env::var("AGENT_IDE_TSSERVER").unwrap())
        .with_file_name("tsc.js")
        .display()
        .to_string();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["project_checks"] = json!({
        "debounce_ms":100,
        "check_timeout_s":30,
        "typescript":{"node":node,"tsc_cli":tsc}
    });
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    fixture
}

/// `line` with the duration after `project check ` (`0.7s`) masked.
fn untimed(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("project check ") {
        let (head, tail) = rest.split_at(at + "project check ".len());
        out.push_str(head);
        let end = tail
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(tail.len());
        let (number, after) = tail.split_at(end);
        if number.is_empty() || !after.starts_with('s') {
            out.push_str(number);
        } else {
            out.push_str("<t>");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Settles an `ide.test` run: starts it and polls its status until it reports a result.
async fn run_tests(session: &mut Session, fixture: &Fixture, arguments: Value) -> (Value, String) {
    let started = session.call(fixture, "ide.test", arguments).await;
    let text = started["text"].as_str().unwrap_or_default().to_owned();
    let Some(id) = text
        .strip_prefix("tests #")
        .and_then(|rest| rest.split(':').next())
        .and_then(|id| id.parse::<u64>().ok())
    else {
        return (started, text);
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let reply = session
            .call(fixture, "ide.test", json!({"status": id}))
            .await;
        let text = reply["text"].as_str().unwrap_or_default().to_owned();
        if !text.contains("started") && !text.contains("running")
            || std::time::Instant::now() > deadline
        {
            return (reply, text);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// The Node back-end transcript on a fresh daemon with `env`: reads, cards, positions, rename,
/// checked test recipes, and the tsc check page. Each route's command details are checked before
/// only those route-specific recipe footer rows are canonicalized for parity. With `TS_DUMP`, it
/// retains route-specific telemetry and source bytes around the rename for unknown-outcome review.
async fn node_transcript(env: &[(&str, &str)]) -> (Vec<String>, String) {
    let fixture = node_fixture(true);
    let route = if env.is_empty() { "module" } else { "native" };
    let dump_dir = std::env::var_os("TS_DUMP").map(PathBuf::from);
    if let Some(directory) = &dump_dir {
        std::fs::create_dir_all(directory).unwrap();
    }
    let telemetry_directory = dump_dir.as_ref().map(|directory| {
        let telemetry = directory.join("telemetry-0700");
        std::fs::create_dir_all(&telemetry).unwrap();
        use std::os::unix::fs::PermissionsExt;
        // Durable telemetry rejects a public parent; keep it separate from ordinary dumps.
        std::fs::set_permissions(&telemetry, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::canonicalize(telemetry).unwrap()
    });
    let telemetry_database = telemetry_directory
        .as_ref()
        .map(|directory| directory.join(format!("node-{route}.sqlite")));
    let telemetry_database = telemetry_database.map(|path| path.display().to_string());
    let mut daemon_env = env.to_vec();
    if let Some(database) = telemetry_database.as_deref() {
        daemon_env.push(("AGENT_IDE_TELEMETRY_DATABASE", database));
    }
    let _daemon = Daemon::start(&fixture, &daemon_env).await;
    let mut session = Session::start(&fixture).await;
    let text = std::fs::read_to_string(fixture.root.join("src/index.js")).unwrap();
    let hello = text.find("hello(").unwrap() + 2;
    let shout = text.find("shout(").unwrap() + 2;
    let mut replies = Vec::new();
    // The project card: the package manager, test runner, formatter and entry points the module
    // computed from `package.json` and the tree.
    let card = session
        .call(&fixture, "ide.start", json!({"activation_id":"node-card"}))
        .await;
    // Its `modules:` row names which path serves, the one line that differs by design.
    let card = line_for(&fixture, "ide.start", &json!({}), &card);
    let served = card
        .lines()
        .find(|line| line.starts_with("modules: "))
        .unwrap_or_default()
        .to_owned();
    replies.push(
        card.lines()
            .filter(|line| !line.starts_with("modules: "))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let reads = [
        ("ide.outline", json!({"path":"src/index.js"})),
        ("ide.outline", json!({"path":"src/legacy.cjs"})),
        ("ide.outline", json!({"path":"test/pass.test.mjs"})),
        ("ide.read", json!({"symbol":"src/index.js#hello"})),
        (
            "ide.symbol",
            json!({"symbol":"src/index.js#hello","callees":1}),
        ),
        ("ide.symbol", json!({"symbol":"src/index.js#PORT"})),
        source_query(json!({"path":"src/index.js","byte_offset":hello})),
        source_query(json!({"path":"src/index.js","byte_offset":shout})),
    ];
    for (tool, arguments) in reads {
        let reply = session.call(&fixture, tool, arguments.clone()).await;
        replies.push(line_for(&fixture, tool, &arguments, &reply));
    }
    for arguments in [
        json!({"path":"test/pass.test.mjs"}),
        json!({"path":"test/fail.test.mjs"}),
        json!({"path":"src/index.js"}),
        // A TAP stream from the explicit command the product parses too.
        json!({"command":["node","--test","--test-reporter=tap","test/fail.test.mjs"]}),
    ] {
        let (reply, text) = run_tests(&mut session, &fixture, arguments.clone()).await;
        assert_node_test_recipe(
            &mut session,
            &fixture,
            &arguments,
            &reply,
            &text,
            env.is_empty(),
        )
        .await;
        // Node reports the canonical spelling of the fixture root (`/private/var/…`).
        let canonical = fixture.root.canonicalize().unwrap().display().to_string();
        let entry =
            line_for(&fixture, "ide.test", &arguments, &reply).replace(&canonical, "<root>");
        replies.push(
            entry
                .lines()
                .filter(|row| !row.starts_with("  full replay:"))
                .map(|row| {
                    if row.starts_with("  rerun") {
                        "  rerun: <route-verified recipe>"
                    } else {
                        row
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let rename = json!({"operation_id":"rename-port","op":"rename","symbol":"src/index.js#PORT","new_name":"LISTEN_PORT"});
    if let Some(directory) = &dump_dir {
        std::fs::write(
            directory.join(format!("node-{route}-src-before-rename.js")),
            std::fs::read(fixture.root.join("src/index.js")).unwrap(),
        )
        .unwrap();
    }
    let reply = session.call(&fixture, "ide.edit", rename.clone()).await;
    if let Some(directory) = &dump_dir {
        std::fs::write(
            directory.join(format!("node-{route}-src-after-rename.js")),
            std::fs::read(fixture.root.join("src/index.js")).unwrap(),
        )
        .unwrap();
    }
    replies.push(line_for(&fixture, "ide.edit", &rename, &reply));
    replies.push(std::fs::read_to_string(fixture.root.join("src/index.js")).unwrap());
    // An edit the formatter plan is asked about (prettier is configured by `.prettierrc`).
    let format = json!({"operation_id":"format-hello","op":"replace","symbol":"src/index.js#hello","content":"export function hello(name){return shout('hello '+name)+legacy()}"});
    let reply = session.call(&fixture, "ide.edit", format.clone()).await;
    assert_eq!(reply["state"], "edit", "formatter-backed edit: {reply}");
    let formatted_source = std::fs::read_to_string(fixture.root.join("src/index.js")).unwrap();
    assert!(
        formatted_source.contains(
            "export function hello(name) {\n  return shout('hello ' + name) + legacy();\n}"
        ),
        "the fixture formatter made its deterministic formatting change:\n{formatted_source}"
    );
    // The edit's own project-check duration is the one timing in the reply.
    replies.push(untimed(&line_for(&fixture, "ide.edit", &format, &reply)));
    replies.push(std::fs::read_to_string(fixture.root.join("src/index.js")).unwrap());
    // A candidate that does not parse is refused by the syntax probe, nothing written.
    let broken = json!({"operation_id":"broken-hello","op":"replace","symbol":"src/index.js#hello","content":"export function hello( {"});
    let reply = session.call(&fixture, "ide.edit", broken.clone()).await;
    replies.push(line_for(&fixture, "ide.edit", &broken, &reply));
    replies.push(std::fs::read_to_string(fixture.root.join("src/index.js")).unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let page = session
            .call(
                &fixture,
                source_query_tool(&json!({"kind":"problems","language":"typescript"})),
                json!({"kind":"problems","language":"typescript"}),
            )
            .await;
        let text = page["text"].as_str().unwrap_or_default().to_owned();
        if (!text.contains("checking") && text.contains("typescript: "))
            || std::time::Instant::now() > deadline
        {
            replies.push(text);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    session.close(&fixture).await;
    (replies, served)
}

/// Verifies one route's settled Node test status and execution recipe before its footer is
/// canonicalized for parity. Native preview receipts are recovered through the public output
/// continuation; module-owned project recipes retain their explicit HOME, PATH, executable and
/// argv, while explicit commands use the daemon-native route in either hosting mode. The passing
/// fixture independently asserts the actual process cwd, PATH prefix and test argv.
async fn assert_node_test_recipe(
    session: &mut Session,
    fixture: &Fixture,
    arguments: &Value,
    reply: &Value,
    text: &str,
    module: bool,
) {
    assert_eq!(reply["state"], "complete", "{reply}");
    assert_eq!(reply["kind"], "test", "{reply}");

    let (target, command_args, summary, exit) = if arguments["path"] == "test/pass.test.mjs" {
        (
            "test/pass.test.mjs",
            vec!["--test", "test/pass.test.mjs"],
            "2 passed, 0 failed",
            "(exit 0)",
        )
    } else if arguments["path"] == "test/fail.test.mjs" {
        (
            "test/fail.test.mjs",
            vec!["--test", "test/fail.test.mjs"],
            "0 passed, 1 failed",
            "(exit 1)",
        )
    } else if arguments["path"] == "src/index.js" {
        assert!(text.contains("no tests in src/index.js"), "{text}");
        assert!(!text.contains("  rerun"), "no command ran: {text}");
        return;
    } else {
        (
            "test/fail.test.mjs",
            vec!["--test", "--test-reporter=tap", "test/fail.test.mjs"],
            "0 passed, 1 failed",
            "(exit 1)",
        )
    };
    assert!(text.contains(summary) && text.contains(exit), "{text}");
    if target == "test/fail.test.mjs" {
        assert!(text.contains("FAIL subtracts"), "{text}");
    }

    let node = PathBuf::from(std::env::var("AGENT_IDE_NODE").unwrap())
        .canonicalize()
        .unwrap();
    let node_dir = node.parent().unwrap().display().to_string();
    let rerun = text
        .lines()
        .find(|line| line.starts_with("  rerun:") || line.starts_with("  rerun preview:"))
        .expect("a started test has an actionable rerun recipe");
    let module_recipe = module && arguments.get("command").is_none();
    if module_recipe {
        let path = format!("{node_dir}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
        let home = agent_ide::userhome::user_home().unwrap();
        let prefix = format!(
            "  rerun: /usr/bin/env -i 'HOME={}' 'PATH={path}'",
            home.display()
        );
        let suffix = format!("{} {}", node.display(), command_args.join(" "));
        assert_eq!(rerun, format!("{prefix} {suffix}"));
    } else {
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("{node_dir}:{inherited_path}");
        if rerun.starts_with("  rerun preview: ") {
            assert!(
                rerun.starts_with("  rerun preview: /usr/bin/env "),
                "{rerun}"
            );
            assert!(rerun.ends_with('…'), "{rerun}");
        } else {
            assert_eq!(
                rerun,
                format!(
                    "  rerun: /usr/bin/env 'PATH={path}' {} {}",
                    node.display(),
                    command_args.join(" ")
                )
            );
        }
    }

    if let Some(line) = text
        .lines()
        .find(|line| line.starts_with("  full replay: ide.test "))
    {
        assert!(
            !module_recipe,
            "module recipes are already complete: {line}"
        );
        assert!(
            rerun.contains("preview:"),
            "full replay follows a preview: {text}"
        );
        let replay: Value =
            serde_json::from_str(line.strip_prefix("  full replay: ide.test ").unwrap())
                .expect("the preview carries its replay query");
        let reference = replay["output"].as_str().unwrap();
        assert!(reference.contains("/replay/output/1"), "{replay}");
        let replay = parity::recover_test_replay(session, fixture, reference).await;
        assert_eq!(
            replay["cwd"], ".",
            "replay runs from the project root: {replay}"
        );
        let expected_path = format!("{node_dir}:{}", std::env::var("PATH").unwrap_or_default());
        let mut expected = vec![
            "/usr/bin/env".to_owned(),
            format!("PATH={expected_path}"),
            node.to_str().unwrap().to_owned(),
        ];
        expected.extend(command_args.into_iter().map(str::to_owned));
        let actual = replay["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| argument.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "complete native argv: {replay}");
    } else {
        assert!(
            !rerun.contains("preview"),
            "a preview must have a full replay: {text}"
        );
    }
}

/// The Node back-end project answers the same in module mode and in process: outline, read, cards
/// with callees, positions across a `node_modules` import and a CommonJS require, `node:test`
/// runs (pass, fail, no tests), a project-wide rename and the server-side `tsc` check.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environments"]
async fn typescript_node_backend_matches_in_process_answers() {
    let (in_process, in_process_served) = node_transcript(&[IN_PROCESS]).await;
    let (moduled, module_served) = node_transcript(&[]).await;
    assert_eq!(
        in_process_served,
        "modules: typescript in process (fallback)"
    );
    assert_eq!(module_served, "modules: typescript module");
    for r in &moduled {
        eprintln!(
            "=====\n{}",
            r.lines().take(30).collect::<Vec<_>>().join("\n")
        );
    }
    if let Ok(dir) = std::env::var("TS_DUMP") {
        std::fs::write(
            format!("{dir}/node-inproc.txt"),
            in_process.join("\n#####\n"),
        )
        .unwrap();
        std::fs::write(format!("{dir}/node-module.txt"), moduled.join("\n#####\n")).unwrap();
    }
    parity::assert_parity(&in_process, &moduled);
    let all = moduled.join("\n");
    for expected in [
        "test: npm run test",
        "2 passed, 0 failed",
        "FAIL subtracts",
        "no tests in src/index.js",
        "renamed PORT → LISTEN_PORT",
        "import { shout } from \"dep\";",
        "produced a syntax error",
        "TS2322",
    ] {
        assert!(all.contains(expected), "{expected}:\n{all}");
    }
}

/// One project shape per fixture: the resolution the module interprets must give the daemon the
/// same verdict as the in-process interpretation, rejection text included.
fn resolution_fixture(files: &[(&str, &str)]) -> Fixture {
    Fixture::new(files, json!([typescript_provider()]))
}

/// The reads of `path` and its context at the declaration on a fresh daemon with `env`.
async fn resolution_transcript(
    env: &[(&str, &str)],
    files: &[(&str, &str)],
    path: &str,
    marker: &str,
) -> Vec<String> {
    let fixture = resolution_fixture(files);
    let _daemon = Daemon::start(&fixture, env).await;
    let mut session = Session::start(&fixture).await;
    let text = std::fs::read_to_string(fixture.root.join(path)).unwrap();
    let at = text.find(marker).unwrap() + 2;
    let mut replies = Vec::new();
    for (tool, arguments) in [
        ("ide.outline", json!({"path": path})),
        source_query(json!({"path": path, "byte_offset": at})),
    ] {
        let reply = session.call(&fixture, tool, arguments.clone()).await;
        replies.push(line_for(&fixture, tool, &arguments, &reply));
    }
    session.close(&fixture).await;
    replies
}

/// A named project tree and either its `semantic` verdict or exact lexical refusal reason.
type ResolutionCase<'a> = (&'a str, Vec<(&'a str, String)>, &'a str);

/// Accepted and refused project shapes give the same answers in module mode and in process: a
/// plain include, a Vite-style solution `tsconfig.json` whose `references` target must be read
/// (a second round with the module), a wildcard include, an `exclude`, a `paths` mapping, open
/// `types` and a document no config admits.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environments"]
async fn typescript_project_resolution_matches_in_process_answers() {
    let source = "export function area(w: number) {\n  return w * w;\n}\n";
    let options = "\"compilerOptions\":{\"types\":[],\"moduleResolution\":\"bundler\"}";
    let cases: [ResolutionCase; 7] = [
        (
            "plain include",
            vec![
                ("tsconfig.json", format!("{{{options},\"include\":[\"src\"]}}")),
                ("src/area.ts", source.to_owned()),
            ],
            "semantic",
        ),
        (
            "solution references",
            vec![
                (
                    "tsconfig.json",
                    "{\"files\":[],\"references\":[{\"path\":\"./tsconfig.app.json\"}]}".to_owned(),
                ),
                (
                    "tsconfig.app.json",
                    format!("{{{options},\"include\":[\"src\"]}}"),
                ),
                ("src/area.ts", source.to_owned()),
            ],
            "semantic",
        ),
        (
            "wildcard include",
            vec![
                ("tsconfig.json", format!("{{{options},\"include\":[\"src/**/*\"]}}")),
                ("src/area.ts", source.to_owned()),
            ],
            "entry `src/**/*` must be a unique normalized relative path without wildcards",
        ),
        (
            "exclude",
            vec![
                (
                    "tsconfig.json",
                    format!("{{{options},\"include\":[\"src\"],\"exclude\":[\"dist\"]}}"),
                ),
                ("src/area.ts", source.to_owned()),
            ],
            "`exclude` is unsupported; membership must be literal",
        ),
        (
            "paths mapping",
            vec![
                (
                    "tsconfig.json",
                    "{\"compilerOptions\":{\"types\":[],\"moduleResolution\":\"bundler\",\"paths\":{\"@/*\":[\"src/*\"]}},\"include\":[\"src\"]}"
                        .to_owned(),
                ),
                ("src/area.ts", source.to_owned()),
            ],
            "`compilerOptions.paths` is unsupported",
        ),
        (
            "open types",
            vec![
                (
                    "tsconfig.json",
                    "{\"compilerOptions\":{\"types\":[\"node\"],\"moduleResolution\":\"bundler\"},\"include\":[\"src\"]}"
                        .to_owned(),
                ),
                ("src/area.ts", source.to_owned()),
            ],
            "`compilerOptions.types` must be `[]` or `[\"vite/client\"]`",
        ),
        (
            "document outside every config",
            vec![
                ("tsconfig.json", format!("{{{options},\"include\":[\"lib\"]}}")),
                ("lib/other.ts", "export const x = 1;\n".to_owned()),
                ("src/area.ts", source.to_owned()),
            ],
            "is not listed in `files` or under a literal `include` entry",
        ),
    ];
    for (name, files, expected) in cases {
        let files: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        let in_process = resolution_transcript(&[IN_PROCESS], &files, "src/area.ts", "area").await;
        let moduled = resolution_transcript(&[], &files, "src/area.ts", "area").await;
        parity::assert_parity(&in_process, &moduled);
        let joined = moduled.join("\n");
        match expected {
            "semantic" => assert!(joined.contains("mode: semantic"), "{name}:\n{joined}"),
            reason => {
                assert!(joined.contains("mode: lexical"), "{name}:\n{joined}");
                assert!(joined.contains(reason), "{name} refusal reason:\n{joined}");
            }
        }
    }
}

/// The `module typescript analyzer` child that owns the bridge.
fn bridge_module(daemon: &mut Daemon) -> Option<Node> {
    daemon.tree().children.into_iter().find(|node| {
        node.command.ends_with("module typescript analyzer")
            && node
                .children
                .iter()
                .any(|(_, command)| command.contains("cli.mjs"))
    })
}

/// `kill -9` of the bridge-hosting module while idle: neither it nor its bridge survives, the next
/// semantic call is answered (a fresh module on the same daemon) or refused with the typed module
/// failure, and the daemon never restarts.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environments"]
async fn a_killed_typescript_module_is_replaced_and_leaves_nothing() {
    let fixture = fixture(json!([typescript_provider()]));
    let mut daemon = Daemon::start(&fixture, &[]).await;
    let daemon_pid = daemon.pid();
    let mut session = Session::start(&fixture).await;
    let text = std::fs::read_to_string(fixture.root.join("src/a.ts")).unwrap();
    let at = text.find("greet_ts").unwrap() + 2;
    let request = json!({"path":"src/a.ts","byte_offset":at});
    let warm = session
        .call(
            &fixture,
            source_query_tool(&request.clone()),
            request.clone(),
        )
        .await;
    assert!(
        warm["text"]
            .as_str()
            .unwrap_or_default()
            .contains("mode: semantic"),
        "{warm}"
    );
    let first = bridge_module(&mut daemon).expect("a module owns the bridge");
    // SAFETY: `first.id` is the exact module identity captured as a child of this test's daemon.
    unsafe { libc::kill(first.id.pid, libc::SIGKILL) };
    assert!(first.id.gone().await, "the killed module is reaped");
    let started = std::time::Instant::now();
    let mut after;
    loop {
        after = session
            .call(
                &fixture,
                source_query_tool(&request.clone()),
                request.clone(),
            )
            .await;
        if after["text"]
            .as_str()
            .unwrap_or_default()
            .contains("mode: semantic")
            || started.elapsed() > std::time::Duration::from_secs(40)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        after["text"]
            .as_str()
            .unwrap_or_default()
            .contains("mode: semantic"),
        "a fresh module answered: {after}"
    );
    let second = bridge_module(&mut daemon).expect("a fresh module owns a bridge");
    assert_ne!(first.id, second.id);
    assert_eq!(daemon.pid(), daemon_pid, "the daemon never restarted");
    let owned = daemon.tree().all();
    session.close(&fixture).await;
    drop(daemon);
    for id in owned.into_iter().chain(std::iter::once(first.id)) {
        assert!(id.gone().await, "{id:?} survived its daemon");
    }
}

/// The module fault cases: they need the seams only a `test-seams` build honours
/// (`cargo test --features test-seams --test module_typescript_parity -- --ignored faults`).
#[cfg(feature = "test-seams")]
mod faults {
    use super::*;

    /// The module fault seam (`agent_ide_core::modules::serve::FAULT_SEAM`).
    const FAULT_SEAM: &str = "AGENT_IDE_TEST_MODULE_FAULT";
    /// The module request budget seam (`agent_ide_core::modules::router::BUDGET_SEAM`).
    const BUDGET_SEAM: &str = "AGENT_IDE_TEST_MODULE_BUDGET_MS";

    /// A stall past the module budget, a malformed reply and a `kill -9` in the middle of a call to
    /// the bridge-hosting module each answer that call with the typed `provider_unavailable`, and the
    /// next call starts a fresh module and answers semantically on the same daemon. Neither the failed
    /// module nor its bridge survives.
    #[tokio::test]
    #[ignore = "requires the accepted Node, bridge and tsserver environments"]
    async fn typescript_analyzer_faults_are_typed_and_restart() {
        for fault in ["stall", "malformed", "kill"] {
            let fixture = fixture(json!([typescript_provider()]));
            let flag = fixture.base.join(format!("module-fault-{fault}"));
            std::fs::write(&flag, "").unwrap();
            let seam = match fault {
                "malformed" => format!("malformed:semantic:{}", flag.display()),
                _ => format!("stall:semantic:{}", flag.display()),
            };
            let budget = if fault == "stall" { "3000" } else { "20000" };
            let mut daemon =
                Daemon::start(&fixture, &[(FAULT_SEAM, &seam), (BUDGET_SEAM, budget)]).await;
            let daemon_pid = daemon.pid();
            let mut session = Session::start(&fixture).await;
            let text = std::fs::read_to_string(fixture.root.join("src/a.ts")).unwrap();
            let at = text.find("greet_ts").unwrap() + 2;
            let request = json!({"path":"src/a.ts","byte_offset":at});
            let usages = json!({"symbol":"src/a.ts#greet_ts"});
            let warm = session
                .call(&fixture, "ide.outline", json!({"path":"src/a.ts"}))
                .await;
            assert_eq!(warm["state"], "complete", "{fault}: {warm}");
            let first = bridge_module(&mut daemon).expect("a module owns the bridge");
            let started = std::time::Instant::now();
            let failed = if fault == "kill" {
                let id = first.id;
                let killer = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    if id.exists() {
                        // SAFETY: `id` is the exact module identity captured as a child of this
                        // test's daemon a moment ago.
                        unsafe { libc::kill(id.pid, libc::SIGKILL) };
                    }
                });
                let reply = session.call(&fixture, "ide.symbol", usages.clone()).await;
                killer.await.unwrap();
                reply
            } else {
                session.call(&fixture, "ide.symbol", usages.clone()).await
            };
            let failed_after = started.elapsed();
            assert_eq!(failed["code"], "provider_unavailable", "{fault}: {failed}");
            match fault {
                "stall" => assert!(
                    failed_after >= std::time::Duration::from_millis(3000)
                        && failed_after < std::time::Duration::from_secs(15),
                    "stall answered after {failed_after:?}"
                ),
                _ => assert!(
                    failed_after < std::time::Duration::from_secs(10),
                    "{fault} answered after {failed_after:?}"
                ),
            }
            let restarted = std::time::Instant::now();
            let mut recovered;
            loop {
                recovered = session
                    .call(
                        &fixture,
                        source_query_tool(&request.clone()),
                        request.clone(),
                    )
                    .await;
                if recovered["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("mode: semantic")
                    || restarted.elapsed() > std::time::Duration::from_secs(30)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            assert!(
                recovered["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("mode: semantic"),
                "{fault}: a fresh module answers semantically: {recovered}"
            );
            let second = bridge_module(&mut daemon).expect("a fresh module owns a bridge");
            assert_ne!(
                first.id, second.id,
                "{fault}: the failed module was replaced"
            );
            assert!(first.id.gone().await, "{fault}: the failed module survived");
            for (id, _) in &first.children {
                assert!(
                    id.gone().await,
                    "{fault}: the failed module's bridge survived"
                );
            }
            assert_eq!(daemon.pid(), daemon_pid, "{fault}: same daemon");
            session.close(&fixture).await;
            drop(daemon);
        }
    }

    /// A stall past the budget, a malformed reply and a `kill -9` of the checker module each leave
    /// TypeScript's check `unavailable` naming `module_unavailable (bundled.typescript:…)`, and after
    /// an edit the next check starts a fresh module and lands, on the same daemon.
    #[tokio::test]
    #[ignore = "requires the accepted Node, bridge and tsserver environments"]
    async fn typescript_checker_faults_are_typed_and_restart() {
        for (fault, expected) in [
            (
                "stall",
                "module_unavailable (bundled.typescript:request:timeout)",
            ),
            (
                "malformed",
                "module_unavailable (bundled.typescript:request:malformed)",
            ),
            (
                "kill",
                "module_unavailable (bundled.typescript:request:exited)",
            ),
        ] {
            let fixture = node_fixture(false);
            let flag = fixture.base.join(format!("module-check-{fault}"));
            std::fs::write(&flag, "").unwrap();
            let seam = match fault {
                "malformed" => format!("malformed:check_plan:{}", flag.display()),
                _ => format!("stall:check_plan:{}", flag.display()),
            };
            let mut daemon =
                Daemon::start(&fixture, &[(FAULT_SEAM, &seam), (BUDGET_SEAM, "3000")]).await;
            let daemon_pid = daemon.pid();
            let mut session = Session::start(&fixture).await;
            if fault == "kill" {
                let mut first = None;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while first.is_none() && std::time::Instant::now() < deadline {
                    first = daemon.tree().module("typescript", "checker").cloned();
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                let first = first.expect("the stalled checker module runs");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                assert!(!flag.exists(), "the stall seam fired before the kill");
                // SAFETY: `first` is the exact module identity captured as a child of this test's
                // daemon a moment ago.
                unsafe { libc::kill(first.id.pid, libc::SIGKILL) };
            }
            let failed =
                problems_page(&mut session, &fixture, |text| text.contains(expected)).await;
            assert!(
                failed.contains(expected),
                "{fault}: the check names the fault:\n{failed}"
            );
            std::fs::write(
                fixture.root.join("src/index.js"),
                format!("export function edited_{fault}() {{\n  /** @type {{number}} */\n  const n = 'x';\n  return n;\n}}\n"),
            )
            .unwrap();
            let landed = problems_page(&mut session, &fixture, |text| {
                text.contains("typescript: ready")
            })
            .await;
            assert!(
                landed.contains("typescript: ready"),
                "{fault}: the next check lands:\n{landed}"
            );
            assert_eq!(daemon.pid(), daemon_pid, "{fault}: same daemon");
            session.close(&fixture).await;
            drop(daemon);
        }
    }

    /// Polls TypeScript's problems page until `done` holds (or 40 s pass).
    async fn problems_page(
        session: &mut Session,
        fixture: &Fixture,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let mut text = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        while std::time::Instant::now() < deadline {
            let reply = session
                .call(
                    fixture,
                    source_query_tool(&json!({"kind":"problems","language":"typescript"})),
                    json!({"kind":"problems","language":"typescript"}),
                )
                .await;
            text = reply["text"].as_str().unwrap_or_default().to_owned();
            if done(&text) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        text
    }
}

/// A project whose test runner is `runner` (a `devDependencies` entry), with a fake `npx` first
/// on the daemon's `PATH` that marks any invocation, and the test replies of one daemon with `env`.
async fn runner_transcript(runner: &str, env: &[(&str, &str)]) -> (Vec<String>, bool) {
    let fixture = Fixture::new(
        &[
            (
                "package.json",
                &format!(
                    "{{\"name\":\"ui\",\"private\":true,\"devDependencies\":{{\"{runner}\":\"1.0.0\"}}}}\n"
                ),
            ),
            ("src/a.test.ts", "test('adds', () => {});\n"),
            ("src/plain.ts", "export const x = 1;\n"),
        ],
        json!([]),
    );
    let tools = fixture.base.join("fake-bin");
    std::fs::create_dir_all(&tools).unwrap();
    let script = tools.join("npx");
    let invoked = fixture.base.join("npx-invoked");
    let invoked = invoked.display().to_string().replace('\'', "'\\''");
    std::fs::write(&script, format!("#!/bin/sh\ntouch '{invoked}'\nexit 0\n")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        tools.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut daemon_env = vec![("PATH", path.as_str())];
    daemon_env.extend_from_slice(env);
    let _daemon = Daemon::start(&fixture, &daemon_env).await;
    let mut session = Session::start(&fixture).await;
    let mut replies = Vec::new();
    for arguments in [
        json!({"path":"src/a.test.ts"}),
        json!({"pattern":"adds"}),
        json!({"path":"src"}),
        json!({"path":"src/plain.ts"}),
    ] {
        let (reply, _) = run_tests(&mut session, &fixture, arguments.clone()).await;
        replies.push(line_for(&fixture, "ide.test", &arguments, &reply));
    }
    session.close(&fixture).await;
    (replies, fixture.base.join("npx-invoked").exists())
}

/// Unprepared Vitest and Jest recipes refuse consistently without invoking ambient `npx`; a plain
/// source file still reports that it has no tests.
#[tokio::test]
async fn typescript_test_runners_refuse_unprepared_packages() {
    for runner in ["vitest", "jest"] {
        let (in_process, in_process_invoked) = runner_transcript(runner, &[IN_PROCESS]).await;
        let (moduled, moduled_invoked) = runner_transcript(runner, &[]).await;
        parity::assert_parity(&in_process, &moduled);
        assert!(!in_process_invoked, "native route invoked ambient npx");
        assert!(!moduled_invoked, "module route invoked ambient npx");
        let all = moduled.join("\n");
        assert!(all.contains("provider_unavailable"), "{runner}:\n{all}");
        assert!(
            all.contains(&format!(
                "project-local {runner} runner is missing, read restricted or untrusted"
            )),
            "{runner}:\n{all}"
        );
        assert!(!all.contains("1 passed, 0 failed"), "{runner}:\n{all}");
        assert!(all.contains("no tests in src/plain.ts"), "{runner}:\n{all}");
    }
}

/// The configured checks Node (nodeA from `AGENT_IDE_NODE`) resolves and executes the project's
/// tests in BOTH hosting modes even when a conflicting ambient `node` (nodeB stub writing a
/// sentinel) leads the daemon's `PATH`: the test passes via nodeA, the nodeB sentinel is
/// absent, the process tree contains no stub, and the test file itself records
/// `process.execPath` (nodeA) and spawns a bare-`node` child whose `execPath` is also nodeA.
/// Prepared Vitest/Jest stand-ins in a contained pnpm layout exercise the real test execution
/// path and record exact file/pattern argv; missing packages refuse without ambient npx.
#[ignore = "requires accepted AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER"]
#[tokio::test]
async fn configured_node_executes_prepared_runners_without_ambient_npx() {
    let fixture = node_fixture(true);
    let sentinel = fixture.base.join("nodeB-ran");
    let ambient_bin = fixture.base.join("ambient-bin");
    std::fs::create_dir_all(&ambient_bin).unwrap();
    let stub = ambient_bin.join("node");
    let sentinel_path = sentinel.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\ntouch '{sentinel_path}'\necho 'ambient nodeB ran' >&2\nexit 1\n"),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A broken admission guard must hit this sentinel instead of a real package manager.
    std::fs::copy(&stub, ambient_bin.join("npx")).unwrap();
    let exec_recording = fixture.base.join("execPath-primary.txt");
    let child_recording = fixture.base.join("execPath-child.txt");
    let exec_recording_str = exec_recording.display().to_string();
    let child_recording_str = child_recording.display().to_string();
    // The primary writeFileSync uses a JSON-quoted path (valid JS string literal regardless
    // of special characters in the approved TMPDIR).
    let exec_recording_json =
        serde_json::to_string(&exec_recording_str).expect("recording path is a string");
    let child_recording_json =
        serde_json::to_string(&child_recording_str).expect("recording path is a string");
    // The child script is built as a plain JS string, then JSON-quoted THE ENTIRE SCRIPT so
    // it splices into the spawnSync argv array as one properly escaped JS string literal —
    // no quote nesting issues at any language boundary.
    let child_script =
        format!("require('fs').writeFileSync({child_recording_json}, process.execPath)");
    let child_script_json = serde_json::to_string(&child_script).expect("child script is a string");
    let test_source = format!(
        r#"import test from 'node:test';
import {{ spawnSync }} from 'node:child_process';
import {{ writeFileSync }} from 'node:fs';
test('records exec paths', () => {{
  writeFileSync({exec_recording}, process.execPath);
  const child = spawnSync('node', ['-e', {child_script}]);
  if (child.status !== 0) throw new Error('child failed: ' + child.stderr);
}});
"#,
        exec_recording = exec_recording_json,
        child_script = child_script_json,
    );
    put_test_file(&fixture, "test/exec-path.test.mjs", &test_source);

    let ambient_prepend = format!(
        "{}:{}",
        ambient_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let real_node = std::env::var("AGENT_IDE_NODE").unwrap();

    for mode_env in [
        vec![],           // module mode (default)
        vec![IN_PROCESS], // native mode
    ] {
        let mut env = vec![("PATH", ambient_prepend.as_str())];
        env.extend_from_slice(&mode_env);
        let _daemon = Daemon::start(&fixture, &env).await;
        let mut session = Session::start(&fixture).await;

        let _ = std::fs::remove_file(&exec_recording);
        let _ = std::fs::remove_file(&child_recording);

        let (reply, text) = run_tests(
            &mut session,
            &fixture,
            json!({"path":"test/exec-path.test.mjs"}),
        )
        .await;
        assert!(
            text.contains("1 passed, 0 failed,") && text.contains("(exit 0)"),
            "test must report a parsed terminal pass with exit 0 in {:?}: {text}\n{reply:?}",
            mode_env,
        );

        let primary = std::fs::read_to_string(&exec_recording).unwrap();
        assert_eq!(
            Path::new(&primary).canonicalize().unwrap(),
            Path::new(&real_node).canonicalize().unwrap(),
            "primary execPath must be the configured nodeA in {:?}",
            mode_env,
        );
        let child = std::fs::read_to_string(&child_recording).unwrap();
        assert_eq!(
            Path::new(&child).canonicalize().unwrap(),
            Path::new(&real_node).canonicalize().unwrap(),
            "child execPath must be the configured nodeA in {:?}",
            mode_env,
        );
        assert!(
            !sentinel.exists(),
            "ambient nodeB must never execute in {:?}: {} exists",
            mode_env,
            sentinel.display(),
        );

        let package_path = fixture.root.join("package.json");
        let mut package: Value =
            serde_json::from_str(&std::fs::read_to_string(&package_path).unwrap()).unwrap();
        for runner in ["vitest", "jest"] {
            package["devDependencies"] = json!({runner: "1.0.0"});
            package["scripts"]["test"] = json!(runner);
            package["engines"]["node"] = json!(">=16.9.0");
            std::fs::write(&package_path, package.to_string()).unwrap();
            let runner_link = fixture.root.join(format!("node_modules/{runner}"));
            let _ = std::fs::remove_file(&runner_link);
            let (_, missing) = run_tests(
                &mut session,
                &fixture,
                json!({"path":"test/exec-path.test.mjs"}),
            )
            .await;
            assert!(
                missing.contains(&format!("project-local {runner} runner is missing")),
                "{mode_env:?}: {missing}"
            );

            let relative = format!("node_modules/.pnpm/{runner}@1/node_modules/{runner}");
            put_test_file(
                &fixture,
                &format!("{relative}/package.json"),
                &format!(
                    r#"{{"name":"{runner}","bin":{{"{runner}":"bin/cli.{}"}}}}"#,
                    if runner == "vitest" { "mjs" } else { "cjs" }
                ),
            );
            let record = fixture.root.join("runner-record.json");
            let script = format!(
                r#"
{imports}
const child = spawnSync('node', ['-p', 'process.execPath'], {{encoding:'utf8'}});
if (child.status !== 0) throw new Error('child Node failed: ' + child.stderr);
writeFileSync('runner-record.json', JSON.stringify({{node:process.execPath, child:child.stdout.trim(), args:process.argv.slice(2)}}));
console.log({summary});
"#,
                imports = if runner == "vitest" {
                    "import {spawnSync} from 'node:child_process'; import {writeFileSync} from 'node:fs'; import {pathToFileURL} from 'node:url'; if (import.meta.url !== pathToFileURL(process.argv[1]).href) throw new Error('ESM entry changed');"
                } else {
                    "const {spawnSync} = require('node:child_process'); const {writeFileSync} = require('node:fs'); if (require.main !== module) throw new Error('CJS entry changed');"
                },
                summary = serde_json::to_string(if runner == "vitest" {
                    "Tests  1 passed (1)"
                } else {
                    "Tests: 1 passed, 1 total"
                })
                .unwrap()
            );
            put_test_file(
                &fixture,
                &format!(
                    "{relative}/bin/cli.{}",
                    if runner == "vitest" { "mjs" } else { "cjs" }
                ),
                &script,
            );
            std::os::unix::fs::symlink(
                format!(".pnpm/{runner}@1/node_modules/{runner}"),
                &runner_link,
            )
            .unwrap();
            put_test_file(
                &fixture,
                "-dash test.test.ts",
                "test('stand-in', () => {});\n",
            );
            for (arguments, mut expected) in [
                (
                    json!({"path":"-dash test.test.ts"}),
                    vec!["./-dash test.test.ts"],
                ),
                (
                    json!({"pattern":"-leading 🦀 [a-z]"}),
                    vec!["-t", "-leading 🦀 [a-z]"],
                ),
                (
                    json!({"path":"test/exec-path.test.mjs"}),
                    vec!["test/exec-path.test.mjs"],
                ),
            ] {
                if runner == "vitest" {
                    expected.insert(0, "run");
                }
                let _ = std::fs::remove_file(&record);
                let (_, text) = run_tests(&mut session, &fixture, arguments).await;
                assert!(
                    text.contains("1 passed, 0 failed,") && text.contains("(exit 0)"),
                    "{runner} {mode_env:?}: {text}"
                );
                let observed: Value =
                    serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                assert_eq!(observed["args"], json!(expected), "{runner} {mode_env:?}");
                for field in ["node", "child"] {
                    assert_eq!(
                        Path::new(observed[field].as_str().unwrap())
                            .canonicalize()
                            .unwrap(),
                        Path::new(&real_node).canonicalize().unwrap(),
                        "{runner} {field} {mode_env:?}"
                    );
                }
                assert!(
                    !sentinel.exists(),
                    "ambient Node/npx executed: {runner} {mode_env:?}"
                );
            }
            for requirement in [">=999", "unknown-node-alias", ""] {
                package["engines"]["node"] = json!(requirement);
                std::fs::write(&package_path, package.to_string()).unwrap();
                std::fs::remove_file(&record).unwrap();
                let (_, refused) = run_tests(
                    &mut session,
                    &fixture,
                    json!({"path":"test/exec-path.test.mjs"}),
                )
                .await;
                assert!(
                    refused.contains("environment unavailable (exit 78)")
                        && !refused.contains("0 passed"),
                    "{runner} {requirement} {mode_env:?}: {refused}"
                );
                assert!(!record.exists(), "refused environment loaded {runner}");
                // Keep the next iteration's record removal deterministic without executing the runner.
                std::fs::write(&record, "refused").unwrap();
            }
            package["engines"]["node"] = json!(">=16.9.0");
            std::fs::write(&package_path, package.to_string()).unwrap();
            std::fs::remove_file(&record).unwrap();
            let tsc = PathBuf::from(std::env::var("AGENT_IDE_TSSERVER").unwrap())
                .with_file_name("tsc.js");
            let config = agent_ide_core::modules::contract::ModuleConfig {
                checks: Some(json!({"node":real_node,"tsc_cli":tsc})),
                provider: Some(json!({"node":accepted_program(&real_node,"24.4.0")})),
                ..Default::default()
            };
            let argv = if runner == "vitest" {
                vec![
                    runner.into(),
                    "run".into(),
                    "test/exec-path.test.mjs".into(),
                ]
            } else {
                vec![runner.into(), "test/exec-path.test.mjs".into()]
            };
            for case in ["unknown-identity", "denied-verifier", "denied-runner"] {
                let mut rejected = config.clone();
                match case {
                    "unknown-identity" => {
                        rejected.provider.as_mut().unwrap()["node"]["identity"] = json!("unknown")
                    }
                    "denied-verifier" => {
                        rejected.read_denies =
                            vec![agent_ide_core::execution::seatbelt::ReadDeny::Path(
                                tsc.with_file_name("typescript.js"),
                            )]
                    }
                    _ => {
                        rejected.read_denies =
                            vec![agent_ide_core::execution::seatbelt::ReadDeny::Path(
                                runner_link.canonicalize().unwrap(),
                            )]
                    }
                }
                assert!(
                    agent_ide_lang_typescript::environment::command_argv(
                        &fixture.root,
                        &argv,
                        &rejected
                    )
                    .is_err(),
                    "native {case}"
                );
                assert!(
                    agent_ide_lang_typescript::module::configured_effect(
                        &argv,
                        &rejected,
                        &fixture.root
                    )
                    .is_err(),
                    "module {case}"
                );
                assert!(!record.exists(), "{case} loaded {runner}");
            }
            std::fs::remove_file(&runner_link).unwrap();
        }
        package["devDependencies"] = serde_json::json!({});
        package["scripts"]["test"] = serde_json::json!("node --test");
        package.as_object_mut().unwrap().remove("engines");
        std::fs::write(&package_path, package.to_string()).unwrap();
        session.close(&fixture).await;
    }
    let _ = std::fs::remove_dir_all(&fixture.base);
}

/// Writes a test file into the fixture's worktree, creating parents.
fn put_test_file(fixture: &Fixture, relative: &str, content: &str) {
    let path = fixture.root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}
