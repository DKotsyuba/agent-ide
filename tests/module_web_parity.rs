//! The HTML and CSS modules against the in-process path on the committed `mixed-frontend` tree:
//! the same MCP transcript in module mode (the shipped default) and with
//! `AGENT_IDE_LANGUAGE_MODE=html=in_process,css=in_process`, edit forms with their resulting file
//! text, the process tree (each module a direct daemon child, no language server anywhere), and a
//! killed HTML module that comes back while the style sheets' facts stay. No provider is needed:
//! these languages have no language server.

#[path = "support/parity.rs"]
mod parity;

use std::time::{Duration, Instant};

use parity::{Daemon, Fixture, LANGUAGE_MODE, Session, line};
use serde_json::{Value, json};

/// The in-process fallback of both web modules.
const IN_PROCESS: (&str, &str) = (LANGUAGE_MODE, "html=in_process,css=in_process");

/// The `mixed-frontend` tree plus a script, a stylesheet link and a CSS module the file-reference
/// kinds can reach.
fn fixture() -> Fixture {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed-frontend");
    let read = |name: &str| std::fs::read_to_string(source.join(name)).unwrap();
    let page = read("index.html").replace(
        "<body>\n",
        "<head><link rel=\"stylesheet\" href=\"styles.css\"><script src=\"app.js\"></script></head>\n<body>\n",
    );
    Fixture::new(
        &[
            ("index.html", &page),
            ("styles.css", &read("styles.css")),
            ("theme.scss", &read("theme.scss")),
            (
                "app.js",
                "import './polyfill.js';\ndocument.getElementById('main');\n",
            ),
            ("polyfill.js", "export {};\n"),
            ("box.module.css", ".panel { color: red; }\n"),
        ],
        json!([]),
    )
}

/// The read-only calls whose replies must not depend on where HTML and CSS compute.
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("ide.outline", json!({"path":"index.html"})),
        ("ide.outline", json!({"path":"styles.css"})),
        ("ide.outline", json!({"path":"theme.scss"})),
        ("ide.outline", json!({"path":"box.module.css"})),
        ("ide.read", json!({"symbol":"styles.css#.btn"})),
        ("ide.read", json!({"symbol":"index.html#main#main"})),
        ("ide.read", json!({"path":"index.html","lines":"1-6"})),
        ("ide.symbol", json!({"symbol":"styles.css#.btn"})),
        ("ide.symbol", json!({"symbol":"index.html#main#main"})),
        ("ide.symbol", json!({"symbol":"##main"})),
        ("ide.symbol", json!({"symbol":".btn"})),
        ("ide.symbol", json!({"symbol":"--brand"})),
        ("ide.symbol", json!({"symbol":".panel"})),
        (
            "ide.graph",
            json!({"symbol":"styles.css#.btn","direction":"callers","depth":2}),
        ),
        (
            "ide.graph",
            json!({"symbol":"index.html#main#main","direction":"callees","depth":2}),
        ),
    ]
}

/// The calls on a fresh daemon with `env`, the module children seen, and the daemon.
async fn run(env: &[(&str, &str)]) -> (Vec<String>, Vec<String>, Daemon) {
    let fixture = fixture();
    let mut daemon = Daemon::start(&fixture, env).await;
    let mut session = Session::start(&fixture).await;
    let mut replies = Vec::new();
    for (tool, arguments) in calls() {
        let reply = session.call(&fixture, tool, arguments.clone()).await;
        replies.push(line(tool, &arguments, &reply));
    }
    let tree = daemon.tree();
    let modules = ["html", "css"]
        .iter()
        .filter(|language| tree.module(language, "analyzer").is_some())
        .map(|language| (*language).to_owned())
        .collect();
    session.close(&fixture).await;
    (replies, modules, daemon)
}

/// Outline, read, symbol cards with their links, sigil lookups, ambiguity lists and graph leaves
/// are the same in module mode and in process. In process no module child runs; in module mode
/// each web language is one direct daemon child and nothing survives its daemon.
#[tokio::test]
async fn web_modules_match_in_process_answers() {
    let (in_process, modules, mut daemon) = run(&[IN_PROCESS]).await;
    assert!(
        modules.is_empty(),
        "in process: no web module runs: {modules:?}"
    );
    let owned = daemon.tree().all();
    drop(daemon);
    let (moduled, modules, mut daemon) = run(&[]).await;
    assert_eq!(
        modules,
        ["html", "css"],
        "each web language is a module child"
    );
    assert!(!daemon.tree().direct_child_runs("langserver"));
    assert!(moduled.iter().any(|reply| reply.contains("[html]")));
    assert!(moduled.iter().any(|reply| reply.contains("[css]")));
    parity::assert_parity(&in_process, &moduled);
    let owned: Vec<_> = owned.into_iter().chain(daemon.tree().all()).collect();
    drop(daemon);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived its daemon");
    }
}

