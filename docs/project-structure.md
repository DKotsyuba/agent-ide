# Project structure builder

`agent_ide::project::structure::build_structure(root, &options)` returns an owned,
serializable tree. `render_tree(&tree)` renders those same data without another walk.
Register the bundled languages with `agent_ide::languages::install()` before collecting
source headers, imports or skeletons. No daemon, model or language server is started.

```rust
use std::path::Path;
use agent_ide::project::structure::{build_structure, render_tree, StructureOptions};
agent_ide::languages::install();
let tree = build_structure(Path::new("."), &StructureOptions {
    max_depth: 4,
    expand_paths: vec!["crates/agent-ide-lang-css".into()],
    include_header: true,
    include_skeleton: true,
    include_imports: true,
    include_line_count: true,
    ..Default::default()
})?;
println!("{}", render_tree(&tree));
let json = serde_json::to_string_pretty(&tree)?;
// Use inside a function returning Result.
```

The runnable example accepts the same root and collection switches:

```sh
cargo run --offline --locked --example project_structure -- . --depth 4 --dirs-only --expand crates/agent-ide-lang-css
cargo run --offline --locked --example project_structure -- crates/agent-ide-lang-css/src --depth 1 --headers --skeleton --imports --lines --size
cargo run --offline --locked --example project_structure -- . --changed-only --diff
cargo run --offline --locked --example project_structure -- . --changed-only --diff --json
```

Options are independent: depth, files/directories, hidden names, Git ignore rules,
generated-directory exclusion, explicit excluded paths, extension selection, expanded
paths, changed-only selection, diffs, leading headers, skeletons, compact imports,
byte sizes and line counts. Limits: nodes, per-file bytes, aggregate source bytes,
header lines, skeleton lines, import declarations. All fields/defaults are documented
on `StructureOptions`; the API also accepts options deserialized from strict JSON.

`changed_only` uses both index and worktree status, including untracked paths, removals,
renames and conflicts. It keeps required ancestor directories. Staged/unstaged counters
are separate: a staged edit reverted in the worktree is still changed. Binary/untracked
line counts are unknown, not zero; hunk counts may independently be unavailable.
A non-Git root or failed status is an error for changed-only mode, never a clean result.

Headers are existing source trivia before imports/code with leading `//!` markers removed,
not generated descriptions. Source ranges and the full-file hash retain the original provenance.
Imports retain original target names and source line ranges; comments and layout trivia
are compacted without interpreting dependencies. Skeletons use existing source-only
language adapters. Unsupported files/adapters have an explicit `unavailable` state.
External Git content filters are disabled with command-local settings: configured code is never
executed. When filters are configured, a warning states that status/counts compare unfiltered
working bytes; conversion-dependent files can differ from normal filtered Git status. Git
queries cover the requested root; tree filters restrict emitted paths and source enrichment.

Files without the selected detail use `absent`; disabled options use `disabled`.
Unregistered source formats are never opened for detail collection. Source hashes
identify the file bytes used for details, not an atomic project snapshot.

The root is canonicalized. Descendant symlinks are listed, never traversed as directories
or opened as sources. `.git` is never traversed. All generated labels and warnings are
English; names and source documentation remain verbatim. This is a local operator
utility: its caller authorizes the root. It creates no session authority/read receipts
and is not automatically attached to activation, a model or the public MCP catalog.

Both text rendering and JSON order the detail fields as `header`, `imports`, `skeleton`.
Collected headers omit leading `//!` markers; skeleton text omits its duplicate file
heading. Source line ranges and hashes still identify the original file.

Skeleton line numbers are left-aligned without leading padding. Member indentation
after the line number is preserved.

The example accepts `--options FILE` to load every `StructureOptions` field from JSON,
including detail and byte budgets. Later command-line switches override loaded values.

Use `--benchmark N` to time N fresh in-process collections plus text rendering and
pretty JSON serialization. Compile with `--release` before measuring. The report
contains per-iteration timings and output digests; compilation, process startup,
output hashing, printing and disk writes are outside the timers. OS caches are not reset.

Text output starts with one source-detail capability note. Unsupported or absent
header/import/skeleton blocks are omitted, and expected unregistered-format cases
produce no per-file warnings. Real read failures, byte/node limits and Git warnings
remain visible. JSON retains explicit unavailable/absent states for machine consumers.

The configured Git filter policy is one informational line before the tree.
Operational Git/read failures and collection limits remain warning lines.

Default connectors are `├` and `└`. `compact: true` (`--compact`) removes all tree
connectors and uses one space per nesting level. Preserve whitespace when transmitting
compact text; directory names retain `/`, and detail labels retain their own nesting.

Skeleton comments remain enabled by default. Set `include_skeleton_comments: false`
(`--no-skeleton-comments`) to omit attached symbol documentation while retaining
signatures, file headers and imports. `--skeleton-comments` enables them again.

With `include_diff`, text output explains once that omitted changes/counters are zero.
Unchanged status and confirmed zero stage/worktree counters are suppressed. Unknown
counts (`?`), unavailable states and changed statuses remain explicit. JSON retains
all collected Git observations. No Git explanation is emitted when diffs are disabled.

Python `.py` and `.pyi` skeletons are parsed in process with pinned Tree-sitter grammar.
They include classes, functions, async methods, nested definitions, annotations,
module/class bindings, type aliases, decorators in source ranges, and attached docstrings.
The bounded CST converter supplies member kinds, literal-preserving signatures and pytest classification;
function-local bindings are omitted. No project imports or interpreter are executed.
The source parser refuses syntax errors, files above 16 MiB, more than 20,000 symbols,
more than 128 traversal levels, an 8 MiB aggregate owned-text budget, a 64 KiB signature
budget, a 20,000-node token frontier or a one-second parse/conversion budget. A refused registered
language outline produces an explicit warning, distinct from a valid empty skeleton.
Pyright still owns interactive semantic operations; server-loading and unverified-resolution
edit fallbacks remain disabled. This source outline does not claim Pyright range parity.

Python source outlines also let existing landing/lexical consumers report proven function
addresses (for example `svc/app.py#broken:1`) instead of a physical-line-only address. This
does not enable server-loading symbol edit fallbacks or replace the project's syntax checker.

Review fixes: bindings in destructured/chained assignments name only their own target;
multiline initializers are not expanded into every binding. Header compaction changes only
CST token gaps, preserving string token bytes. A multiline string token in a signature cannot
satisfy the one-line signature contract and explicitly refuses the outline rather than changing
its value. Long multiline parameter lists remain supported. Decorator ranges use the CST wrapper.
Imports use each owning language's CST/lexer so literal/template/raw-string contents are excluded.
A directory exceeding its entry scan budget is omitted as a whole and marked truncated; no
unspecified read_dir prefix is exposed. Changed gitlinks retain their own directory status.
Global unavailable Git observations are always explicit, including directory-only views.


## Bounded parallel collection

`workers` / `--workers N` selects 1..=16 parser workers (default 4). With one worker
all enrichment runs inline. Directory traversal and source reads remain ordered so
aggregate byte-budget admission is independent of thread scheduling. Reads overlap
parsing through a zero-capacity channel; at most one source per active worker plus
the reader is retained, bounded also by the aggregate read limit. No async runtime,
new dependency, persistent cache or daemon is introduced. Workers own disjoint nodes;
results and warnings are merged in source order and all workers join before return.
Thread startup/worker failure returns an error, never a successful partial tree.
Metadata-only collection does not spawn parser threads. Python imports and skeletons
share one immutable CST per file, with the same bounded per-file cooperative deadline.
