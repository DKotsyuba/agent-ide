//! Compact model-facing renderings of outlines, symbol bodies and symbol cards.
//!
//! Everything here is plain text with 1-based lines, ceilings on list lengths and no internal
//! bookkeeping fields: the agent reads what a human would read in an editor's outline, "go to
//! definition" preview and "find usages" panel.

use std::path::Path;

use async_lsp::lsp_types as lsp;

use super::{LineRange, Outline, Symbol, SymbolKind, slice_lines};

/// Usages printed inline before the list collapses into "… N more".
pub const MAX_USAGE_LINES: usize = 30;
/// Callers or callees printed inline per level.
pub const MAX_CALL_LINES: usize = 20;
/// Doc paragraph ceiling in characters.
pub const MAX_DOC_CHARS: usize = 600;
/// Maximum unique nodes retained by one call graph.
pub const MAX_GRAPH_NODES: usize = 60;
/// Maximum unique edges retained by one call graph.
pub const MAX_GRAPH_EDGES: usize = 120;

/// Direction in which one call-graph edge is rendered from its parent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum GraphDirection {
    /// The edge points to a function that calls its parent.
    Callers,
    /// The edge points to a function called by its parent.
    Callees,
}

impl GraphDirection {
    /// Returns the displayed arrow for this call relationship.
    pub const fn arrow(self) -> &'static str {
        match self {
            Self::Callers => "←",
            Self::Callees => "→",
        }
    }
}

/// One unique symbol in a bounded live call graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphNode {
    /// Full relative symbol path, or its bare name when outline resolution failed.
    pub path: String,
    /// Relative source file used in the location suffix.
    pub file: String,
    /// One-based source line.
    pub line: u32,
    /// Whether the outline classifies this symbol as a test.
    pub is_test: bool,
}

/// One directed call relationship between node indexes in a [`CallGraph`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphEdge {
    /// Index of the symbol whose callers or callees were queried.
    pub from: usize,
    /// Index of the related caller or callee.
    pub to: usize,
    /// Whether the related symbol calls or is called by `from`.
    pub direction: GraphDirection,
}

/// Bounded, deduplicated graph collected from live call-hierarchy requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallGraph {
    /// Unique nodes, with the queried symbol at index zero.
    pub nodes: Vec<GraphNode>,
    /// Unique parent-to-related-symbol relationships in discovery order.
    pub edges: Vec<GraphEdge>,
    /// Tests dropped per parent node and direction when the graph hides them, rendered as `+N tests`.
    pub collapsed_tests: std::collections::BTreeMap<(usize, GraphDirection), usize>,
    /// Set when adding a node or edge reached the fixed graph ceiling.
    pub capped: bool,
}

impl CallGraph {
    /// Starts a graph with its root symbol at index zero.
    pub fn new(root: GraphNode) -> Self {
        Self {
            nodes: vec![root],
            edges: Vec::new(),
            collapsed_tests: Default::default(),
            capped: false,
        }
    }

    /// Adds a unique node, returning its index; returns `None` after the 60-node ceiling.
    pub fn add_node(&mut self, node: GraphNode) -> Option<usize> {
        if self.nodes.len() == MAX_GRAPH_NODES {
            self.capped = true;
            return None;
        }
        let index = self.nodes.len();
        self.nodes.push(node);
        Some(index)
    }

    /// Adds one deduplicated edge; returns false when it already exists or the 120-edge ceiling is hit.
    pub fn add_edge(&mut self, edge: GraphEdge) -> bool {
        if self.edges.contains(&edge) {
            return true;
        }
        if self.edges.len() == MAX_GRAPH_EDGES {
            self.capped = true;
            return false;
        }
        self.edges.push(edge);
        true
    }
}

