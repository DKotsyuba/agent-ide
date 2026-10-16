//! The related-links block of Read and positional Symbol: cross-language names of the file or
//! requested position) with the same rows, uncertainty labels and notes as a symbol card, the
//! same answer whether HTML and CSS compute in their modules or in process, and no block at all
//! for a file without name facts.

use parity::source_query_tool;
#[path = "support/parity.rs"]
mod parity;

use parity::{Daemon, Fixture, LANGUAGE_MODE, Session, line_for};
use serde_json::{Value, json};

/// The `mixed-frontend` tree with an assets container holding the file references, a script that
/// imports, and a CSS module with a script that reads it.
fn fixture() -> Fixture {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed-frontend");
    let read = |name: &str| std::fs::read_to_string(source.join(name)).unwrap();
    let page = page();
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
            ("notes.txt", "plain text\n"),
        ],
        json!([]),
    )
}

/// The fixture's page: the original with the assets container before `<main>`.
fn page() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed-frontend/index.html"),
    )
    .unwrap()
    .replace(
        "<main id=\"main\"",
        "<div id=\"assets\"><link rel=\"stylesheet\" href=\"styles.css\"><script src=\"js/app.js\"></script><script src=\"js/missing.js\"></script><script src=\"https://cdn.example/x.js\"></script></div>\n<main id=\"main\"",
    )
}

/// Reads a complete comparison sweep on the same warmed fixture/session. Forced source reads
/// measure actual related-link parity instead of an unchanged-body acknowledgement.
async fn context(fixture: &Fixture, session: &mut Session, requests: &[Value]) -> Vec<String> {
    let mut replies = Vec::new();
    for request in requests {
        let tool = source_query_tool(request);
        let mut query = request.clone();
        if tool == "ide.read" {
            query["force"] = json!(true);
        }
        let reply = session.call(fixture, tool, query).await;
        replies.push(line_for(fixture, tool, request, &reply));
    }
    replies
}

/// The requests whose replies must not depend on where a language computes: whole files of every
/// kind, the line under a position, and a file without name facts.
fn requests(page: &str) -> Vec<Value> {
    let at = page.find("<main").expect("the main element") + 2;
    vec![
        json!({"path":"index.html"}),
        json!({"path":"styles.css"}),
        json!({"path":"ui/Panel.jsx"}),
        json!({"path":"ui/Panel.module.css"}),
        json!({"path":"notes.txt"}),
        json!({"path":"index.html","byte_offset":at}),
    ]
}

