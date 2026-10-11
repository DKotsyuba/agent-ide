//! Python conformance for `symbol-address-v1`: region fixture over a server-shaped outline.
//!
//! Python has no source outline (symbols come from Pyright), so the fixture builds the symbols
//! Pyright would report and runs them through the language's own region rules.

use std::path::Path;

use agent_ide_core::lang::{
    LanguageSupport, LineRange, Symbol, SymbolKind, SymbolPath, address::AddressMap,
};

use crate::support::Python;

/// A childless top-level symbol on `start..=end`.
fn symbol(name: &str, kind: SymbolKind, start: u32, end: u32) -> Symbol {
    let range = LineRange::new(start, end);
    Symbol {
        path: SymbolPath::new(Some("f.py".into()), vec![name.to_owned()]),
        kind,
        name: name.to_owned(),
        range,
        body: range,
        signature: name.to_owned(),
        doc: None,
        children: Vec::new(),
    }
}

/// `first-last address` rows of every unit of `source` with the given top-level symbols.
fn rows(source: &str, symbols: &[Symbol]) -> Vec<String> {
    let map = AddressMap::build_parts(Path::new("f.py"), symbols, source, Python.region_rules());
    assert_eq!(map.violations(source), Vec::<String>::new());
    map.units()
        .iter()
        .map(|u| format!("{}-{} {}", u.span.start, u.span.end, u.address))
        .collect()
}

/// Docstring header, import runs, module variables, statement blocks and `@main`.
#[test]
fn address_regions_python() {
    let source = "#!/usr/bin/env python\n\"\"\"Module doc.\"\"\"\nfrom __future__ import annotations\nimport os\nfrom a import (\n    b,\n)\n\nLIMIT = 4\n\nsys.path.insert(0, ROOT)\n\nif TYPE_CHECKING:\n    import x\n\ndef run():\n    return 1\n\nif __name__ == \"__main__\":\n    run()\n";
    let symbols = [
        symbol("LIMIT", SymbolKind::Variable, 9, 9),
        symbol("run", SymbolKind::Function, 16, 17),
    ];
    assert_eq!(
        rows(source, &symbols),
        [
            "1-2 f.py#@header",
            "3-7 f.py#@imports",
            "8-8 f.py#@after/@imports",
            "9-9 f.py#LIMIT",
            "10-10 f.py#@after/LIMIT",
            "11-11 f.py#@code/sys.path.insert",
            "12-12 f.py#@after/@code%2Fsys.path.insert",
            "13-14 f.py#@code/if TYPE_CHECKING",
            "15-15 f.py#@after/@code%2Fif TYPE_CHECKING",
            "16-17 f.py#run",
            "18-18 f.py#@after/run",
            "19-20 f.py#@main",
        ]
    );
}