/// Renders a depth-limited graph as an indented tree; repeated nodes are marked `(seen)`.
pub fn call_graph_text(graph: &CallGraph, direction: &str, depth: u8) -> String {
    let root = &graph.nodes[0];
    let subject = match direction {
        "both" => "callers and callees",
        "callees" => "callees",
        _ => "callers",
    };
    let mut out = format!(
        "graph: {subject} of {} (depth {depth}, {} nodes, {} edges)\n",
        root.path,
        graph.nodes.len(),
        graph.edges.len()
    );
    let mut seen = vec![false; graph.nodes.len()];
    seen[0] = true;
    let directions = if direction == "both" {
        [Some(GraphDirection::Callers), Some(GraphDirection::Callees)]
    } else if direction == "callees" {
        [Some(GraphDirection::Callees), None]
    } else {
        [Some(GraphDirection::Callers), None]
    };
    for selected in directions.into_iter().flatten() {
        render_graph_children(graph, 0, selected, 1, depth, &mut seen, &mut out);
    }
    if graph.capped {
        out.push_str("… capped at 60 nodes or 120 edges\n");
    }
    out
}

/// Renders the first-visit tree for one side of the graph without following cycles twice.
fn render_graph_children(
    graph: &CallGraph,
    parent: usize,
    direction: GraphDirection,
    level: u8,
    depth: u8,
    seen: &mut [bool],
    out: &mut String,
) {
    if level > depth {
        return;
    }
    for edge in graph
        .edges
        .iter()
        .filter(|edge| edge.from == parent && edge.direction == direction)
    {
        let node = &graph.nodes[edge.to];
        let already_seen = seen[edge.to];
        out.push_str(&format!(
            "{}{} {}  {}:{}",
            "  ".repeat(level as usize),
            direction.arrow(),
            node.path,
            node.file,
            node.line
        ));
        if !already_seen {
            seen[edge.to] = true;
        }
        if node.is_test {
            out.push_str(" [test]");
        }
        if already_seen {
            out.push_str(" (seen)");
        }
        out.push('\n');
        if !already_seen {
            render_graph_children(graph, edge.to, direction, level + 1, depth, seen, out);
        }
    }
    if let Some(count) = graph
        .collapsed_tests
        .get(&(parent, direction))
        .filter(|count| **count > 0)
    {
        out.push_str(&format!("{}+{count} tests\n", "  ".repeat(level as usize)));
    }
}

#[cfg(test)]
mod graph_tests {
    use super::*;

    /// Builds a small cyclic graph and verifies first-visit rendering plus the node ceiling.
    #[test]
    fn call_graph_renders_cycles_and_stops_at_node_cap() {
        let mut graph = CallGraph::new(GraphNode {
            path: "src/lib.rs#root".to_owned(),
            file: "src/lib.rs".to_owned(),
            line: 1,
            is_test: false,
        });
        for index in 1..3 {
            let node = graph
                .add_node(GraphNode {
                    path: format!("src/lib.rs#f{index}"),
                    file: "src/lib.rs".to_owned(),
                    line: index + 1,
                    is_test: index == 2,
                })
                .unwrap();
            graph.add_edge(GraphEdge {
                from: node - 1,
                to: node,
                direction: GraphDirection::Callers,
            });
        }
        graph.add_edge(GraphEdge {
            from: 2,
            to: 0,
            direction: GraphDirection::Callers,
        });
        let text = call_graph_text(&graph, "callers", 3);
        assert!(text.contains("← src/lib.rs#root  src/lib.rs:1 (seen)"));
        assert!(text.contains("[test]"));

        for index in graph.nodes.len()..MAX_GRAPH_NODES {
            assert!(
                graph
                    .add_node(GraphNode {
                        path: format!("src/lib.rs#f{index}"),
                        file: "src/lib.rs".to_owned(),
                        line: index as u32 + 1,
                        is_test: false,
                    })
                    .is_some()
            );
        }
        assert!(
            graph
                .add_node(GraphNode {
                    path: "src/lib.rs#overflow".to_owned(),
                    file: "src/lib.rs".to_owned(),
                    line: 99,
                    is_test: false,
                })
                .is_none()
        );
        assert!(graph.capped);
        assert!(call_graph_text(&graph, "callers", 1).contains("… capped at 60 nodes"));
    }