/// Warms one real daemon/session until its related-link index settles or the existing forty
/// second limit expires. Retries retain the warming work rather than repeatedly restarting it.
async fn settled(mode: &str, requests: &[Value]) -> Vec<String> {
    let fixture = fixture();
    let env = if mode.is_empty() {
        vec![]
    } else {
        vec![(LANGUAGE_MODE, mode)]
    };
    let _daemon = Daemon::start(&fixture, &env).await;
    let mut session = Session::start(&fixture).await;
    let started = std::time::Instant::now();
    let replies = loop {
        let replies = context(&fixture, &mut session, requests).await;
        if replies
            .iter()
            .all(|reply| !reply.contains("unavailable for:"))
            || started.elapsed() > std::time::Duration::from_secs(40)
        {
            break replies;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    session.close(&fixture).await;
    replies
}

/// The block lists what the file defines and links with the card's rows and labels, is the same
/// in every mixed setting, follows the requested position, and is absent without name facts.
#[tokio::test]
async fn the_related_links_block_is_identical_in_every_mixed_setting() {
    let requests = requests(&page());
    let reference = settled(
        "html=in_process,css=in_process,typescript=in_process",
        &requests,
    )
    .await;
    let (html, css, jsx, module_css, notes, position) = (
        &reference[0],
        &reference[1],
        &reference[2],
        &reference[3],
        &reference[4],
        &reference[5],
    );
    // HTML: ids it defines with their usages, the names and files it uses; the missing script
    // reads "no indexed file", the network one is not listed.
    assert!(
        html.contains("related_links:\n  defines: element id assets\n  defines: element id main"),
        "{html}"
    );
    assert!(
        html.contains("js/app.js") && html.contains("js/missing.js"),
        "{html}"
    );
    assert!(html.contains("js/missing.js  → no indexed file"), "{html}");
    assert!(
        !html.contains("cdn.example\n") && !html.contains("  cdn.example"),
        "{html}"
    );
    // CSS: classes with the heuristic template use labelled as such.
    assert!(css.contains("defines: class name btn"), "{css}");
    assert!(
        css.contains("[html ~template .btn]") && css.contains("(~ = heuristic match, not proven)"),
        "{css}"
    );
    // The script reads its CSS module's member; the module defines it in its own domain.
    assert!(jsx.contains(".panel in ui/Panel.module.css"), "{jsx}");
    assert!(
        module_css.contains("defines: class name panel (in ui/Panel.module.css)"),
        "{module_css}"
    );
    assert!(module_css.contains("ui/Panel.jsx:2"), "{module_css}");
    // No name facts, no block; a position keeps only its own line.
    assert!(!notes.contains("related_links"), "{notes}");
    assert!(
        position.contains("related_links:\n") && position.contains("defines: element id main"),
        "{position}"
    );
    let block = &position[position.find("related_links:").unwrap()..];
    assert!(
        !block[..block.find("\n\n").unwrap()].contains("js/app.js"),
        "{position}"
    );
    // Every mixed setting answers byte for byte like the reference.
    for mode in [
        "css=in_process,typescript=in_process",
        "html=in_process,typescript=in_process",
        "typescript=in_process",
        "html=in_process,css=in_process",
        "",
    ] {
        parity::assert_parity(&reference, &settled(mode, &requests).await);
    }
}

/// Every usage row stays in the block, past the card's ceiling of 30: the rows that do not fit the
/// first reply page follow on the reply's own `detail_ref` pages. Read keeps its exact numbered
/// source before the links, and the fully delivered reference still authorizes the same bytes.
#[tokio::test]
async fn long_usage_lists_continue_on_the_replys_own_pages() {
    const FILES: usize = 1200;
    let mut files: Vec<(String, String)> = (0..FILES)
        .map(|n| {
            (
                format!("pages/p{n:04}.html"),
                "<p class=\"wide\">x</p>\n".to_owned(),
            )
        })
        .collect();
    let source = ".wide { margin: 0; }\n";
    files.push(("wide.css".to_owned(), source.to_owned()));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let fixture = Fixture::new(&refs, json!([]));
    let _daemon = Daemon::start(&fixture, &[]).await;
    let mut session = Session::start(&fixture).await;
    let started = std::time::Instant::now();
    let first = loop {
        let reply = session
            .call(
                &fixture,
                source_query_tool(&json!({"path":"wide.css"})),
                json!({"path":"wide.css"}),
            )
            .await;
        let text = reply["text"].as_str().unwrap_or_default();
        if !text.contains("unavailable for:")
            || started.elapsed() > std::time::Duration::from_secs(40)
        {
            break reply;
        }
    };
    let mut text = first["text"].as_str().unwrap().to_owned();
    assert!(
        text.contains(&format!("usages: {FILES} indexed in {FILES} files")),
        "{text}"
    );
    assert!(!text.contains("… "), "no cut row: {text}");
    // The rows exceed one page: the reply says so and `ide.inspect` serves the rest.
    assert_eq!(first["continuation"], true, "{first}");
    let mut reply = first.clone();
    let mut pages = 1;
    while reply["continuation"] == true {
        let reference = reply["detail_ref"].as_str().unwrap().to_owned();
        reply = session
            .call(&fixture, "ide.inspect", json!({"detail_ref": reference}))
            .await;
        text.push_str(reply["text"].as_str().unwrap());
        pages += 1;
        assert!(pages < 20, "{reply}");
    }
    assert!(pages > 1);
    for n in [0, 29, 30, FILES - 1] {
        assert!(text.contains(&format!("pages/p{n:04}.html:1")), "p{n}");
    }
    assert_eq!(
        text.matches(".html:1  [html] <p").count(),
        FILES,
        "every row once"
    );
    // Read preserves its numbered source before the links, without losing any source byte.
    let numbered = format!("wide.css  (lines 1)\n1\t{source}");
    let source_at = text.find(&numbered).expect("complete numbered source");
    let links_at = text.find("related_links:").expect("retained link rows");
    assert!(source_at < links_at, "{text}");
    // The actual delivered read reference still binds the file after all link pages are fetched.
    let reference = first["detail_ref"].as_str().unwrap();
    let edited = ".wide { margin: 1px; }\n";
    let edit = session
        .call(
            &fixture,
            "ide.edit",
            json!({"operation_id":"wide","path":"wide.css","source_ref":reference,"content":edited}),
        )
        .await;
    assert_eq!(edit["result"]["outcome"], "replaced", "{edit}");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("wide.css")).unwrap(),
        edited
    );
    session.close(&fixture).await;
}

