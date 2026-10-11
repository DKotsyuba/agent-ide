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
/// Usage-row tag of a site that builds a value of the struct, enum or class (a lexical rule; the
/// `~` makes the card print its heuristic-match legend). The card's usages line counts them.
pub const CONSTRUCTION_TAG: &str = "construction ~";

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

    /// Returns the plural noun of this direction's section, `callers` or `callees`, as printed
    /// in an `unavailable (<cause>)` line.
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
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
    /// Printed after the location as `[tag]`: the language of a cross-language link node, or the
    /// kind of a name node; `None` for call-hierarchy nodes.
    pub tag: Option<String>,
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
    /// A cross-language name link (a use of a name `from` defines, or a name `from` uses),
    /// rendered `⇢`, not a call.
    pub link: bool,
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
    /// Call-hierarchy requests that failed, keyed by the queried node index and direction, with
    /// the cause rendered in parentheses. A failed request is reported where it happened and is
    /// never rendered as an absence of calls.
    pub faults: std::collections::BTreeMap<(usize, GraphDirection), String>,
    /// Tagged symbol addresses (`file#unit (L123)`) by node `(file, line)`; a node with one is
    /// printed as that address in place of its raw path and `file:line`.
    pub addresses: LocationAddresses,
}

impl CallGraph {
    /// Starts a graph with its root symbol at index zero.
    pub fn new(root: GraphNode) -> Self {
        Self {
            nodes: vec![root],
            edges: Vec::new(),
            collapsed_tests: Default::default(),
            capped: false,
            faults: Default::default(),
            addresses: Default::default(),
        }
    }

    /// The node as the graph names it: its tagged address when resolved, else the raw path.
    fn node_name(&self, node: &GraphNode) -> String {
        match self.addresses.get(&(node.file.clone(), node.line)) {
            Some(address) => address.clone(),
            None => node.path.clone(),
        }
    }

    /// Records that the `direction` call-hierarchy request of node `node` failed with `cause`
    /// (for example `incoming calls request failed`). The first cause recorded for one node and
    /// direction is kept; recording never adds a node or an edge.
    pub fn note_fault(&mut self, node: usize, direction: GraphDirection, cause: String) {
        self.faults.entry((node, direction)).or_insert(cause);
    }

