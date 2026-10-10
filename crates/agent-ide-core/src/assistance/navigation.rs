//! Pure navigation rules shared by `ide.symbol` and `ide.graph` (symbol-graph-item r2 §2-§3).
//!
//! - Bare-name collapse: a declaration and the impl blocks that share its address are one
//!   definition, so a bare name whose matches collapse to one address resolves directly.
//! - Construction sites: a lexical, per-line rule that tags the usages of a struct, enum or
//!   class that build a value of it. It is a heuristic (macros, `use … as` aliases and patterns
//!   can be misclassified), which is why the tag carries `~`.
//!
//! Nothing here touches a session, a file or the worker; the callers in `symbols.rs` apply it.

use crate::lang::{
    Symbol, SymbolKind,
    render::{CONSTRUCTION_TAG, Usage},
};

/// Collapses the same-name candidates of one outline by rendered address.
///
/// `candidates` are the outline's symbols with the requested name, in outline order. Candidates
/// that share one address (a struct and its `impl` blocks) count once: the first non-`impl`
/// candidate of that address is kept, or the first `impl` when the address has no declaration.
/// The result keeps the first-occurrence order of each address; an empty input gives an empty
/// result.
pub(super) fn collapse_named(candidates: Vec<&Symbol>) -> Vec<&Symbol> {
    let mut kept: Vec<&Symbol> = Vec::new();
    for candidate in candidates {
        match kept.iter_mut().find(|kept| kept.path == candidate.path) {
            Some(kept) if kept.kind == SymbolKind::Impl && candidate.kind != SymbolKind::Impl => {
                *kept = candidate;
            }
            Some(_) => {}
            None => kept.push(candidate),
        }
    }
    kept
}

/// Tags every usage row of the type `name` whose line builds a value of it (see
/// [`is_construction_site`]) with [`CONSTRUCTION_TAG`], then moves the tagged rows before the
/// others, keeping relative order within both parts, so the card lists construction sites first
/// within its source and test groups. Rows that already carry a tag (index-backed rows) are
/// neither tagged nor moved ahead. `enum_type` enables the enum-variant forms.
pub(super) fn classify_constructions(usages: &mut [Usage], name: &str, enum_type: bool) {
    for usage in usages.iter_mut() {
        if usage.tag.is_none() && is_construction_site(&usage.text, name, enum_type) {
            usage.tag = Some(CONSTRUCTION_TAG.to_owned());
        }
    }
    usages.sort_by_key(|usage| usage.tag.as_deref() != Some(CONSTRUCTION_TAG));
}

/// Whether usages of a symbol of `kind` are classified for construction sites: structs, enums
/// and classes only.
pub(super) fn constructs(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Class
    )
}

/// Whether the source `line` builds a value of the type `name` (symbol-graph-item r2 §3).
///
/// `name` is the bare type name (non-empty); `enum_type` enables the enum-variant forms. The rule
/// looks at every whole-word occurrence of `name` in `line`. After the name, optional spaces and
/// one optional generic list (`<…>` or `::<…>`), an occurrence is a construction site when the
/// text continues with `{` (struct literal), `(` (tuple struct or class call), `::new(`,
/// `::default(` or `::from(`; for an enum also `::<Variant>(`, `::<Variant> {` or a bare
/// `::<Variant>` path; or when the occurrence is preceded by the word `new` (a constructor
/// expression). A line that declares or imports (its trimmed text, after visibility words,
/// starts with `use`, `import`, `from`, `impl`, `struct`, `enum`, `class`, `trait`, `type`,
/// `interface` or `union`) is never one, and neither is an occurrence directly after `->` (a
/// return type followed by a function body). The definition line itself is excluded by the
/// caller.
pub(super) fn is_construction_site(line: &str, name: &str, enum_type: bool) -> bool {
    if name.is_empty() || declares_or_imports(line) {
        return false;
    }
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(found) = line[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        from = end;
        let bounded_before = start == 0 || !is_identifier_byte(bytes[start - 1]);
        let bounded_after = end == bytes.len() || !is_identifier_byte(bytes[end]);
        if !bounded_before || !bounded_after {
            continue;
        }
        let before = line[..start].trim_end();
        if before.ends_with("->") {
            continue;
        }
        if before.ends_with("new")
            && before[..before.len() - 3]
                .chars()
                .last()
                .is_none_or(|previous| !previous.is_alphanumeric() && previous != '_')
        {
            return true;
        }
        let after = skip_generics(line[end..].trim_start());
        if after.starts_with('{') || after.starts_with('(') {
            return true;
        }
        if ["::new(", "::default(", "::from("]
            .iter()
            .any(|call| after.starts_with(call))
        {
            return true;
        }
        if enum_type && let Some(variant) = after.strip_prefix("::") {
            let length = variant
                .bytes()
                .take_while(|byte| is_identifier_byte(*byte))
                .count();
            if length > 0 && variant.as_bytes()[0].is_ascii_uppercase() {
                let rest = variant[length..].trim_start();
                if rest.starts_with('(') || rest.starts_with('{') || !rest.starts_with("::") {
                    return true;
                }
            }
        }
    }
    false
}

/// Skips one leading generic argument list, `<…>` or `::<…>` with balanced angle brackets, and
/// the spaces after it; returns `text` unchanged when it starts with neither or is unbalanced.
pub(super) fn skip_generics(text: &str) -> &str {
    let open = if text.starts_with("::<") {
        2
    } else if text.starts_with('<') {
        0
    } else {
        return text;
    };
    let mut depth = 0usize;
    for (index, byte) in text.bytes().enumerate().skip(open) {
        match byte {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return text[index + 1..].trim_start();
                }
            }
            _ => {}
        }
    }
    text
}