/// A killed HTML module never reads as "no links": the style sheet's own names keep answering,
/// any reply produced while the module is down names `html` as unavailable, and the block of the
/// restarted module lists the page's usage again.
#[tokio::test]
async fn a_downed_html_module_is_disclosed_and_the_block_recovers() {
    let fixture = fixture();
    let mut daemon = Daemon::start(&fixture, &[]).await;
    let mut session = Session::start(&fixture).await;
    let request = json!({"path":"styles.css"});
    let usage = "index.html:";
    let started = std::time::Instant::now();
    let warm = loop {
        let reply = session
            .call(
                &fixture,
                source_query_tool(&request.clone()),
                request.clone(),
            )
            .await;
        let text = reply["text"].as_str().unwrap_or_default().to_owned();
        if text.contains(usage) || started.elapsed() > std::time::Duration::from_secs(40) {
            break text;
        }
    };
    assert!(warm.contains("[html ~template .btn]"), "{warm}");
    let module = daemon
        .tree()
        .module("html", "analyzer")
        .cloned()
        .expect("the HTML module runs");
    // SAFETY: `module.id` is the exact module identity captured as a child of this test's daemon.
    unsafe { libc::kill(module.id.pid, libc::SIGKILL) };
    assert!(module.id.gone().await, "the killed module is reaped");
    std::fs::write(
        fixture.root.join("index.html"),
        "<main id=\"main\"><p class=\"btn\">x</p></main>\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    loop {
        let reply = session
            .call(
                &fixture,
                source_query_tool(&request.clone()),
                request.clone(),
            )
            .await;
        let text = reply["text"].as_str().unwrap_or_default().to_owned();
        assert_eq!(reply["kind"], "read", "{reply}");
        assert!(text.contains("defines: class name btn"), "{text}");
        if text.contains("unavailable for:") {
            assert!(text.contains("unavailable for: html"), "{text}");
        } else if text.contains("index.html:1") {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(40),
            "the block never recovered: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    session.close(&fixture).await;
}

/// A usage whose source line ends in the card's own marker words is a row like any other: the
/// block splices the hidden rows at the real marker line only.
#[tokio::test]
async fn a_snippet_that_looks_like_the_cut_marker_stays_intact() {
    const FILES: usize = 31;
    let mut files: Vec<(String, String)> = (0..FILES)
        .map(|n| {
            let tail = if n == 0 { "  … 1 more" } else { "" };
            (
                format!("pages/p{n:02}.html"),
                format!("<p class=\"wide\">x</p>{tail}\n"),
            )
        })
        .collect();
    files.push(("wide.css".to_owned(), ".wide { margin: 0; }\n".to_owned()));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let fixture = Fixture::new(&refs, json!([]));
    let _daemon = Daemon::start(&fixture, &[]).await;
    let mut session = Session::start(&fixture).await;
    let started = std::time::Instant::now();
    let text = loop {
        let reply = session
            .call(
                &fixture,
                source_query_tool(&json!({"path":"wide.css"})),
                json!({"path":"wide.css"}),
            )
            .await;
        let text = reply["text"].as_str().unwrap_or_default().to_owned();
        if !text.contains("unavailable for:")
            || started.elapsed() > std::time::Duration::from_secs(40)
        {
            break text;
        }
    };
    assert!(
        text.contains("pages/p00.html:1  [html] <p class=\"wide\">x</p>  … 1 more\n"),
        "{text}"
    );
    assert_eq!(text.matches(".html:1  [html] <p").count(), FILES, "{text}");
    session.close(&fixture).await;
}