    /// The cause when the root has no answer at all for `directions`: every listed direction's
    /// request faulted and no edge of those directions leaves the root (a cross-language link
    /// edge is an answer). Several distinct causes are joined with `; `. `None` when any listed
    /// direction answered or `directions` is empty.
    pub fn root_unavailable(&self, directions: &[GraphDirection]) -> Option<String> {
        if directions.is_empty()
            || self
                .edges
                .iter()
                .any(|edge| edge.from == 0 && directions.contains(&edge.direction))
        {
            return None;
        }
        let mut causes: Vec<&str> = Vec::new();
        for direction in directions {
            let cause = self.faults.get(&(0, *direction))?;
            if !causes.contains(&cause.as_str()) {
                causes.push(cause);
            }
        }
        Some(causes.join("; "))
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
///
/// A failed call-hierarchy request is never rendered as an absence of calls:
/// - when the root has no answer for any selected direction, the whole reply is the single line
///   `graph: <subject> of <root> unavailable (<cause>)`;
/// - a root direction that faulted while another answered prints `<noun>: unavailable (<cause>)`
///   before that direction's rows;
/// - a deeper node whose request faulted ends its row with `(<noun> unavailable: <cause>)`.
pub fn call_graph_text(graph: &CallGraph, direction: &str, depth: u8) -> String {
    let root = &graph.nodes[0];
    let subject = match direction {
        "both" => "callers and callees",
        "callees" => "callees",
        _ => "callers",
    };
    let directions: &[GraphDirection] = match direction {
        "both" => &[GraphDirection::Callers, GraphDirection::Callees],
        "callees" => &[GraphDirection::Callees],
        _ => &[GraphDirection::Callers],
    };
    if let Some(cause) = graph.root_unavailable(directions) {
        return format!(
            "graph: {subject} of {} unavailable ({cause})\n",
            graph.node_name(root)
        );
    }
    let mut out = format!(
        "graph: {subject} of {} (depth {depth}, {} nodes, {} edges)\n",
        graph.node_name(root),
        graph.nodes.len(),
        graph.edges.len()
    );
    let mut seen = vec![false; graph.nodes.len()];
    seen[0] = true;
    for selected in directions {
        if let Some(cause) = graph.faults.get(&(0, *selected)) {
            out.push_str(&format!("{}: unavailable ({cause})\n", selected.noun()));
        }
        render_graph_children(graph, 0, *selected, 1, depth, &mut seen, &mut out);
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
        let arrow = if edge.link { "⇢" } else { direction.arrow() };
        out.push_str(&format!(
            "{}{arrow} {}",
            "  ".repeat(level as usize),
            graph.node_name(node)
        ));
        // A resolved address already carries the file and the absolute line.
        if !node.file.is_empty()
            && !graph
                .addresses
                .contains_key(&(node.file.clone(), node.line))
        {
            out.push_str(&format!("  {}:{}", node.file, node.line));
        }
        if let Some(tag) = &node.tag {
            out.push_str(&format!(" [{tag}]"));
        }
        if !already_seen {
            seen[edge.to] = true;
        }
        if node.is_test {
            out.push_str(" [test]");
        }
        if already_seen {
            out.push_str(" (seen)");
        } else if let Some(cause) = graph.faults.get(&(edge.to, direction)) {
            out.push_str(&format!(" ({} unavailable: {cause})", direction.noun()));
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
            tag: None,
        });
        for index in 1..3 {
            let node = graph
                .add_node(GraphNode {
                    path: format!("src/lib.rs#f{index}"),
                    file: "src/lib.rs".to_owned(),
                    line: index + 1,
                    is_test: index == 2,
                    tag: None,
                })
                .unwrap();
            graph.add_edge(GraphEdge {
                from: node - 1,
                to: node,
                direction: GraphDirection::Callers,
                link: false,
            });
        }
        graph.add_edge(GraphEdge {
            from: 2,
            to: 0,
            direction: GraphDirection::Callers,
            link: false,
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
                        tag: None,
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
                    tag: None,
                })
                .is_none()
        );
        assert!(graph.capped);
        assert!(call_graph_text(&graph, "callers", 1).contains("… capped at 60 nodes"));
    }

    /// A failed call-hierarchy request is reported where it happened, never as missing calls: a
    /// root with no answer is one `unavailable` line, a root direction that faulted beside one
    /// that answered prints its own line, and a deeper node carries the cause on its row.
    #[test]
    fn call_graph_reports_faulted_requests_as_unavailable() {
        let node = |path: &str, line: u32| GraphNode {
            path: path.to_owned(),
            file: "src/lib.rs".to_owned(),
            line,
            is_test: false,
            tag: None,
        };
        let mut graph = CallGraph::new(node("root", 1));
        graph.note_fault(
            0,
            GraphDirection::Callers,
            "incoming calls request failed".into(),
        );
        assert_eq!(
            call_graph_text(&graph, "callers", 2),
            "graph: callers of root unavailable (incoming calls request failed)\n"
        );

        // Callees answered, callers did not: the graph renders with the callers line.
        let callee = graph.add_node(node("callee", 5)).unwrap();
        graph.add_edge(GraphEdge {
            from: 0,
            to: callee,
            direction: GraphDirection::Callees,
            link: false,
        });
        graph.note_fault(
            callee,
            GraphDirection::Callees,
            "outgoing calls request failed".into(),
        );
        let text = call_graph_text(&graph, "both", 2);
        assert!(
            text.starts_with("graph: callers and callees of root (depth 2, 2 nodes, 1 edges)\n"),
            "{text}"
        );
        assert!(
            text.contains("callers: unavailable (incoming calls request failed)\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "→ callee  src/lib.rs:5 (callees unavailable: outgoing calls request failed)\n"
            ),
            "{text}"
        );
        assert!(
            graph
                .root_unavailable(&[GraphDirection::Callers, GraphDirection::Callees])
                .is_none()
        );
        assert_eq!(
            graph
                .root_unavailable(&[GraphDirection::Callers])
                .as_deref(),
            Some("incoming calls request failed")
        );
    }