/// Whether a line declares or imports rather than uses: its trimmed text, after leading
/// visibility and modifier words, starts with a declaration or import keyword.
fn declares_or_imports(line: &str) -> bool {
    const MODIFIERS: [&str; 6] = [
        "pub ",
        "export ",
        "default ",
        "abstract ",
        "async ",
        "unsafe ",
    ];
    const KEYWORDS: [&str; 11] = [
        "use ",
        "import ",
        "from ",
        "impl",
        "struct ",
        "enum ",
        "class ",
        "trait ",
        "type ",
        "interface ",
        "union ",
    ];
    let mut rest = line.trim_start();
    loop {
        if let Some(stripped) = rest.strip_prefix("pub(") {
            match stripped.find(')') {
                Some(close) => rest = stripped[close + 1..].trim_start(),
                None => return false,
            }
        } else if let Some(modifier) = MODIFIERS.iter().find(|word| rest.starts_with(**word)) {
            rest = rest[modifier.len()..].trim_start();
        } else {
            break;
        }
    }
    KEYWORDS.iter().any(|keyword| rest.starts_with(keyword))
}

/// ASCII letters, digits and underscore: the bytes that extend an identifier.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{LineRange, SymbolPath};

    /// A symbol named `Outcome` at `src/model.rs#Outcome` of `kind`.
    fn symbol(kind: SymbolKind) -> Symbol {
        Symbol {
            path: SymbolPath::parse("src/model.rs#Outcome").unwrap(),
            kind,
            name: "Outcome".into(),
            range: LineRange::new(1, 3),
            body: LineRange::new(1, 3),
            signature: String::new(),
            doc: None,
            children: Vec::new(),
        }
    }

    /// A declaration and its impl blocks collapse to the declaration in either order; two
    /// distinct addresses stay two.
    #[test]
    fn collapse_keeps_one_declaration_per_address() {
        let declaration = symbol(SymbolKind::Struct);
        let implementation = symbol(SymbolKind::Impl);
        let collapsed = collapse_named(vec![&implementation, &declaration, &implementation]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].kind, SymbolKind::Struct);
        let mut other = symbol(SymbolKind::Struct);
        other.path = SymbolPath::parse("src/other.rs#Outcome").unwrap();
        assert_eq!(collapse_named(vec![&declaration, &other]).len(), 2);
        assert_eq!(
            collapse_named(vec![&implementation])[0].kind,
            SymbolKind::Impl
        );
        assert!(collapse_named(Vec::new()).is_empty());
    }

    /// The construction matrix of symbol-graph-item r2 §3: what builds a value, and what only
    /// names the type.
    #[test]
    fn construction_sites_are_classified_lexically() {
        let yes = [
            ("    let q = TranscriptQuery { limit: 3 };", false),
            ("let q = TranscriptQuery::<u8> {", false),
            ("let p = Point(1, 2);", false),
            ("let s = Store::new(path);", false),
            ("let s = Store::default();", false),
            ("let s: Store = Store::from(raw);", false),
            ("const store = new Store(path);", false),
            ("items.push(Wrapper(x))", false),
            ("let e = Event::Started(id);", true),
            ("let e = Event::Moved { to };", true),
            ("return Event::Stopped;", true),
            ("    vec![Point(0, 0)]", false),
        ];
        for (line, enum_type) in yes {
            let name = line
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .find(|word| {
                    ["TranscriptQuery", "Point", "Store", "Wrapper", "Event"].contains(word)
                })
                .unwrap();
            assert!(is_construction_site(line, name, enum_type), "{line}");
        }
        let no = [
            ("use crate::store::Store;", "Store", false),
            ("import { Store } from './store';", "Store", false),
            ("impl Display for Store {", "Store", false),
            ("pub(crate) struct Store {", "Store", false),
            ("export class Store {", "Store", false),
            ("fn open() -> Store {", "Store", false),
            ("fn take(store: Store) {", "Store", false),
            ("let all: Vec<Store> = Vec::new();", "Store", false),
            ("Store::open(path)", "Store", false),
            ("let s = Storage { };", "Store", false),
            ("let s = renew Store", "Store", false),
            ("match e { Event::Started(_) => {} }", "Event", false),
            ("Event::from_raw(x)", "Event", true),
        ];
        for (line, name, enum_type) in no {
            assert!(!is_construction_site(line, name, enum_type), "{line}");
        }
    }

    /// Construction rows are tagged and listed first; other rows keep their order and tags.
    #[test]
    fn classified_rows_come_first_in_stable_order() {
        let row = |line: u32, text: &str, tag: Option<&str>| Usage {
            file: "src/a.rs".into(),
            line,
            text: text.into(),
            is_test: false,
            tag: tag.map(str::to_owned),
        };
        let mut usages = vec![
            row(1, "fn take(store: Store) {", None),
            row(2, "let s = Store::new();", None),
            row(3, "let t = Store { a };", Some("[alpha]")),
            row(4, "Store(1)", None),
        ];
        classify_constructions(&mut usages, "Store", false);
        let order: Vec<u32> = usages.iter().map(|usage| usage.line).collect();
        assert_eq!(order, vec![2, 4, 1, 3]);
        assert_eq!(usages[0].tag.as_deref(), Some(CONSTRUCTION_TAG));
        assert_eq!(usages[3].tag.as_deref(), Some("[alpha]"));
    }

    /// Only structs, enums and classes are classified.
    #[test]
    fn only_value_types_are_classified() {
        assert!(constructs(SymbolKind::Struct));
        assert!(constructs(SymbolKind::Enum));
        assert!(constructs(SymbolKind::Class));
        assert!(!constructs(SymbolKind::Function));
        assert!(!constructs(SymbolKind::Trait));
    }
}