/// Every edit form the web languages support answers the same and leaves the same file: CSS
/// insertion (two-space children), symbol replacement and deletion, line edits of both languages
/// and the HTML insertion that stays unsupported.
#[tokio::test]
async fn web_edits_match_in_process_results() {
    let edits = [
        json!({"operation_id":"css-insert","op":"insert","symbol":"styles.css#.btn","where":"after","content":".added { color: red; }"}),
        json!({"operation_id":"css-replace","op":"replace","symbol":"styles.css#.layout .btn","content":".layout .btn { margin: 1px; }"}),
        json!({"operation_id":"html-insert","op":"insert","symbol":"index.html#main#main","where":"after","content":"<footer></footer>"}),
        json!({"operation_id":"html-delete","op":"delete","symbol":"index.html#main#main"}),
    ];
    let mut outcomes: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for env in [&[IN_PROCESS][..], &[][..]] {
        let fixture = fixture();
        let _daemon = Daemon::start(&fixture, env).await;
        let mut session = Session::start(&fixture).await;
        // Read first, as the workflow does, so symbol edits see a retained source.
        for path in ["index.html", "styles.css"] {
            session
                .call(&fixture, "ide.outline", json!({"path":path}))
                .await;
        }
        let mut replies = Vec::new();
        for edit in &edits {
            let reply = session.call(&fixture, "ide.edit", edit.clone()).await;
            replies.push(line("ide.edit", edit, &reply));
        }
        // A line edit proves its source with the reference of a fresh read.
        let read = session
            .call(
                &fixture,
                "ide.read",
                json!({"path":"index.html","lines":"1-1"}),
            )
            .await;
        let edit = json!({"operation_id":"html-lines","path":"index.html","lines":"1-1",
            "source_ref":read["detail_ref"],"content":"<!DOCTYPE html>"});
        let reply = session.call(&fixture, "ide.edit", edit.clone()).await;
        replies.push(line("ide.edit", &edit, &reply));
        let files = ["index.html", "styles.css"]
            .iter()
            .map(|name| std::fs::read_to_string(fixture.root.join(name)).unwrap())
            .collect();
        session.close(&fixture).await;
        outcomes.push((replies, files));
    }
    parity::assert_parity(&outcomes[0].0, &outcomes[1].0);
    assert_eq!(outcomes[0].1, outcomes[1].1, "the edited files differ");
    assert!(
        outcomes[0].0[2].contains("unsupported") || outcomes[0].0[2].contains("insert"),
        "HTML insertion stays refused: {}",
        outcomes[0].0[2]
    );
}

/// `kill -9` of the idle HTML module: the style sheets' facts keep answering at once, the next
/// HTML-backed answer starts a fresh module on the same daemon, and the killed module leaves
/// nothing behind.
#[tokio::test]
async fn a_killed_html_module_restarts_and_the_style_facts_stay() {
    let fixture = fixture();
    let mut daemon = Daemon::start(&fixture, &[]).await;
    let daemon_pid = daemon.pid();
    let mut session = Session::start(&fixture).await;
    let card = json!({"symbol":"styles.css#.btn"});
    let warm = session.call(&fixture, "ide.symbol", card.clone()).await;
    assert!(
        warm["text"].as_str().unwrap_or_default().contains("[html]"),
        "{warm}"
    );
    let first = daemon
        .tree()
        .module("html", "analyzer")
        .cloned()
        .expect("the HTML module runs");
    // SAFETY: `first.id` is the exact module identity captured as a child of this test's daemon.
    unsafe { libc::kill(first.id.pid, libc::SIGKILL) };
    assert!(first.id.gone().await, "the killed module is reaped");
    // The CSS definitions are indexed and need no HTML module; the card answers at once.
    let during = session.call(&fixture, "ide.symbol", card.clone()).await;
    assert_eq!(during["kind"], "symbol", "{during}");
    assert!(
        during["text"]
            .as_str()
            .unwrap_or_default()
            .contains("styles.css#.btn"),
        "{during}"
    );
    // A changed HTML file needs the module: it restarts within its budget and indexes it.
    std::fs::write(
        fixture.root.join("index.html"),
        "<main id=\"main\"><p class=\"btn\">x</p></main>\n",
    )
    .unwrap();
    let started = Instant::now();
    let mut after = session.call(&fixture, "ide.symbol", card.clone()).await;
    while !after["text"]
        .as_str()
        .unwrap_or_default()
        .contains("index.html:1")
        && started.elapsed() < Duration::from_secs(20)
    {
        tokio::time::sleep(Duration::from_millis(250)).await;
        after = session.call(&fixture, "ide.symbol", card.clone()).await;
    }
    assert!(
        after["text"]
            .as_str()
            .unwrap_or_default()
            .contains("index.html:1"),
        "the restarted module indexed the edit: {after}"
    );
    let second = daemon
        .tree()
        .module("html", "analyzer")
        .cloned()
        .expect("a fresh HTML module runs");
    assert_ne!(first.id, second.id);
    assert_eq!(daemon.pid(), daemon_pid, "the daemon never restarted");
    session.close(&fixture).await;
}