    /// Keeps callers before callees for a two-sided graph and marks a repeated test node.
    #[test]
    fn call_graph_renders_both_sides_in_order_and_marks_seen_tests() {
        let mut graph = CallGraph::new(GraphNode {
            path: "root".into(),
            file: "src/lib.rs".into(),
            line: 1,
            is_test: false,
            tag: None,
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
                    tag: None,
                })
                .unwrap();
            graph.add_edge(GraphEdge {
                from: 0,
                to: index,
                direction,
                link: false,
            });
        }
        graph.add_edge(GraphEdge {
            from: 1,
            to: 2,
            direction: GraphDirection::Callers,
            link: false,
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
            tag: None,
        });
        let production = collapsed
            .add_node(GraphNode {
                path: "src/lib.rs#prod".into(),
                file: "src/lib.rs".into(),
                line: 2,
                is_test: false,
                tag: None,
            })
            .unwrap();
        collapsed.add_edge(GraphEdge {
            from: 0,
            to: production,
            direction: GraphDirection::Callers,
            link: false,
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
            tag: None,
        });
        for (path, is_test) in [("prod", false), ("t1", true), ("t2", true)] {
            let index = shown
                .add_node(GraphNode {
                    path: path.into(),
                    file: "src/lib.rs".into(),
                    line: 2,
                    is_test,
                    tag: None,
                })
                .unwrap();
            shown.add_edge(GraphEdge {
                from: 0,
                to: index,
                direction: GraphDirection::Callers,
                link: false,
            });
        }
        let text = call_graph_text(&shown, "callers", 2);
        assert!(text.contains("(depth 2, 4 nodes, 3 edges)"), "{text}");
        assert_eq!(text.matches("[test]").count(), 2, "{text}");
    }
}

/// One candidate row of a bare-name answer: `<address>  <kind>  <signature>  (lines a–b)`.
pub fn candidate_row(unit: &super::address::Unit) -> String {
    let kind = match unit.kind {
        super::address::UnitKind::Symbol(kind) => kind.name(),
        super::address::UnitKind::Region(_) => "region",
    };
    format!(
        "{}  {kind}  {}  (lines {})",
        unit.address, unit.signature, unit.span
    )
}

/// Kind filter of [`address_outline_text`]: the selected symbol kinds, whether regions show, and
/// the caller's raw list for the footer.
pub struct OutlineFilter<'a> {
    /// Selected closed kind names that parsed.
    pub kinds: &'a [SymbolKind],
    /// The list named `region`, so file-level regions are shown.
    pub regions: bool,
    /// The raw comma list, printed back unchanged.
    pub requested: &'a str,
}

