//! Bounded source-only syntax outlines without importing or executing project code.
//!
//! CST nodes provide scopes, signatures and decorator spans directly. Parsing, traversal and
//! text construction share a deadline and aggregate budget; failure refuses the whole outline.
//! Interactive server-loading and unverified-resolution edit fallbacks remain disabled.

use crate::{LANGUAGE, support::Python};
use agent_ide_core::lang::{
    LanguageSupport, LineRange, Outline, Symbol, SymbolKind, SymbolPath, line_count,
};
use std::{
    ops::ControlFlow,
    path::Path,
    time::{Duration, Instant},
};
use tree_sitter::{Node, ParseOptions, Parser};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_SYMBOLS: usize = 20_000;
/// Limits aggregate owned text before allocating signatures, documentation and copied paths.
const MAX_TEXT: usize = 8 * 1024 * 1024;
const MAX_SIGNATURE: usize = 64 * 1024;
const TIME_LIMIT: Duration = Duration::from_secs(1);

/// Parses complete source; malformed, ambiguous one-line literals and exceeded budgets refuse.
pub(crate) fn outline(file: &Path, source: &str) -> Option<Outline> {
    let started = Instant::now();
    let tree = parse(source, started)?;
    outline_tree(file, source, &tree, started)
}

/// Shares a single bounded CST parse between independently requested operator consumers.
pub(crate) fn details(
    file: &Path,
    source: &str,
    want_imports: bool,
    want_outline: bool,
) -> (Option<Vec<LineRange>>, Option<Outline>) {
    if !want_imports && !want_outline {
        return (None, None);
    }
    let started = Instant::now();
    let Some(tree) = parse(source, started) else {
        return (None, None);
    };
    let imports = want_imports.then(|| imports_tree(&tree, started)).flatten();
    let outline = want_outline
        .then(|| outline_tree(file, source, &tree, started))
        .flatten();
    (imports, outline)
}

/// Converts an existing immutable CST under the parse's original deadline and text budgets.
fn outline_tree(
    file: &Path,
    source: &str,
    tree: &tree_sitter::Tree,
    started: Instant,
) -> Option<Outline> {
    let mut scan = Scan {
        source,
        started,
        remaining: MAX_SYMBOLS,
        text_left: MAX_TEXT,
        test: Python.is_test_file(file),
        file_bytes: file.as_os_str().len(),
    };
    let path = SymbolPath::new(Some(file.to_path_buf()), Vec::new());
    let symbols = scan.children(tree.root_node(), Owner::Module, &path, 0)?;
    let line_count = line_count(source);
    scan.check()?;
    Some(Outline {
        file: file.to_path_buf(),
        language: LANGUAGE,
        line_count,
        symbols,
    })
}

/// Parses within the same explicit source-size and wall-clock bounds for either consumer.
fn parse(source: &str, started: Instant) -> Option<tree_sitter::Tree> {
    if source.len() > MAX_BYTES {
        return None;
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() > TIME_LIMIT {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )?;
    if tree.root_node().has_error() {
        return None;
    }
    Some(tree)
}

/// Reads only syntactic module import nodes; literals and nested suites cannot forge entries.
pub(crate) fn imports(source: &str) -> Option<Vec<LineRange>> {
    let started = Instant::now();
    let tree = parse(source, started)?;
    imports_tree(&tree, started)
}

/// Collects physical import ranges without reparsing, preserving the original shared deadline.
fn imports_tree(tree: &tree_sitter::Tree, started: Instant) -> Option<Vec<LineRange>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut spans: Vec<LineRange> = Vec::new();
    let mut imports_seen = 0usize;
    for node in root.named_children(&mut cursor) {
        if started.elapsed() > TIME_LIMIT {
            return None;
        }
        if matches!(
            node.kind(),
            "import_statement" | "import_from_statement" | "future_import_statement"
        ) {
            if imports_seen >= MAX_SYMBOLS {
                return None;
            }
            imports_seen += 1;
            let span = LineRange::new(
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            );
            if let Some(last) = spans.last_mut()
                && span.start <= last.end
            {
                last.end = last.end.max(span.end);
            } else {
                spans.push(span);
            }
        }
    }
    Some(spans)
}

/// Lexical scope determines member/test kinds, independently of execution or import resolution.
#[derive(Clone, Copy, PartialEq)]
enum Owner {
    Module,
    Class,
    Function,
}

