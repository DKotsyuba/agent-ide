//! TypeScript conformance for `symbol-address-v1`: region fixture and the repository sweep.

use std::path::Path;

use agent_ide_core::lang::address::{AddressMap, conformance_sweep};

use crate::LANGUAGE;

/// `first-last address` rows of every unit of `source`, in file order.
fn rows(source: &str) -> Vec<String> {
    let outline = LANGUAGE
        .support()
        .outline_from_source(Path::new("f.ts"), source)
        .expect("typescript has a source outline");
    let map = AddressMap::build(&outline, source);
    assert_eq!(map.violations(source), Vec::<String>::new());
    map.units()
        .iter()
        .map(|u| format!("{}-{} {}", u.span.start, u.span.end, u.address))
        .collect()
}

/// Every line of every `.ts`/`.js` file of this repository has exactly one address.
#[test]
fn address_totality_typescript_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (_, failures) = conformance_sweep(&root, LANGUAGE);
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(10)].join("\n")
    );
}

/// Directive header, imports, constants and expression statements.
#[test]
fn address_regions_typescript() {
    let source = "\"use strict\";\n// c\nimport { a } from \"a\";\nimport \"side\";\n\nexport const LIMIT = 4;\n\nmodule.exports = {\n  x: 1,\n};\n\n/** Run. */\nexport function run() {\n  return 1;\n}\n";
    // The JSDoc block belongs to `run`, as it does in a server outline.
    assert_eq!(
        rows(source),
        [
            "1-2 f.ts#@header",
            "3-4 f.ts#@imports",
            "5-5 f.ts#@after/@imports",
            "6-6 f.ts#LIMIT",
            "7-7 f.ts#@after/LIMIT",
            "8-10 f.ts#@code/module.exports",
            "11-11 f.ts#@after/@code%2Fmodule.exports",
            "12-15 f.ts#run",
        ]
    );
}
