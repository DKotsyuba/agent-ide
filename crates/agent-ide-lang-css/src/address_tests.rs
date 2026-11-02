//! CSS conformance for `symbol-address-v1`: region fixture and the repository sweep.

use std::path::Path;

use agent_ide_core::lang::address::{AddressMap, conformance_sweep};

use crate::LANGUAGE;

/// `first-last address` rows of every unit of `source`, in file order.
fn rows(source: &str) -> Vec<String> {
    let outline = LANGUAGE
        .support()
        .outline_from_source(Path::new("f.css"), source)
        .expect("css has a source outline");
    let map = AddressMap::build(&outline, source);
    assert_eq!(map.violations(source), Vec::<String>::new());
    map.units()
        .iter()
        .map(|u| format!("{}-{} {}", u.span.start, u.span.end, u.address))
        .collect()
}

/// Every line of every style sheet of this repository has exactly one address.
#[test]
fn address_totality_css_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (_, failures) = conformance_sweep(&root, LANGUAGE);
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(10)].join("\n")
    );
}

/// Charset header, import run and rules.
#[test]
fn address_regions_css() {
    let source = "@charset \"utf-8\";\n/* c */\n@import \"a.css\";\n@import \"b.css\";\n\n:root {\n  --brand: red;\n}\n\n/* rules */\n.card {\n  color: red;\n}\n";
    assert_eq!(
        rows(source),
        [
            "1-2 f.css#@header",
            "3-4 f.css#@imports",
            "5-5 f.css#@after/@imports",
            "6-8 f.css#:root",
            "9-10 f.css#@after/:root",
            "11-13 f.css#.card",
        ]
    );
}
