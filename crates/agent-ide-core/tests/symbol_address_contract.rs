//! Boundary test of `symbol-address-v1` (docs/contracts/symbol-address-v1.md): the public text
//! grammar and the resolver map seen from a consumer (M-029, M-030). It builds maps from
//! hand-made outlines, so it needs no language server.

use std::path::Path;

use agent_ide_core::lang::{
    LineRange, Symbol, SymbolKind, SymbolPath,
    address::{Address, AddressMap, Parsed, ResolveError, Window},
    regions::GENERIC_RULES,
};

/// A childless or parent symbol of `file` named by `names` on lines `a..=b`.
fn symbol(names: &[&str], kind: SymbolKind, a: u32, b: u32, children: Vec<Symbol>) -> Symbol {
    let range = LineRange::new(a, b);
    Symbol {
        path: SymbolPath::new(
            Some("f.rs".into()),
            names.iter().map(|n| (*n).to_owned()).collect(),
        ),
        kind,
        name: (*names.last().unwrap()).to_owned(),
        range,
        body: range,
        signature: format!("sig {}", names.last().unwrap()),
        doc: None,
        children,
    }
}

/// `struct Foo;` plus an impl holding two methods named `new`, after a header comment, with
/// `pad` extra blank-comment lines inserted above the impl's first method.
fn file(pad: u32) -> (String, AddressMap) {
    let mut source = String::from("// header\n\nstruct Foo;\n\nimpl Foo {\n");
    source.push_str("    fn new() {}\n");
    for _ in 0..pad {
        source.push_str("    // pad\n");
    }
    source.push_str("    fn new() {}\n}\n");
    let first = 6;
    let second = 7 + pad;
    let new = |n| symbol(&["Foo", "new"], SymbolKind::Method, n, n, Vec::new());
    let symbols = vec![
        symbol(&["Foo"], SymbolKind::Struct, 3, 3, Vec::new()),
        symbol(
            &["Foo"],
            SymbolKind::Impl,
            5,
            second + 1,
            vec![new(first), new(second)],
        ),
    ];
    let map = AddressMap::build_parts(Path::new("f.rs"), &symbols, &source, &GENERIC_RULES);
    (source, map)
}

fn address(text: &str) -> Address {
    match Address::parse(text).expect("valid address") {
        Parsed::Address(address) => address,
        other => panic!("not an address: {other:?}"),
    }
}

/// Text that parses prints back identically, for names with every special character and for
/// every window form.
#[test]
fn symbol_address_contract_grammar_round_trips() {
    for text in [
        "src/a.rs#Store/get:7",
        "src/a.rs#Store/get:3-5",
        "src/a.rs#Store/get:3-",
        "src/a.rs#@imports~2",
        "src/a.rs#impl Display for Foo/fmt",
        "a.rs#x%2Fy/z",
        "a.rs#a%7E2",
        "a.rs#a%3A3:5",
        "Store/get",
    ] {
        assert_eq!(address(text).to_string(), text);
    }
    assert!(matches!(
        Address::parse("src/a.rs:120:15"),
        Ok(Parsed::Location { line: 120, .. })
    ));
    assert_eq!(
        address("a.rs#f:2-").window(),
        Some(Window {
            first: 2,
            last: None
        })
    );
}

/// No two units share a canonical address, each resolves to itself, and duplicates carry `~k`.
#[test]
fn symbol_address_contract_duplicates_are_unique_and_resolvable() {
    let (_, map) = file(0);
    let names: Vec<String> = map.units().iter().map(|u| u.address.to_string()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "duplicate addresses: {names:?}");
    assert!(names.contains(&"f.rs#Foo~2".to_owned()), "{names:?}");
    assert!(names.contains(&"f.rs#Foo/new~2".to_owned()), "{names:?}");
    for (i, unit) in map.units().iter().enumerate() {
        assert_eq!(map.resolve(&unit.address).unwrap().unit, i);
    }
}

/// Every line has exactly one address that resolves back to a window containing the line.
#[test]
fn symbol_address_contract_every_line_has_one_address() {
    let (source, map) = file(2);
    assert_eq!(map.violations(&source), Vec::<String>::new());
    let lines = source.lines().count() as u32;
    assert!(map.at_line(lines + 1).is_none());
    assert!(map.at_line(0).is_none());
}

/// `N` counts from the unit's first line, so it moves only when its own unit changes: padding
/// above a symbol leaves its address and `:N` alone, padding inside shifts only what is below.
#[test]
fn symbol_address_contract_n_is_relative_to_the_unit() {
    let (_, plain) = file(0);
    let (_, padded) = file(3);
    let at = |map: &AddressMap, line| {
        map.address_at(Path::new("f.rs"), line)
            .expect("address for the line")
    };
    // `struct Foo;` is line 3 in both files and does not move.
    assert_eq!(at(&plain, 3), "f.rs#Foo:1");
    assert_eq!(at(&padded, 3), "f.rs#Foo:1");
    // The impl's first line keeps `:1`; its first method keeps its address in both files.
    assert_eq!(at(&plain, 5), "f.rs#Foo~2:1");
    assert_eq!(at(&padded, 5), "f.rs#Foo~2:1");
    assert_eq!(at(&plain, 6), "f.rs#Foo/new:1");
    assert_eq!(at(&padded, 6), "f.rs#Foo/new:1");
    // The padded lines belong to the impl, counted from its first line: line 7 is its third.
    assert_eq!(at(&padded, 7), "f.rs#Foo~2:3");
    // The second method keeps `:1` wherever it sits.
    assert_eq!(at(&plain, 7), "f.rs#Foo/new~2:1");
    assert_eq!(at(&padded, 10), "f.rs#Foo/new~2:1");
    // A window beyond the unit is refused, and an end beyond it is clamped.
    assert_eq!(
        plain.resolve(&address("f.rs#Foo:2")).unwrap_err(),
        ResolveError::LineOutOfRange { length: 1 }
    );
    assert!(plain.resolve(&address("f.rs#Foo~2:2-99")).unwrap().clamped);
}