/// Renders a file as one row per unit in file order, regions included:
/// `<first>–<last>  <address>  <signature>  // <doc>` (regions have no signature; blank `@after`
/// regions are skipped; a test module is one row ending `[N tests collapsed]`). `filter` limits
/// symbols to the selected kinds (a container of a selected member stays for context, tests stay
/// whole) and hides regions unless it asks for them.
pub fn address_outline_text(
    outline: &Outline,
    map: &super::address::AddressMap,
    filter: Option<&OutlineFilter<'_>>,
) -> String {
    use super::address::UnitKind;

    let units = map.units();
    // Parent of each unit by depth, then keep flags propagated from selected kinds upward.
    let mut parent = vec![None; units.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|&top| units[top].depth >= unit.depth)
        {
            stack.pop();
        }
        parent[i] = stack.last().copied();
        stack.push(i);
    }
    let mut keep: Vec<bool> = units
        .iter()
        .map(|unit| match (filter, unit.kind) {
            (None, _) => true,
            (Some(f), UnitKind::Region(_)) => f.regions,
            (Some(f), UnitKind::Symbol(kind)) => {
                kind == SymbolKind::Test || f.kinds.contains(&kind)
            }
        })
        .collect();
    for i in (0..units.len()).rev() {
        if keep[i]
            && let Some(p) = parent[i]
            && matches!(units[p].kind, UnitKind::Symbol(_))
        {
            keep[p] = true;
        }
    }
    let mut out = format!(
        "{}  ({} lines, {})\n",
        outline.file.display(),
        outline.line_count,
        outline.language
    );
    let (mut symbols, mut regions, mut tests, mut total) = (0usize, 0usize, 0usize, 0usize);
    let mut collapsed_until = 0u32;
    for (i, unit) in units.iter().enumerate() {
        if unit.span.start <= collapsed_until && matches!(unit.kind, UnitKind::Symbol(_)) {
            continue;
        }
        match unit.kind {
            UnitKind::Region(_) => {
                if unit.blank {
                    continue;
                }
                if keep[i] {
                    regions += 1;
                    out.push_str(&format!("{}  {}\n", unit.span, unit.address));
                }
            }
            UnitKind::Symbol(SymbolKind::Test) => {
                // A test module or function: count it and its members, print one collapsed line.
                let members = units
                    .iter()
                    .filter(|other| {
                        matches!(other.kind, UnitKind::Symbol(_))
                            && other.span.start >= unit.span.start
                            && other.span.end <= unit.span.end
                    })
                    .count();
                collapsed_until = unit.span.end;
                tests += members;
                out.push_str(&format!(
                    "{}  {}  {} [{members} tests collapsed]\n",
                    unit.span, unit.address, unit.signature
                ));
            }
            UnitKind::Symbol(_) => {
                total += 1;
                if keep[i] {
                    symbols += 1;
                    let doc = unit
                        .doc
                        .as_deref()
                        .map(|doc| first_line(doc, 72))
                        .filter(|doc| !doc.is_empty())
                        .map(|doc| format!("  // {doc}"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "{}  {}  {}{doc}\n",
                        unit.span, unit.address, unit.signature
                    ));
                }
            }
        }
    }
    let region_note = if regions > 0 {
        format!(", {regions} regions")
    } else {
        String::new()
    };
    match filter {
        None => out.push_str(&format!("  ({symbols} symbols{region_note}")),
        Some(f) => out.push_str(&format!(
            "  ({total} symbols; showing {symbols}{region_note} by kinds {}",
            f.requested
        )),
    }
    if tests > 0 {
        out.push_str(&format!("; {tests} tests collapsed"));
    }
    out.push_str(")\n");
    out
}

/// Renders a file skeleton: one declaration line per symbol, members indented, tests collapsed.
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

/// Renders `outline` limited to `kinds` (closed [`SymbolKind`] names): a symbol shows when its
/// own kind is selected or a descendant's is, so a container of a selected member still shows for
/// context. Test modules are always kept whole and collapse exactly as [`outline_text`] collapses
/// them. `requested` is the caller's raw comma list, printed back in the footer unchanged.
pub fn filtered_outline_text(outline: &Outline, kinds: &[SymbolKind], requested: &str) -> String {
    let total = {
        let mut count = 0usize;
        let mut tests = 0usize;
        let mut scratch = String::new();
        for symbol in &outline.symbols {
            render_symbol_line(symbol, 0, &mut scratch, &mut count, &mut tests);
        }
        count
    };
    let mut out = format!(
        "{}  ({} lines, {})\n",
        outline.file.display(),
        outline.line_count,
        outline.language
    );
    let mut count = 0usize;
    let mut tests = 0usize;
    for symbol in &outline.symbols {
        if let Some(kept) = keep_by_kind(symbol, kinds) {
            render_symbol_line(&kept, 0, &mut out, &mut count, &mut tests);
        }
    }
    out.push_str(&format!(
        "  ({total} symbols; showing {count} by kinds {requested}"
    ));
    if tests > 0 {
        out.push_str(&format!("; tests collapsed: {tests}"));
    }
    out.push_str(")\n");
    out
}