    /// Keeps callers before callees for a two-sided graph and marks a repeated test node.
    #[test]
    fn call_graph_renders_both_sides_in_order_and_marks_seen_tests() {
        let mut graph = CallGraph::new(GraphNode {
            path: "root".into(),
            file: "src/lib.rs".into(),
            line: 1,
            is_test: false,
        });
        for (path, is_test, direction) in [
            ("caller", false, GraphDirection::Callers),
            ("test", true, GraphDirection::Callees),
        ] {
            let index = graph
                .add_node(GraphNode {
                    path: path.into(),
                    file: "src/lib.rs".into(),
                    line: 2,
                    is_test,
                })
                .unwrap();
            graph.add_edge(GraphEdge {
                from: 0,
                to: index,
                direction,
            });
        }
        graph.add_edge(GraphEdge {
            from: 1,
            to: 2,
            direction: GraphDirection::Callers,
        });
        let text = call_graph_text(&graph, "both", 2);
        assert!(text.find("← caller").unwrap() < text.find("→ test").unwrap());
        assert!(text.contains("→ test  src/lib.rs:2 [test] (seen)"));
    }

    /// Collapses hidden test children into one `+N tests` line at the parent's indent; the
    /// default header counts only rendered nodes, and showing tests restores `[test]` marks.
    #[test]
    fn call_graph_collapses_test_children_until_they_are_shown() {
        let mut collapsed = CallGraph::new(GraphNode {
            path: "src/lib.rs#root".into(),
            file: "src/lib.rs".into(),
            line: 1,
            is_test: false,
        });
        let production = collapsed
            .add_node(GraphNode {
                path: "src/lib.rs#prod".into(),
                file: "src/lib.rs".into(),
                line: 2,
                is_test: false,
            })
            .unwrap();
        collapsed.add_edge(GraphEdge {
            from: 0,
            to: production,
            direction: GraphDirection::Callers,
        });
        collapsed
            .collapsed_tests
            .insert((0, GraphDirection::Callers), 2);
        let text = call_graph_text(&collapsed, "callers", 2);
        assert!(text.contains("(depth 2, 2 nodes, 1 edges)"), "{text}");
        assert!(text.contains("← src/lib.rs#prod"), "{text}");
        assert!(text.contains("  +2 tests\n"), "{text}");
        assert!(!text.contains("[test]"), "{text}");

        let mut shown = CallGraph::new(GraphNode {
            path: "src/lib.rs#root".into(),
            file: "src/lib.rs".into(),
            line: 1,
            is_test: false,
        });
        for (path, is_test) in [("prod", false), ("t1", true), ("t2", true)] {
            let index = shown
                .add_node(GraphNode {
                    path: path.into(),
                    file: "src/lib.rs".into(),
                    line: 2,
                    is_test,
                })
                .unwrap();
            shown.add_edge(GraphEdge {
                from: 0,
                to: index,
                direction: GraphDirection::Callers,
            });
        }
        let text = call_graph_text(&shown, "callers", 2);
        assert!(text.contains("(depth 2, 4 nodes, 3 edges)"), "{text}");
        assert_eq!(text.matches("[test]").count(), 2, "{text}");
    }
}

/// Renders a file skeleton: one line per symbol, members indented, test modules collapsed.
pub fn outline_text(outline: &Outline) -> String {
    let mut out = format!(
        "{}  ({} lines, {})\n",
        outline.file.display(),
        outline.line_count,
        outline.language
    );
    let mut count = 0usize;
    let mut tests = 0usize;
    for symbol in &outline.symbols {
        render_symbol_line(symbol, 0, &mut out, &mut count, &mut tests);
    }
    out.push_str(&format!("  ({count} symbols"));
    if tests > 0 {
        out.push_str(&format!("; {tests} tests collapsed"));
    }
    out.push_str(")\n");
    out
}