/// Borrows immutable source and accounts for every owned text expansion before allocation.
struct Scan<'a> {
    source: &'a str,
    started: Instant,
    remaining: usize,
    text_left: usize,
    test: bool,
    file_bytes: usize,
}

impl Scan<'_> {
    /// Includes text construction in the same per-file deadline as parsing and discovery.
    fn check(&self) -> Option<()> {
        (self.started.elapsed() <= TIME_LIMIT).then_some(())
    }

    /// Charges copied text before allocation; exhaustion never becomes a partial/empty outline.
    fn charge(&mut self, bytes: usize) -> Option<()> {
        self.check()?;
        self.text_left = self.text_left.checked_sub(bytes)?;
        Some(())
    }

    /// Collects declarations through suites, never through string contents or function locals.
    fn children(
        &mut self,
        parent: Node<'_>,
        owner: Owner,
        path: &SymbolPath,
        depth: usize,
    ) -> Option<Vec<Symbol>> {
        self.check()?;
        if depth > MAX_DEPTH {
            return None;
        }
        let mut out = Vec::new();
        let mut cursor = parent.walk();
        for node in parent.named_children(&mut cursor) {
            self.check()?;
            match node.kind() {
                "decorated_definition" => out.push(self.definition(
                    node.child_by_field_name("definition")?,
                    node,
                    owner,
                    path,
                    depth,
                )?),
                "function_definition" | "class_definition" => {
                    out.push(self.definition(node, node, owner, path, depth)?)
                }
                "expression_statement" if owner != Owner::Function => {
                    let mut cursor = node.walk();
                    for assignment in node.named_children(&mut cursor) {
                        if assignment.kind() == "assignment" {
                            self.assignment(assignment, path, &mut out, depth)?;
                        }
                    }
                }
                "type_alias_statement" if owner != Owner::Function => {
                    if let Some(name) = Self::alias_name(node.child_by_field_name("left")?) {
                        let signature = self.snippet(node, node.end_byte())?;
                        out.push(self.symbol(
                            node,
                            node,
                            name,
                            SymbolKind::TypeAlias,
                            signature,
                            None,
                            Vec::new(),
                            path,
                        )?);
                    }
                }
                "block" | "if_statement" | "elif_clause" | "else_clause" | "for_statement"
                | "while_statement" | "try_statement" | "except_clause" | "finally_clause"
                | "with_statement" | "match_statement" | "case_clause" => {
                    out.extend(self.children(node, owner, path, depth + 1)?)
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// Uses the exact wrapper for ranges and the definition itself for body/declaration lines.
    fn definition(
        &mut self,
        node: Node<'_>,
        wrapper: Node<'_>,
        owner: Owner,
        path: &SymbolPath,
        depth: usize,
    ) -> Option<Symbol> {
        let class = node.kind() == "class_definition";
        let name = node.child_by_field_name("name")?;
        let text = name.utf8_text(self.source.as_bytes()).ok()?;
        let mut kind = if class {
            SymbolKind::Class
        } else if owner == Owner::Class {
            if text == "__init__" {
                SymbolKind::Constructor
            } else {
                SymbolKind::Method
            }
        } else {
            SymbolKind::Function
        };
        if self.test
            && owner != Owner::Function
            && ((class && text.starts_with("Test"))
                || (!class && kind != SymbolKind::Constructor && text.starts_with("test_")))
        {
            kind = SymbolKind::Test;
        }
        let colon = (0..node.child_count())
            .filter_map(|i| node.child(i))
            .find(|c| c.kind() == ":")?;
        let signature = self.snippet(node, colon.start_byte())?;
        let body = node.child_by_field_name("body")?;
        let doc = self.doc(body)?;
        self.charge(
            path.segments().iter().map(String::len).sum::<usize>() + text.len() + self.file_bytes,
        )?;
        let child_path = path.child(text);
        let children = self.children(
            body,
            if class { Owner::Class } else { Owner::Function },
            &child_path,
            depth + 1,
        )?;
        self.symbol(node, wrapper, name, kind, signature, doc, children, path)
    }

    /// Destructured/chained bindings show their own target, never repeat a shared RHS/comment.
    fn assignment(
        &mut self,
        node: Node<'_>,
        path: &SymbolPath,
        out: &mut Vec<Symbol>,
        depth: usize,
    ) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.targets(node.child_by_field_name("left")?, node, path, out, depth)?;
        if let Some(right) = node.child_by_field_name("right")
            && right.kind() == "assignment"
        {
            self.assignment(right, path, out, depth + 1)?;
        }
        Some(())
    }

    /// Retains annotations and short single-line values; multiline initializers remain opaque.
    fn targets(
        &mut self,
        node: Node<'_>,
        statement: Node<'_>,
        path: &SymbolPath,
        out: &mut Vec<Symbol>,
        depth: usize,
    ) -> Option<()> {
        self.check()?;
        if depth > MAX_DEPTH {
            return None;
        }
        match node.kind() {
            "identifier" => {
                let name = node.utf8_text(self.source.as_bytes()).ok()?;
                let kind = if name.chars().any(char::is_uppercase)
                    && !name.chars().any(char::is_lowercase)
                {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                let direct = statement
                    .child_by_field_name("left")
                    .is_some_and(|left| left.id() == node.id());
                let end = statement.child_by_field_name("type").map(|t| t.end_byte());
                let signature = if direct && end.is_some() {
                    self.snippet(statement, end?)?
                } else if direct
                    && statement.start_position().row == statement.end_position().row
                    && statement.byte_range().len() <= 256
                    && !statement
                        .child_by_field_name("right")
                        .is_some_and(|r| r.kind() == "assignment")
                {
                    self.snippet(statement, statement.end_byte())?
                } else {
                    self.snippet(node, node.end_byte())?
                };
                out.push(self.symbol(
                    statement,
                    statement,
                    node,
                    kind,
                    signature,
                    None,
                    Vec::new(),
                    path,
                )?);
            }
            "pattern_list" | "tuple_pattern" | "list_pattern" | "list_splat_pattern" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.targets(child, statement, path, out, depth + 1)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    /// Locates an alias identifier through type wrappers without unbounded recursion.
    fn alias_name(mut node: Node<'_>) -> Option<Node<'_>> {
        for _ in 0..MAX_DEPTH {
            if node.kind() == "identifier" {
                return Some(node);
            }
            node = node.named_child(0)?;
        }
        None
    }

    /// Compacts only gaps between CST tokens; bytes inside string tokens are never rewritten.
    /// Multiline string tokens cannot be represented by this one-line contract and refuse.
    fn snippet(&mut self, node: Node<'_>, end: usize) -> Option<String> {
        let mut out = String::new();
        let mut stack = vec![node];
        let mut previous = node.start_byte();
        while let Some(token) = stack.pop() {
            self.check()?;
            if token.start_byte() >= end || token.kind() == "comment" {
                continue;
            }
            if token.child_count() != 0 && token.kind() != "string" {
                if usize::try_from(token.child_count()).ok()?
                    > MAX_SYMBOLS.saturating_sub(stack.len())
                {
                    return None;
                }
                for i in (0..token.child_count()).rev() {
                    stack.push(token.child(i)?);
                }
                continue;
            }
            let text = token.utf8_text(self.source.as_bytes()).ok()?;
            if text.contains(['\n', '\r']) {
                return None;
            }
            let gap = &self.source[previous..token.start_byte()];
            if !out.is_empty()
                && gap.chars().any(char::is_whitespace)
                && !out.ends_with(['(', '['])
                && !text.starts_with([')', ']'])
            {
                self.charge(1)?;
                out.push(' ');
            }
            if out.len().checked_add(text.len())? > MAX_SIGNATURE {
                return None;
            }
            self.charge(text.len())?;
            out.push_str(text);
            previous = token.end_byte();
        }
        Some(out)
    }

    /// Takes the first paragraph of a constant leading string expression; comments are skipped.
    fn doc(&mut self, body: Node<'_>) -> Option<Option<String>> {
        let mut cursor = body.walk();
        let Some(first) = body
            .named_children(&mut cursor)
            .find(|n| n.kind() != "comment")
        else {
            return Some(None);
        };
        if first.kind() != "expression_statement" {
            return Some(None);
        }
        let Some(mut string) = first.named_child(0) else {
            return Some(None);
        };
        let mut depth = 0;
        while string.kind() == "parenthesized_expression" {
            self.check()?;
            if depth >= MAX_DEPTH {
                return None;
            }
            let mut cursor = string.walk();
            let mut children = string
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment");
            let Some(inner) = children.next() else {
                return Some(None);
            };
            if children.next().is_some() {
                return Some(None);
            }
            drop(children);
            string = inner;
            depth += 1;
        }
        let mut parts = Vec::new();
        match string.kind() {
            "string" => parts.push(string),
            "concatenated_string" => {
                let mut cursor = string.walk();
                for child in string.named_children(&mut cursor) {
                    if child.kind() == "comment" {
                        continue;
                    }
                    self.check()?;
                    if child.kind() != "string" {
                        return Some(None);
                    }
                    if parts.len() >= MAX_SYMBOLS {
                        return None;
                    }
                    parts.push(child);
                }
            }
            _ => return Some(None),
        }
        let mut content = String::new();
        for part in parts {
            let start = part.child(0)?;
            let opened = start.utf8_text(self.source.as_bytes()).ok()?;
            if opened
                .chars()
                .any(|c| matches!(c, 'f' | 'F' | 'b' | 'B' | 't' | 'T'))
            {
                return Some(None);
            }
            let delimiter =
                if opened.ends_with("\"\"\"") || opened.as_bytes().ends_with(&[39, 39, 39]) {
                    3
                } else {
                    1
                };
            let piece = self
                .source
                .get(start.end_byte()..part.end_byte().checked_sub(delimiter)?)?;
            self.charge(piece.len())?;
            content.push_str(piece);
        }
        let mut out = String::new();
        for line in content
            .lines()
            .map(str::trim)
            .skip_while(|l| l.is_empty())
            .take_while(|l| !l.is_empty())
        {
            self.charge(line.len() + usize::from(!out.is_empty()))?;
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(line);
        }
        Some((!out.is_empty()).then_some(out))
    }

    /// Charges repeated names/path segments before cloning them into an owned symbol.
    #[expect(
        clippy::too_many_arguments,
        reason = "Explicit CST facts avoid a second unbounded normalizer"
    )]
    fn symbol(
        &mut self,
        node: Node<'_>,
        wrapper: Node<'_>,
        name: Node<'_>,
        kind: SymbolKind,
        signature: String,
        doc: Option<String>,
        children: Vec<Symbol>,
        parent: &SymbolPath,
    ) -> Option<Symbol> {
        self.remaining = self.remaining.checked_sub(1)?;
        let name = name.utf8_text(self.source.as_bytes()).ok()?;
        self.charge(
            name.len() * 2
                + self.file_bytes
                + parent.segments().iter().map(String::len).sum::<usize>(),
        )?;
        Some(Symbol {
            path: parent.child(name),
            kind,
            name: name.into(),
            range: LineRange::new(
                wrapper.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            ),
            body: LineRange::new(
                node.start_position().row as u32 + 1,
                node.end_position().row as u32 + 1,
            ),
            signature,
            doc,
            children,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ide_core::lang::{LineRange, SymbolKind, SymbolPath, address::AddressMap};

    /// Classes, nested functions, decorated async methods, attributes and docstrings retain scope.
    #[test]
    fn source_outline_preserves_python_structure() {
        let source = r#""""Module documentation.
class NotAClass:
    def not_a_function(): pass
"""
from typing import Generic, TypeVar
LIMIT = 3
class Worker[T](Generic[T]):
    """Processes jobs."""
    label: str = "worker"
    def __init__(self, name: str):
        self.name = name
        local = 1
    @decorator(
        flag=True,
    )
    async def run(
        self, path: str = "http://example.test:a",
    ) -> str:
        """Runs one job.

        Extra paragraph.
        """
        def inner() -> str:
            """Nested helper."""
            return path
        return inner()
"#;
        let file = Path::new("src/worker.py");
        let outline = outline(file, source).unwrap();
        assert_eq!(
            outline
                .symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["LIMIT", "Worker"]
        );
        let worker = &outline.symbols[1];
        assert_eq!(worker.kind, SymbolKind::Class);
        assert_eq!(worker.signature, "class Worker[T](Generic[T])");
        assert_eq!(worker.doc.as_deref(), Some("Processes jobs."));
        assert_eq!(
            worker
                .children
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["label", "__init__", "run"]
        );
        assert_eq!(worker.children[0].signature, "label: str");
        assert_eq!(worker.children[1].kind, SymbolKind::Constructor);
        assert!(worker.children[1].children.is_empty());
        let run = &worker.children[2];
        assert_eq!(run.kind, SymbolKind::Method);
        assert_eq!(run.range, LineRange::new(13, 26));
        assert_eq!(run.doc.as_deref(), Some("Runs one job."));
        assert_eq!(
            run.signature,
            "async def run(self, path: str = \"http://example.test:a\",) -> str"
        );
        assert_eq!(
            run.children[0].path.to_string(),
            "src/worker.py#Worker/run/inner"
        );
        assert_eq!(run.children[0].doc.as_deref(), Some("Nested helper."));
        let map = AddressMap::build_parts(file, &outline.symbols, source, Python.region_rules());
        assert_eq!(map.violations(source), Vec::<String>::new());
        assert!(!Python.outline_while_loading());
        assert!(!Python.outline_when_resolution_unverified());
    }

    /// Statements under conditionals retain their enclosing scope; function bindings are omitted.
    #[test]
    fn source_outline_handles_bindings_and_control_flow() {
        let source = "a, b = (1, 2)\nx = y = 3\ntype Result[T] = tuple[T, str]\nif True:\n    def conditional():\n        def nested(): pass\n        local = 4\n        return nested\nclass Box:\n    if True:\n        VALUE: int = 3\n    class Inner: pass\n";
        let outline = outline(Path::new("bindings.pyi"), source).unwrap();
        assert_eq!(
            outline
                .symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "x", "y", "Result", "conditional", "Box"]
        );
        assert_eq!(outline.symbols[4].kind, SymbolKind::TypeAlias);
        let path = SymbolPath::parse("bindings.pyi#conditional/nested").unwrap();
        assert!(outline.find(&path).is_some());
        assert!(
            outline.symbols[5]
                .children
                .iter()
                .all(|s| s.name != "local")
        );
        assert_eq!(outline.symbols[6].children[0].name, "VALUE");
        assert_eq!(outline.symbols[6].children[1].kind, SymbolKind::Class);
    }

    /// Pytest symbols use the same collapse policy as Pyright, while helpers remain visible.
    #[test]
    fn source_outline_preserves_test_classification() {
        let source = "def test_one(): pass\nclass TestWorker:\n    def test_two(self): pass\n    def helper(self): pass\n";
        let outline = outline(Path::new("tests/test_worker.py"), source).unwrap();
        assert_eq!(outline.symbols[0].kind, SymbolKind::Test);
        assert_eq!(outline.symbols[1].kind, SymbolKind::Test);
        assert_eq!(outline.symbols[1].children[0].kind, SymbolKind::Test);
        assert_eq!(outline.symbols[1].children[1].kind, SymbolKind::Method);
    }

    /// Broken syntax and resource refusals differ from a valid empty source file.
    #[test]
    fn source_outline_refuses_invalid_and_excessive_input() {
        let file = Path::new("bad.py");
        for source in [
            "def broken(:\n pass\n",
            "\"\"\"unterminated",
            "class A\n pass\n",
        ] {
            assert!(outline(file, source).is_none(), "{source}");
        }
        assert!(
            outline(file, "# Empty module\n")
                .unwrap()
                .symbols
                .is_empty()
        );
        assert!(outline(file, &" ".repeat(MAX_BYTES + 1)).is_none());
        let excessive = (0..MAX_SYMBOLS + 1)
            .map(|i| format!("v{i} = 0\n"))
            .collect::<String>();
        assert!(outline(file, &excessive).is_none());
    }
    /// R1: each tuple target has its own short signature; shared comments/RHS are never copied.
    #[test]
    fn tuple_bindings_do_not_amplify_shared_source() {
        let names = (0..128)
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("{names} = (0,) * 128 # {}\n", "x".repeat(1024 * 1024));
        let out = outline(Path::new("tuple.py"), &source).unwrap();
        assert_eq!(out.symbols.len(), 128);
        assert!(out.symbols.iter().all(|s| s.signature == s.name));
        assert!(out.symbols.iter().map(|s| s.signature.len()).sum::<usize>() < 1024);
        let tree = parse("def f(): pass\n", Instant::now()).unwrap();
        let path = SymbolPath::new(Some("f.py".into()), Vec::new());
        let mut scan = Scan {
            source: "def f(): pass\n",
            started: Instant::now() - TIME_LIMIT - Duration::from_secs(1),
            remaining: MAX_SYMBOLS,
            text_left: MAX_TEXT,
            test: false,
            file_bytes: 4,
        };
        assert!(
            scan.children(tree.root_node(), Owner::Module, &path, 0)
                .is_none()
        );
        scan.started = Instant::now();
        scan.text_left = 3;
        assert!(
            scan.children(tree.root_node(), Owner::Module, &path, 0)
                .is_none()
        );
    }

    /// R5: literal bytes survive token-gap compaction, including spacing and punctuation.
    #[test]
    fn signatures_preserve_literals_and_long_headers() {
        let out = outline(
            Path::new("literals.py"),
            "def f(x=\"a  b\", y=\"( x, )\"): pass\n",
        )
        .unwrap();
        assert_eq!(out.symbols[0].signature, "def f(x=\"a  b\", y=\"( x, )\")");
        let args = (0..90)
            .map(|i| format!("    p{i}: int = {i},\n"))
            .collect::<String>();
        let out = outline(
            Path::new("long.py"),
            &format!("def f(\n{args}) -> int:\n    return p89\n"),
        )
        .unwrap();
        assert!(out.symbols[0].signature.contains("p89: int = 89"));
        assert!(out.symbols[0].signature.ends_with(") -> int"));
        let tuple = outline(
            Path::new("tuple_default.py"),
            "def f(x=(\"one\",), y=[1,]): pass\n",
        )
        .unwrap();
        assert_eq!(tuple.symbols[0].signature, "def f(x=(\"one\",), y=[1,])");
        let out = outline(Path::new("concat.py"), "def f(error=\"Connection \"\n    \"reset by peer\"):\n    'Doc ' \"body.\"\n    pass\n").unwrap();
        assert_eq!(
            out.symbols[0].signature,
            "def f(error=\"Connection \" \"reset by peer\")"
        );
        assert_eq!(out.symbols[0].doc.as_deref(), Some("Doc body."));
        assert!(
            outline(
                Path::new("multiline.py"),
                "def f(x=\"\"\"a\nb\"\"\"): pass\n"
            )
            .is_none()
        );
    }

    /// R6: exact decorated wrapper survives intervening comments/blank lines and CRLF.
    #[test]
    fn decorated_ranges_are_taken_from_cst() {
        let source = "@decorate(\r\n    1\r\n)\r\n# explanation\r\n\r\ndef f():\r\n    'Short doc.'\r\n    return 1\r\n";
        let out = outline(Path::new("decorated.py"), source).unwrap();
        assert_eq!(out.symbols[0].range.start, 1);
        assert_eq!(out.symbols[0].body.start, 6);
        assert_eq!(out.symbols[0].doc.as_deref(), Some("Short doc."));
        let map = AddressMap::build_parts(
            Path::new("decorated.py"),
            &out.symbols,
            source,
            Python.region_rules(),
        );
        assert!(map.violations(source).is_empty());
    }
    /// Parentheses preserve constant docstrings but cannot turn tuples/expressions into docs.
    #[test]
    fn parenthesized_docstrings_and_budget_refusals_are_explicit() {
        for body in [
            "    ('Useful '\n     'documentation.')\n",
            "    ((\n        # explanation\n        'Useful documentation.'\n    ))\n",
        ] {
            let out = outline(Path::new("doc.py"), &format!("def f():\n{body}    pass\n")).unwrap();
            assert_eq!(out.symbols[0].doc.as_deref(), Some("Useful documentation."));
        }
        for body in [
            "    ('not a doc',)\n",
            "    ('not' + ' a doc')\n",
            "    (123)\n",
        ] {
            let out = outline(Path::new("doc.py"), &format!("def f():\n{body}    pass\n")).unwrap();
            assert!(out.symbols[0].doc.is_none());
        }
        let parts = "'x' ".repeat(MAX_SYMBOLS);
        let out = outline(
            Path::new("doc.py"),
            &format!("def f():\n    {parts}\n    pass\n"),
        )
        .unwrap();
        assert_eq!(out.symbols[0].doc.as_ref().unwrap().len(), MAX_SYMBOLS);
        assert!(
            outline(
                Path::new("doc.py"),
                &format!("def f():\n    {parts}'x'\n    pass\n")
            )
            .is_none()
        );
    }

    /// Same-line imports form one physical range before source joins; adjacent lines stay separate.
    #[test]
    fn import_ranges_merge_shared_physical_lines_before_materialization() {
        let line = (0..128)
            .map(|i| format!("import a{i}"))
            .collect::<Vec<_>>()
            .join("; ");
        let source = format!("{line} # {}\nimport next\n", "x".repeat(16384));
        assert_eq!(
            imports(&source),
            Some(vec![LineRange::new(1, 1), LineRange::new(2, 2)])
        );
        assert_eq!(
            imports("from a import (\n    b); import c\n"),
            Some(vec![LineRange::new(1, 2)])
        );
    }
}
