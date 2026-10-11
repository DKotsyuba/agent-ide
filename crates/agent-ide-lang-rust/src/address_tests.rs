//! Rust conformance for `symbol-address-v1`: region fixture and the repository sweep.

use std::path::Path;

use agent_ide_core::lang::address::{AddressMap, conformance_sweep};

use crate::LANGUAGE;

/// `first-last address` rows of every unit of `source`, in file order.
fn rows(source: &str) -> Vec<String> {
    let outline = LANGUAGE
        .support()
        .outline_from_source(Path::new("f.rs"), source)
        .expect("rust has a source outline");
    let map = AddressMap::build(&outline, source);
    assert_eq!(map.violations(source), Vec::<String>::new());
    map.units()
        .iter()
        .map(|u| format!("{}-{} {}", u.span.start, u.span.end, u.address))
        .collect()
}

/// Every line of every `.rs` file of this repository has exactly one address.
#[test]
fn address_totality_rust_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (checked, failures) = conformance_sweep(&root, LANGUAGE);
    assert!(checked > 50, "only {checked} files swept");
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(10)].join("\n")
    );
}

/// Header, imports, constants, macro blocks and trailing trivia each get their address.
#[test]
fn address_regions_rust() {
    let source = "//! Crate docs.\n#![allow(dead_code)]\n\nuse std::fmt;\n// between\n#[cfg(test)]\nuse std::io::{\n    self,\n};\n\nconst LIMIT: u32 = 4;\n\nthread_local! {\n    static X: u8 = 1;\n}\n\n/// Run.\nfn run() {}\n// tail\n";
    assert_eq!(
        rows(source),
        [
            "1-3 f.rs#@header",
            "4-9 f.rs#@imports",
            "10-10 f.rs#@after/@imports",
            "11-11 f.rs#LIMIT",
            "12-12 f.rs#@after/LIMIT",
            "13-15 f.rs#@code/thread_local!",
            "16-16 f.rs#@after/@code%2Fthread_local!",
            "17-18 f.rs#run",
            "19-19 f.rs#@after/run",
        ]
    );
}

/// Every map operation the worker uses answers on a tiny two-function file without panicking.
#[test]
fn address_operations_on_a_tiny_file() {
    let source =
        "/// Answers cold.\npub fn value() -> i32 { 7 }\n\npub fn caller() -> i32 { value() }\n";
    let outline = LANGUAGE
        .support()
        .outline_from_source(Path::new("src/lib.rs"), source)
        .unwrap();
    let map = AddressMap::build(&outline, source);
    for symbol in &outline.symbols {
        assert!(map.address_of(symbol).is_some(), "{symbol:?}");
        assert!(map.symbol_of(map.named(&symbol.name)[0]).is_some());
    }
    for line in 1..=4 {
        assert!(map.address_at(Path::new("src/lib.rs"), line).is_some());
    }
    let missing = match agent_ide_core::lang::address::Address::parse("src/lib.rs#missing") {
        Ok(agent_ide_core::lang::address::Parsed::Address(address)) => address,
        other => panic!("{other:?}"),
    };
    assert!(map.resolve(&missing).is_err());
    assert!(map.near(&missing).is_empty());
}