/// Renders one bounded directory level, counting regular files recursively inside child folders.
pub fn directory_outline(root: &Path, directory: &Path) -> std::io::Result<String> {
    use std::fs;

    let absolute = root.join(directory);
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(&absolute)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "target" | "node_modules" | "__pycache__" | ".git"
            )
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            dirs.push((name.into_owned(), count_files(&entry.path())?));
        } else if kind.is_file() {
            files.push((name.into_owned(), entry.path()));
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let displayed = files.len().min(200);
    let title = if directory.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", directory.display())
    };
    let mut out = format!("{title}  ({} files, {} dirs)\n", files.len(), dirs.len());
    if !dirs.is_empty() {
        out.push_str("  dirs: ");
        out.push_str(
            &dirs
                .iter()
                .map(|(name, count)| format!("{name}/ {count}"))
                .collect::<Vec<_>>()
                .join(" · "),
        );
        out.push('\n');
    }
    for (name, path) in files.iter().take(displayed) {
        let (line_count, prefix) = read_file_lines_and_prefix(path)?;
        let doc = first_file_doc(path, &prefix);
        out.push_str(&format!("  {name:<20} {line_count:>5}"));
        if let Some(doc) = doc {
            out.push_str("  ");
            out.push_str(&clip(&doc, 60));
        }
        out.push('\n');
    }
    if files.len() > displayed {
        out.push_str(&format!("  … {} more files\n", files.len() - displayed));
    }
    Ok(out)
}

/// Counts file lines in constant memory and retains only the first 4 KiB for documentation.
fn read_file_lines_and_prefix(path: &Path) -> std::io::Result<(u32, Vec<u8>)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0; 8192];
    let mut prefix = Vec::with_capacity(4096);
    let mut newlines = 0u64;
    let mut last = None;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        newlines += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        last = bytes.last().copied();
        let take = (4096 - prefix.len()).min(read);
        prefix.extend_from_slice(&bytes[..take]);
    }
    let lines = newlines + u64::from(last.is_some() && last != Some(b'\n'));
    Ok((lines.min(u32::MAX as u64) as u32, prefix))
}

/// Counts non-hidden regular files below a directory, without following symlinks.
fn count_files(directory: &Path) -> std::io::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "target" | "node_modules" | "__pycache__" | ".git"
            )
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            count += count_files(&entry.path())?;
        } else if kind.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

/// Extracts the first module documentation line of `path` in the language that owns its
/// extension (see [`LanguageSupport::file_doc`](crate::lang::LanguageSupport::file_doc)); `None`
/// for non-UTF-8 bytes, an unowned extension or a file without module documentation.
fn first_file_doc(path: &Path, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    crate::lang::Language::for_path(path)?
        .support()
        .file_doc(text)
}

fn render_symbol_line(
    symbol: &Symbol,
    depth: usize,
    out: &mut String,
    count: &mut usize,
    tests: &mut usize,
) {
    if symbol.kind == SymbolKind::Test {
        // A test module or test function: count it and its members, print one collapsed line.
        let mut members = 0usize;
        symbol.walk(&mut |_| members += 1);
        *tests += members;
        out.push_str(&format!(
            "{:>5}  {}{} [{} tests collapsed]\n",
            symbol.range.start,
            "  ".repeat(depth),
            symbol.signature,
            members
        ));
        return;
    }
    *count += 1;
    let doc = symbol
        .doc
        .as_deref()
        .map(|doc| first_line(doc, 72))
        .filter(|doc| !doc.is_empty())
        .map(|doc| format!("    // {doc}"))
        .unwrap_or_default();
    out.push_str(&format!(
        "{:>5}  {}{}{}\n",
        symbol.range.start,
        "  ".repeat(depth),
        symbol.signature,
        doc
    ));
    for child in &symbol.children {
        render_symbol_line(child, depth + 1, out, count, tests);
    }
}

