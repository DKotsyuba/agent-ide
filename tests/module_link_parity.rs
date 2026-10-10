//! The cross-language bridge answers are byte-identical however the front-end languages compute:
//! every combination of HTML and CSS in their modules or in process (TypeScript follows its own
//! switch) gives the same `ide.symbol` cards with `defines:`/`links:`, `ide.graph` link leaves,
//! sigil and bare-name lookups, `ide.read` of a sigil address, the `ide.start` links line, the new
//! file-reference and CSS-module rows, and the same answers after an edit and after the target of
//! a reference is deleted. A language whose module is down is listed as unavailable instead of
//! reading as "no links".

#[path = "support/parity.rs"]
mod parity;

use parity::{Daemon, Fixture, LANGUAGE_MODE, Session, line_for};
use serde_json::{Value, json};

/// Every language in process: the reference transcript.
const ALL_IN_PROCESS: &str = "html=in_process,css=in_process,typescript=in_process";
/// Mixed settings compared against it: each web language in its module alone, the style and
/// markup languages together, TypeScript alone, and every language in its module.
const MIXED: [&str; 5] = [
    "css=in_process,typescript=in_process",
    "html=in_process,typescript=in_process",
    "typescript=in_process",
    "html=in_process,css=in_process",
    "",
];

/// The `mixed-frontend` tree with an assets container holding the file references, a script that
/// imports, and a CSS module with a script that reads it.
fn fixture() -> Fixture {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed-frontend");
    let read = |name: &str| std::fs::read_to_string(source.join(name)).unwrap();
    let page = read("index.html").replace(
        "<main id=\"main\"",
        "<div id=\"assets\"><link rel=\"stylesheet\" href=\"styles.css\"><script src=\"js/app.js\"></script><script src=\"js/missing.js\"></script><script src=\"https://cdn.example/x.js\"></script></div>\n<main id=\"main\"",
    );
    Fixture::new(
        &[
            ("index.html", &page),
            ("styles.css", &read("styles.css")),
            ("theme.scss", &read("theme.scss")),
            ("js/app.js", "import './lib';\n"),
            ("js/lib.js", "export const lib = 1;\n"),
            (
                "ui/Panel.jsx",
                "import s from './Panel.module.css';\nexport const Panel = () => <p className={s.panel}>x</p>;\n",
            ),
            ("ui/Panel.module.css", ".panel { color: red; }\n"),
            ("ui/Other.module.css", ".panel { color: blue; }\n"),
        ],
        json!([]),
    )
}

/// The read-only calls whose replies must not depend on where a language computes.
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("ide.symbol", json!({"symbol":"styles.css#.btn"})),
        ("ide.symbol", json!({"symbol":"index.html#div#assets"})),
        ("ide.symbol", json!({"symbol":"index.html#main#main"})),
        ("ide.symbol", json!({"symbol":"##main"})),
        ("ide.symbol", json!({"symbol":".btn"})),
        ("ide.symbol", json!({"symbol":".panel"})),
        ("ide.symbol", json!({"symbol":"--brand"})),
        ("ide.symbol", json!({"symbol":"btn"})),
        ("ide.read", json!({"symbol":".btn"})),
        ("ide.read", json!({"symbol":"##main"})),
        (
            "ide.graph",
            json!({"symbol":"styles.css#.btn","direction":"callers","depth":2}),
        ),
        (
            "ide.graph",
            json!({"symbol":"index.html#div#assets","direction":"callees","depth":2}),
        ),
        (
            "ide.graph",
            json!({"symbol":"index.html#main#main","direction":"callees","depth":2}),
        ),
    ]
}

/// The activation card and `calls()` on a fresh daemon with the mode `env` value, then the same
/// card after the referenced script is deleted.
async fn run(mode: &str) -> Vec<String> {
    let fixture = fixture();
    let env: Vec<(&str, &str)> = if mode.is_empty() {
        Vec::new()
    } else {
        vec![(LANGUAGE_MODE, mode)]
    };
    let _daemon = Daemon::start(&fixture, &env).await;
    let mut session = Session::start(&fixture).await;
    let mut replies = Vec::new();
    let started = session
        .call(&fixture, "ide.start", json!({"activation_id":"links"}))
        .await;
    replies.push(line_for(&fixture, "ide.start", &json!({}), &started));
    for (tool, arguments) in calls() {
        let reply = session.call(&fixture, tool, arguments.clone()).await;
        replies.push(line_for(&fixture, tool, &arguments, &reply));
    }
    // A deletion and a native edit are visible to the very next query, in every mode.
    std::fs::remove_file(fixture.root.join("js/app.js")).unwrap();
    let card = json!({"symbol":"index.html#div#assets"});
    let after = session.call(&fixture, "ide.symbol", card.clone()).await;
    replies.push(line_for(&fixture, "ide.symbol", &card, &after));
    let edited = std::fs::read_to_string(fixture.root.join("index.html"))
        .unwrap()
        .replace("<script src=\"js/app.js\"></script>", "");
    std::fs::write(fixture.root.join("index.html"), edited).unwrap();
    let after = session.call(&fixture, "ide.symbol", card.clone()).await;
    replies.push(line_for(&fixture, "ide.symbol", &card, &after));
    session.close(&fixture).await;
    replies
}

/// Module versus in-process answers are equal for every mixed setting, and the new rows are there:
/// the assets container links its script and style sheet (the missing script reads "no indexed
/// file", the network one is not listed) and the CSS-module class joins only its own module.
#[tokio::test]
async fn the_bridge_answers_are_identical_in_every_mixed_setting() {
    let reference = run(ALL_IN_PROCESS).await;
    let card = &reference[2];
    assert!(
        card.contains("links: 3 file references used here"),
        "{card}"
    );
    assert!(card.contains("js/app.js"), "{card}");
    assert!(card.contains("js/missing.js"), "{card}");
    assert!(card.contains("no indexed file"), "{card}");
    assert!(!card.contains("cdn.example"), "{card}");
    let panel = reference
        .iter()
        .find(|reply| reply.contains("\"symbol\":\".panel\""))
        .expect("the .panel name card");
    assert!(panel.contains("ui/Panel.module.css"), "{panel}");
    assert!(panel.contains("ui/Panel.jsx"), "{panel}");
    for mode in MIXED {
        parity::assert_parity(&reference, &run(mode).await);
    }
    // The deleted target reads "no indexed file"; the removed tag takes its row away.
    let (deleted, removed) = (
        &reference[reference.len() - 2],
        &reference[reference.len() - 1],
    );
    assert!(
        deleted.contains("js/app.js") && deleted.matches("no indexed file").count() == 2,
        "{deleted}"
    );
    assert!(!removed.contains("js/app.js"), "{removed}");
}