/// Keeps `symbol` when its own kind is selected or a descendant's is; test modules are always
/// kept whole so the filter never disturbs their normal collapse.
fn keep_by_kind(symbol: &Symbol, kinds: &[SymbolKind]) -> Option<Symbol> {
    if symbol.kind == SymbolKind::Test {
        return Some(symbol.clone());
    }
    let children: Vec<Symbol> = symbol
        .children
        .iter()
        .filter_map(|child| keep_by_kind(child, kinds))
        .collect();
    if kinds.contains(&symbol.kind) || !children.is_empty() {
        let mut kept = symbol.clone();
        kept.children = children;
        Some(kept)
    } else {
        None
    }
}

/// Renders one bounded directory level, counting regular files recursively inside child folders.
/// File documentation is computed where each file's language computes; a module failure is
/// returned as an error wrapping its [`ModuleUnavailable`](crate::modules::contract::ModuleUnavailable).
pub async fn directory_outline(root: &Path, directory: &Path) -> std::io::Result<String> {
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
        let doc = first_file_doc(root, path, &prefix)
            .await
            .map_err(std::io::Error::other)?;
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
async fn first_file_doc(
    root: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<Option<String>, crate::modules::contract::ModuleUnavailable> {
    let (Ok(text), Some(language)) = (
        std::str::from_utf8(bytes),
        crate::lang::Language::for_path(path),
    ) else {
        return Ok(None);
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    crate::modules::calls::file_doc(language, root, relative, text).await
}

/// Renders a symbol at its declaration line while its stored range keeps attached documentation.
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
            symbol.body.start,
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
        symbol.body.start,
        "  ".repeat(depth),
        symbol.signature,
        doc
    ));
    for child in &symbol.children {
        render_symbol_line(child, depth + 1, out, count, tests);
    }
}