/// Renders a symbol body or an explicit line range with line numbers, header included.
pub fn read_text(file: &Path, title: Option<&str>, range: LineRange, source: &str) -> String {
    let mut out = match title {
        Some(title) => format!("{title}  (lines {range})\n"),
        None => format!("{}  (lines {range})\n", file.display()),
    };
    let width = range.end.to_string().len();
    for (index, line) in slice_lines(source, range).split_inclusive('\n').enumerate() {
        let number = range.start as usize + index;
        out.push_str(&format!("{number:>width$}  {line}"));
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// One usage line: relative path, line, and the source line text trimmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub file: String,
    pub line: u32,
    pub text: String,
    pub is_test: bool,
}

/// One caller or callee path and its location; `name` is `file#Owner/name` when resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub name: String,
    pub file: String,
    pub line: u32,
}

/// Everything a symbol card prints; absent parts are simply omitted.
#[derive(Clone, Debug, Default)]
pub struct SymbolCard {
    /// `BindingStatus — enum, src/assistance/host_binding.rs#BindingStatus (lines 330–346)`.
    pub heading: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
    /// Definition address: `file#symbol (lines a–b)`; read the body separately with `ide.read`.
    pub definition: Option<String>,
    pub usages: Vec<Usage>,
    /// Printed in place of an empty callers list when the language server has no call
    /// hierarchy (`unavailable (pyright has no call hierarchy)`).
    pub callers_note: Option<String>,
    pub callers: Vec<Call>,
    pub callees: Vec<Call>,
    /// Short sha, date and subject of recent commits touching the definition.
    pub history: Vec<String>,
    /// Detail reference for the full usage list, printed when usages were cut.
    pub more_detail: Option<String>,
    /// Prints the `usages:` line even with no references — the server legitimately answered
    /// none (pyright on a constructor nobody names explicitly).
    pub report_empty_usages: bool,
}

