//! HTML conformance for `symbol-address-v1`: region fixture and the repository sweep.

use std::path::Path;

use agent_ide_core::lang::address::{AddressMap, conformance_sweep};

use crate::LANGUAGE;

/// `first-last address` rows of every unit of `source`, in file order.
fn rows(source: &str) -> Vec<String> {
    let outline = LANGUAGE
        .support()
        .outline_from_source(Path::new("f.html"), source)
        .expect("html has a source outline");
    let map = AddressMap::build(&outline, source);
    assert_eq!(map.violations(source), Vec::<String>::new());
    map.units()
        .iter()
        .map(|u| format!("{}-{} {}", u.span.start, u.span.end, u.address))
        .collect()
}

/// Every line of every HTML file of this repository has exactly one address.
#[test]
fn address_totality_html_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (_, failures) = conformance_sweep(&root, LANGUAGE);
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(10)].join("\n")
    );
}

/// Doctype header and a document with text outside outlined elements.
#[test]
fn address_regions_html() {
    let source = "<!doctype html>\n<!-- c -->\n<html>\n<body id=\"main\">\n<p>hi</p>\n</body>\n</html>\n\ntrailing text\n";
    assert_eq!(
        rows(source),
        [
            "1-2 f.html#@header",
            "3-3 f.html#@code/<html>",
            "4-6 f.html#body#main",
            "7-7 f.html#@code/<%2Fhtml>",
            "8-8 f.html#@after/@code%2F<%252Fhtml>",
            "9-9 f.html#@code/trailing text",
        ]
    );
}