/// Renders a symbol body or an explicit line range with line numbers, header included. The
/// gutter is one tab — the separator agents already strip from their own read tool — so a
/// copied `old_string` never keeps separator spaces.
pub fn read_text(file: &Path, title: Option<&str>, range: LineRange, source: &str) -> String {
    let mut out = match title {
        Some(title) => format!("{title}  (lines {range})\n"),
        None => format!("{}  (lines {range})\n", file.display()),
    };
    let width = range.end.to_string().len();
    for (index, line) in slice_lines(source, range).split_inclusive('\n').enumerate() {
        let number = range.start as usize + index;
        out.push_str(&format!("{number:>width$}\t{line}"));
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
    /// Printed before the text: `[<language>]` or `[<language> ~<reason>]` for an index-backed
    /// row; `None` for a language-server row (printed untagged).
    pub tag: Option<String>,
}

/// Printed locations by `(file, line)`: the tagged address `file#unit:N (L123)` that replaces
/// `file:line` in a row when the location resolved.
pub type LocationAddresses = std::collections::BTreeMap<(String, u32), String>;

/// A row's location text: the resolved address with its absolute line, else `file:line`.
fn location_text(addresses: &LocationAddresses, file: &str, line: u32) -> String {
    addresses
        .get(&(file.to_owned(), line))
        .cloned()
        .unwrap_or_else(|| format!("{file}:{line}"))
}

impl Usage {
    /// The row as printed, with its location padded to `width`.
    fn row(&self, width: usize, addresses: &LocationAddresses) -> String {
        let location = location_text(addresses, &self.file, self.line);
        match &self.tag {
            Some(tag) => format!("  {location:<width$}  {tag} {}\n", self.text),
            None => format!("  {location:<width$}  {}\n", self.text),
        }
    }
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
    /// Printed in place of an empty usages list when nothing can answer references
    /// (`unavailable (<language> has no language server)`).
    pub usages_note: Option<String>,
    /// Printed in place of an empty callers list when the language server has no call
    /// hierarchy (`unavailable (<server> has no call hierarchy)`).
    pub callers_note: Option<String>,
    pub callers: Vec<Call>,
    /// Printed in place of an empty callees list when callees were requested: `unavailable
    /// (<reason>)` for a failed request, or `0` when the server legitimately answered none.
    /// A requested section is never silently omitted.
    pub callees_note: Option<String>,
    pub callees: Vec<Call>,
    /// Short sha, date and subject of recent commits touching the definition.
    pub history: Vec<String>,
    /// Prints the fixed no-Git-data sentence instead of commits: the activated folder is a plain
    /// directory, so a requested history section still answers.
    pub history_unavailable: bool,
    /// Detail reference for the full usage list, printed when usages were cut.
    pub more_detail: Option<String>,
    /// Prints the `usages:` line even with no references — the server legitimately answered
    /// none (a server answering nothing for a constructor nobody names explicitly).
    pub report_empty_usages: bool,
    /// Rows under a `definitions:` line (a name card), printed after the heading block.
    pub definitions: Vec<String>,
    /// `defines:` lines: the cross-language names this symbol defines.
    pub defines: Vec<String>,
    /// The usages come from the name index: counts read `N indexed in`, not live.
    pub usages_indexed: bool,
    /// Index rows dropped because their file could no longer be read.
    pub usages_dropped: usize,
    /// Lines printed after the usages as they are (`links:` section, `unavailable for:`, index
    /// state).
    pub links: Vec<String>,
    /// Tagged position addresses of usage rows, replacing `file:line` where resolved.
    pub usage_addresses: LocationAddresses,
    /// Tagged symbol addresses of caller and callee rows, replacing `file:line` where resolved.
    pub call_addresses: LocationAddresses,
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
    if !card.definitions.is_empty() {
        out.push_str("definitions:\n");
        for row in &card.definitions {
            out.push_str(&format!("  {row}\n"));
        }
    }
    for defines in &card.defines {
        out.push_str(&format!("defines: {defines}\n"));
    }
    let counted = if card.usages_indexed {
        "indexed in"
    } else {
        "in"
    };
    let dropped = match card.usages_dropped {
        0 => String::new(),
        dropped => format!("; {dropped} stale dropped"),
    };
    if !card.usages.is_empty() {
        let (src, tests): (Vec<_>, Vec<_>) = card.usages.iter().partition(|usage| !usage.is_test);
        let files = card
            .usages
            .iter()
            .map(|usage| usage.file.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let constructions = match card
            .usages
            .iter()
            .filter(|usage| usage.tag.as_deref() == Some(CONSTRUCTION_TAG))
            .count()
        {
            0 => String::new(),
            count => format!("; {count} constructions"),
        };
        out.push_str(&format!(
            "usages: {} {counted} {} files (src {}, tests {}{dropped}{constructions})\n",
            card.usages.len(),
            files,
            src.len(),
            tests.len()
        ));
        let width = card
            .usages
            .iter()
            .take(MAX_USAGE_LINES)
            .map(|usage| location_text(&card.usage_addresses, &usage.file, usage.line).len())
            .max()
            .unwrap_or(0);
        for usage in src.iter().chain(tests.iter()).take(MAX_USAGE_LINES) {
            out.push_str(&usage.row(width, &card.usage_addresses));
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
        if card
            .usages
            .iter()
            .any(|usage| usage.tag.as_deref().is_some_and(|tag| tag.contains('~')))
        {
            out.push_str("  (~ = heuristic match, not proven)\n");
        }
    } else if let Some(note) = &card.usages_note {
        out.push_str(&format!("usages: {note}\n"));
    } else if card.report_empty_usages {
        out.push_str(&format!(
            "usages: 0 {counted} 0 files (src 0, tests 0{dropped})\n"
        ));
    }
    for line in &card.links {
        out.push_str(line);
        out.push('\n');
    }
    if card.callers_note.is_some() && card.callers.is_empty() {
        out.push_str(&format!(
            "callers: {}\n",
            card.callers_note.as_deref().unwrap_or_default()
        ));
    } else {
        render_calls(&mut out, "callers", &card.callers, &card.call_addresses);
    }
    if card.callees_note.is_some() && card.callees.is_empty() {
        out.push_str(&format!(
            "callees: {}\n",
            card.callees_note.as_deref().unwrap_or_default()
        ));
    } else {
        render_calls(&mut out, "callees", &card.callees, &card.call_addresses);
    }
    if card.history_unavailable {
        out.push_str(&format!(
            "history: {}\n",
            crate::workspace::authority::NO_GIT_DATA
        ));
    } else if !card.history.is_empty() {
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

/// Usage rows the card cut after [`MAX_USAGE_LINES`], in the card's order (source files first,
/// then tests), headed `usages 31–N of N:`; `None` when nothing was cut. Retained behind the
/// card's `detail_ref` and delivered by `ide.inspect`.
pub fn hidden_usages_text(card: &SymbolCard) -> Option<String> {
    let (src, tests): (Vec<_>, Vec<_>) = card.usages.iter().partition(|usage| !usage.is_test);
    let hidden: Vec<&Usage> = src.into_iter().chain(tests).skip(MAX_USAGE_LINES).collect();
    if hidden.is_empty() {
        return None;
    }
    let total = card.usages.len();
    let mut out = format!("usages {}–{total} of {total}:\n", MAX_USAGE_LINES + 1);
    let width = hidden
        .iter()
        .map(|usage| location_text(&card.usage_addresses, &usage.file, usage.line).len())
        .max()
        .unwrap_or(0);
    for usage in hidden {
        out.push_str(&usage.row(width, &card.usage_addresses));
    }
    Some(out)
}

fn render_calls(out: &mut String, label: &str, calls: &[Call], addresses: &LocationAddresses) {
    if calls.is_empty() {
        return;
    }
    out.push_str(&format!("{label}: {}\n", calls.len()));
    for call in calls.iter().take(MAX_CALL_LINES) {
        match addresses.get(&(call.file.clone(), call.line)) {
            // The symbol address names the node and carries its file and absolute line.
            Some(address) => out.push_str(&format!("  {address}\n")),
            None => out.push_str(&format!("  {}  {}:{}\n", call.name, call.file, call.line)),
        }
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

    #[tokio::test]
    async fn directory_outline_lists_docs_and_counts_child_files() {
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
        let text = directory_outline(&root, Path::new("src")).await.unwrap();
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
    fn filtered_outline_keeps_containers_of_selected_members_and_collapses_tests_unfiltered() {
        let mut foo = symbol("Foo", SymbolKind::Struct, 1, 10, None);
        foo.children = vec![
            symbol("x", SymbolKind::Field, 2, 2, None),
            symbol("bar", SymbolKind::Method, 3, 5, None),
        ];
        let run = symbol("run", SymbolKind::Function, 12, 14, None);
        let mut tests = symbol("tests", SymbolKind::Test, 16, 20, None);
        tests.signature = "mod tests".into();
        tests.children = vec![symbol("it_works", SymbolKind::Test, 17, 19, None)];
        let outline = Outline {
            file: PathBuf::from("a.rs"),
            language: crate::lang::testing::ALPHA,
            line_count: 20,
            symbols: vec![foo, run, tests],
        };
        let text = filtered_outline_text(&outline, &[SymbolKind::Method], "method");
        assert_eq!(
            text,
            "a.rs  (20 lines, alpha)\n    1  pub fn Foo()\n    3    pub fn bar()\n   16  mod tests [2 tests collapsed]\n  (4 symbols; showing 2 by kinds method; tests collapsed: 2)\n"
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
            "a.rs#run  (lines 9–11)\n 9\tfn run() {\n10\t    x\n11\t}\n"
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

    /// A requested history section in a plain directory prints why no commits can be listed.
    #[test]
    fn symbol_card_explains_history_without_git_data() {
        let card = SymbolCard {
            heading: "simulate — fn, sim.py#simulate (lines 1–2)".into(),
            history_unavailable: true,
            ..Default::default()
        };
        assert!(symbol_card_text(&card).contains("history: not a git repository: no git data\n"));
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
                tag: None,
            });
        }
        card.more_detail = Some("sym-1".into());
        let text = symbol_card_text(&card);
        assert!(text.starts_with("symbol: Run — fn, a.rs#Run (lines 1–3)\nsignature: pub fn run()\ndoc: Runs.\ndefinition a.rs#Run  (lines 1–3)\nusages: 35 in 3 files (src 28, tests 7)\n"));
        assert!(text.contains("… 5 more (ide.inspect sym-1)"));
        assert!(text.contains("callers: 1\n  a.rs#Owner/caller  a.rs:8\n"));
        assert_eq!(text.matches("run();").count(), MAX_USAGE_LINES);
        // Sources come first, so the cut rows are the last five tests, in card order.
        assert_eq!(
            hidden_usages_text(&card).unwrap(),
            "usages 31–35 of 35:\n  f1.rs:11  run();\n  f0.rs:16  run();\n  \
             f2.rs:21  run();\n  f1.rs:26  run();\n  f0.rs:31  run();\n"
        );
        card.usages.truncate(MAX_USAGE_LINES);
        assert_eq!(hidden_usages_text(&card), None);
    }

    /// Construction rows are counted on the usages line and print their tag and the heuristic
    /// legend; a card without them keeps the 0.10.8 line.
    #[test]
    fn symbol_card_counts_construction_sites() {
        let usage = |line: u32, tag: Option<&str>| Usage {
            file: "src/a.rs".into(),
            line,
            text: "let s = Store::new();".into(),
            is_test: false,
            tag: tag.map(str::to_owned),
        };
        let mut card = SymbolCard {
            heading: "Store — struct, src/a.rs#Store (lines 1–3)".into(),
            usages: vec![usage(7, Some(CONSTRUCTION_TAG)), usage(9, None)],
            ..Default::default()
        };
        let text = symbol_card_text(&card);
        assert!(
            text.contains("usages: 2 in 1 files (src 2, tests 0; 1 constructions)\n"),
            "{text}"
        );
        assert!(
            text.contains("src/a.rs:7  construction ~ let s = Store::new();"),
            "{text}"
        );
        assert!(text.contains("(~ = heuristic match, not proven)"), "{text}");
        card.usages[0].tag = None;
        assert!(
            symbol_card_text(&card).contains("usages: 2 in 1 files (src 2, tests 0)\n"),
            "{text}"
        );
    }

    /// A callers note replaces an empty callers list, and an explicitly answered zero prints
    /// the `usages:` line instead of omitting it.
    #[test]
    fn symbol_card_reports_callers_note_and_explicit_zero_usages() {
        let mut card = SymbolCard {
            heading: "__init__ — method, main.py#Greeter/__init__ (lines 2–3)".into(),
            callers_note: Some("unavailable (fake-server has no call hierarchy)".into()),
            report_empty_usages: true,
            ..Default::default()
        };
        let text = symbol_card_text(&card);
        assert!(
            text.contains("usages: 0 in 0 files (src 0, tests 0)\n"),
            "{text}"
        );
        assert!(
            text.contains("callers: unavailable (fake-server has no call hierarchy)\n"),
            "{text}"
        );
        assert!(!text.contains("callers: 0"), "{text}");

        for note in [
            "unavailable (prepare call hierarchy request failed)",
            "unavailable (incoming calls request failed)",
            "unavailable (fake-server still indexing)",
        ] {
            card.callers_note = Some(note.to_owned());
            assert!(
                symbol_card_text(&card).contains(&format!("callers: {note}\n")),
                "{note}"
            );
        }

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

    /// A requested callees section always answers: the note states a failed request or an
    /// explicitly answered zero, and never suppresses a real list.
    #[test]
    fn symbol_card_never_omits_a_requested_callees_section() {
        let mut card = SymbolCard {
            heading: "work — method, a.rs#Service/work (lines 4–5)".into(),
            callees_note: Some("unavailable (call hierarchy request failed)".into()),
            ..Default::default()
        };
        let text = symbol_card_text(&card);
        assert!(
            text.contains("callees: unavailable (call hierarchy request failed)\n"),
            "{text}"
        );
        card.callees_note = Some("0".into());
        let text = symbol_card_text(&card);
        assert!(text.contains("callees: 0\n"), "{text}");
        // A note never suppresses a real callees list.
        card.callees.push(Call {
            name: "a.rs#Service/helper".into(),
            file: "a.rs".into(),
            line: 6,
        });
        let text = symbol_card_text(&card);
        assert!(
            text.contains("callees: 1\n  a.rs#Service/helper  a.rs:6\n"),
            "{text}"
        );
    }
}