/// Renders the card with the fixed ceilings.
pub fn symbol_card_text(card: &SymbolCard) -> String {
    let mut out = format!("symbol: {}\n", card.heading);
    if let Some(signature) = &card.signature {
        out.push_str(&format!("signature: {signature}\n"));
    }
    if let Some(doc) = &card.doc {
        let doc = clip(doc, MAX_DOC_CHARS);
        let mut lines = doc.lines();
        if let Some(first) = lines.next() {
            out.push_str(&format!("doc: {first}\n"));
            for line in lines {
                out.push_str(&format!("     {line}\n"));
            }
        }
    }
    if let Some(definition) = &card.definition {
        out.push_str("definition ");
        out.push_str(definition);
        if !definition.ends_with('\n') {
            out.push('\n');
        }
    }
    if !card.usages.is_empty() {
        let (src, tests): (Vec<_>, Vec<_>) = card.usages.iter().partition(|usage| !usage.is_test);
        let files = card
            .usages
            .iter()
            .map(|usage| usage.file.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        out.push_str(&format!(
            "usages: {} in {} files (src {}, tests {})\n",
            card.usages.len(),
            files,
            src.len(),
            tests.len()
        ));
        let width = card
            .usages
            .iter()
            .take(MAX_USAGE_LINES)
            .map(|usage| usage.file.len() + 1 + usage.line.to_string().len())
            .max()
            .unwrap_or(0);
        for usage in src.iter().chain(tests.iter()).take(MAX_USAGE_LINES) {
            let location = format!("{}:{}", usage.file, usage.line);
            out.push_str(&format!("  {location:<width$}  {}\n", usage.text));
        }
        let hidden = card.usages.len().saturating_sub(MAX_USAGE_LINES);
        if hidden > 0 {
            match &card.more_detail {
                Some(detail) => {
                    out.push_str(&format!("  … {hidden} more (ide.inspect {detail})\n"))
                }
                None => out.push_str(&format!("  … {hidden} more\n")),
            }
        }
    } else if card.report_empty_usages {
        out.push_str("usages: 0 in 0 files (src 0, tests 0)\n");
    }
    if card.callers_note.is_some() && card.callers.is_empty() {
        out.push_str(&format!(
            "callers: {}\n",
            card.callers_note.as_deref().unwrap_or_default()
        ));
    } else {
        render_calls(&mut out, "callers", &card.callers);
    }
    render_calls(&mut out, "callees", &card.callees);
    if !card.history.is_empty() {
        out.push_str(&format!(
            "history: {} last commits touching the definition\n",
            card.history.len()
        ));
        for entry in &card.history {
            out.push_str(&format!("  {entry}\n"));
        }
    }
    out
}

fn render_calls(out: &mut String, label: &str, calls: &[Call]) {
    if calls.is_empty() {
        return;
    }
    out.push_str(&format!("{label}: {}\n", calls.len()));
    for call in calls.iter().take(MAX_CALL_LINES) {
        out.push_str(&format!("  {}  {}:{}\n", call.name, call.file, call.line));
    }
    if calls.len() > MAX_CALL_LINES {
        out.push_str(&format!("  … {} more\n", calls.len() - MAX_CALL_LINES));
    }
}

/// Text of one source line (1-based), trimmed, for usage listings.
pub fn line_text(source: &str, line: u32) -> String {
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(|text| clip(text.trim(), 120))
        .unwrap_or_default()
}

/// Relative display path for a location inside the worktree; absolute otherwise.
pub fn display_path(worktree: &Path, uri: &lsp::Url) -> String {
    match uri.to_file_path() {
        Ok(path) => path
            .strip_prefix(worktree)
            .map(|relative| relative.display().to_string())
            .unwrap_or_else(|_| path.display().to_string()),
        Err(_) => uri.to_string(),
    }
}

fn first_line(text: &str, max: usize) -> String {
    clip(text.lines().next().unwrap_or("").trim(), max)
}

/// Clips text to `max` characters, ending with `…` when truncated.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::SymbolPath;
    use std::path::PathBuf;

    #[test]
    fn directory_outline_lists_docs_and_counts_child_files() {
        let root = std::env::temp_dir().join(format!(
            "outline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::lang::testing::install();
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(
            root.join("src/lib.alpha"),
            "#!doc Alpha module docs\nfn a() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/sub/x"), "x\n").unwrap();
        let text = directory_outline(&root, Path::new("src")).unwrap();
        assert_eq!(
            text,
            "src/  (1 files, 1 dirs)\n  dirs: sub/ 1\n  lib.alpha                2  Alpha module docs\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn symbol(name: &str, kind: SymbolKind, start: u32, end: u32, doc: Option<&str>) -> Symbol {
        Symbol {
            path: SymbolPath::new(Some(PathBuf::from("a.rs")), vec![name.into()]),
            kind,
            name: name.into(),
            range: LineRange::new(start, end),
            body: LineRange::new(start, end),
            signature: format!("pub fn {name}()"),
            doc: doc.map(str::to_owned),
            children: vec![],
        }
    }

    #[test]
    fn outline_prints_lines_docs_and_collapses_tests() {
        let mut tests = symbol("tests", SymbolKind::Test, 40, 90, None);
        tests.signature = "mod tests".into();
        tests.children = vec![symbol("it_works", SymbolKind::Test, 42, 50, None)];
        let outline = Outline {
            file: PathBuf::from("a.rs"),
            language: crate::lang::testing::ALPHA,
            line_count: 90,
            symbols: vec![
                symbol(
                    "run",
                    SymbolKind::Function,
                    3,
                    20,
                    Some("Runs it.\n\nMore."),
                ),
                tests,
            ],
        };
        let text = outline_text(&outline);
        assert_eq!(
            text,
            "a.rs  (90 lines, alpha)\n    3  pub fn run()    // Runs it.\n   40  mod tests [2 tests collapsed]\n  (1 symbols; 2 tests collapsed)\n"
        );
    }

    #[test]
    fn read_numbers_lines_from_the_range_start() {
        let text = read_text(
            Path::new("a.rs"),
            Some("a.rs#run"),
            LineRange::new(9, 11),
            "1\n2\n3\n4\n5\n6\n7\n8\nfn run() {\n    x\n}\n",
        );
        assert_eq!(
            text,
            "a.rs#run  (lines 9–11)\n 9  fn run() {\n10      x\n11  }\n"
        );
    }

    /// The history section prints only when the card carries parsed commit entries, one
    /// two-space-indented `sha date subject` line each.
    #[test]
    fn symbol_card_prints_history_entries_only_when_present() {
        let mut card = SymbolCard {
            heading: "value — fn, src/lib.rs#value (lines 1–1)".into(),
            signature: Some("pub fn value() -> i32".into()),
            ..Default::default()
        };
        assert!(
            !symbol_card_text(&card).contains("history:"),
            "{}",
            symbol_card_text(&card)
        );
        card.history = vec![
            "59482ce 2026-09-26 tune value bound".into(),
            "6290c82 2026-09-26 cross-crate fixture".into(),
        ];
        assert!(
            symbol_card_text(&card).contains(
                "history: 2 last commits touching the definition\n  \
                 59482ce 2026-09-26 tune value bound\n  \
                 6290c82 2026-09-26 cross-crate fixture\n"
            ),
            "{}",
            symbol_card_text(&card)
        );
    }

    #[test]
    fn symbol_card_lists_usages_with_ceiling_and_groups() {
        let mut card = SymbolCard {
            heading: "Run — fn, a.rs#Run (lines 1–3)".into(),
            signature: Some("pub fn run()".into()),
            doc: Some("Runs.".into()),
            definition: Some("a.rs#Run  (lines 1–3)".into()),
            callers: vec![Call {
                name: "a.rs#Owner/caller".into(),
                file: "a.rs".into(),
                line: 8,
            }],
            ..Default::default()
        };
        for index in 0..35 {
            card.usages.push(Usage {
                file: format!("f{}.rs", index % 3),
                line: index + 1,
                text: "run();".into(),
                is_test: index % 5 == 0,
            });
        }
        card.more_detail = Some("sym-1".into());
        let text = symbol_card_text(&card);
        assert!(text.starts_with("symbol: Run — fn, a.rs#Run (lines 1–3)\nsignature: pub fn run()\ndoc: Runs.\ndefinition a.rs#Run  (lines 1–3)\nusages: 35 in 3 files (src 28, tests 7)\n"));
        assert!(text.contains("… 5 more (ide.inspect sym-1)"));
        assert!(text.contains("callers: 1\n  a.rs#Owner/caller  a.rs:8\n"));
        assert_eq!(text.matches("run();").count(), MAX_USAGE_LINES);
    }

    /// A callers note replaces an empty callers list, and an explicitly answered zero prints
    /// the `usages:` line instead of omitting it.
    #[test]
    fn symbol_card_reports_callers_note_and_explicit_zero_usages() {
        let mut card = SymbolCard {
            heading: "__init__ — method, main.py#Greeter/__init__ (lines 2–3)".into(),
            callers_note: Some("unavailable (pyright has no call hierarchy)".into()),
            report_empty_usages: true,
            ..Default::default()
        };
        let text = symbol_card_text(&card);
        assert!(
            text.contains("usages: 0 in 0 files (src 0, tests 0)\n"),
            "{text}"
        );
        assert!(
            text.contains("callers: unavailable (pyright has no call hierarchy)\n"),
            "{text}"
        );
        assert!(!text.contains("callers: 0"), "{text}");

        // A note never suppresses a real callers list.
        card.callers.push(Call {
            name: "main.py#caller".into(),
            file: "main.py".into(),
            line: 8,
        });
        let text = symbol_card_text(&card);
        assert!(
            text.contains("callers: 1\n  main.py#caller  main.py:8\n"),
            "{text}"
        );
    }
}
