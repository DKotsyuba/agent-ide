//! Cross-language name facts: the fourth language seam (the language bridge).
//!
//! A language states, per file, which names it defines or uses in shared namespaces (a class name,
//! an element id, a style variable). The core joins those facts across languages by
//! `(namespace, domain, name)` without knowing any language: namespaces are core-owned
//! descriptors, extraction is a pure function of one file's text, and the collector
//! ([`FactSink`]) validates and bounds what a provider emits. See
//! `docs/contracts/language-bridge.md`.
//!
//! [`FactSink`]: crate::lang::names::FactSink

use std::{fmt, path::Path};

/// Static description of one namespace; rendering words come from here, never from a `match`.
pub struct NamespaceDescriptor {
    /// Versioned identifier (`class/v1`); a change of normalization rules bumps the version.
    pub id: &'static str,
    /// What a name in this namespace is called in replies (`class name`).
    pub label: &'static str,
    /// What a defining site is called in replies (`rule`, `element`, `declaration`).
    pub define_word: &'static str,
    /// Prefix that addresses a bare name in this namespace (`.btn`, `##main`, `--brand`), if any.
    pub sigil: Option<&'static str>,
    /// Names are worktree-relative file paths (spaces allowed, up to [`MAX_PATH_KEY_BYTES`])
    /// instead of identifiers (no whitespace, up to [`MAX_NAME_BYTES`]).
    pub path_keys: bool,
}

/// A namespace handle: a cheap copyable reference to a core-owned [`NamespaceDescriptor`].
///
/// Equality, ordering and hashing use the descriptor's `id`, so two crates that name the same
/// constant always agree.
#[derive(Clone, Copy)]
pub struct Namespace(&'static NamespaceDescriptor);

impl Namespace {
    /// Builds the handle for a static descriptor.
    pub const fn of(descriptor: &'static NamespaceDescriptor) -> Self {
        Self(descriptor)
    }

    /// Versioned identifier.
    pub const fn id(self) -> &'static str {
        self.0.id
    }

    /// Rendered name of the namespace's names.
    pub const fn label(self) -> &'static str {
        self.0.label
    }

    /// Rendered name of a defining site.
    pub const fn define_word(self) -> &'static str {
        self.0.define_word
    }

    /// Bare-name prefix, if the namespace has one.
    pub const fn sigil(self) -> Option<&'static str> {
        self.0.sigil
    }

    /// Whether names are file paths (see [`NamespaceDescriptor::path_keys`]).
    pub const fn path_keys(self) -> bool {
        self.0.path_keys
    }
}

impl PartialEq for Namespace {
    /// Compares identifiers.
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for Namespace {}

impl std::hash::Hash for Namespace {
    /// Hashes the identifier.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl PartialOrd for Namespace {
    /// Total order by identifier.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Namespace {
    /// Orders by identifier bytes.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.id.cmp(other.0.id)
    }
}

impl fmt::Debug for Namespace {
    /// Writes the identifier.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.id)
    }
}

/// The core's namespaces. A new namespace is one more constant here plus extractors in the
/// language crates; no core logic changes.
pub mod ns {
    use super::{Namespace, NamespaceDescriptor};

    /// Descriptor of [`CLASS`].
    static CLASS_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "class/v1",
        label: "class name",
        define_word: "rule",
        sigil: Some("."),
        path_keys: false,
    };
    /// Descriptor of [`ELEMENT_ID`].
    static ELEMENT_ID_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "id/v1",
        label: "element id",
        define_word: "element",
        sigil: Some("##"),
        path_keys: false,
    };
    /// Descriptor of [`STYLE_VARIABLE`].
    static STYLE_VARIABLE_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "style-variable/v1",
        label: "style variable",
        define_word: "declaration",
        sigil: Some("--"),
        path_keys: false,
    };
    /// Descriptor of [`FILE_REF`].
    static FILE_REF_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "file-ref/v1",
        label: "file reference",
        define_word: "file",
        sigil: None,
        path_keys: true,
    };

    /// Class names: defined by style rule selectors, used by markup and code class attributes.
    pub const CLASS: Namespace = Namespace::of(&CLASS_DESCRIPTOR);
    /// Element ids: defined by markup `id` attributes, used by selectors, fragment links and queries.
    pub const ELEMENT_ID: Namespace = Namespace::of(&ELEMENT_ID_DESCRIPTOR);
    /// Custom style properties: defined by `--x:` declarations, used by `var(--x)`.
    pub const STYLE_VARIABLE: Namespace = Namespace::of(&STYLE_VARIABLE_DESCRIPTOR);
    /// File references: used by `<script src>`, `<link href>`, imports and `require` of a local
    /// file; a name is the worktree-relative path the reference spells (lexically normalized),
    /// and the file itself is the definition, known to the core's file listing rather than to
    /// any extractor.
    pub const FILE_REF: Namespace = Namespace::of(&FILE_REF_DESCRIPTOR);
    /// Every core namespace, in id order.
    pub const ALL: [Namespace; 4] = [CLASS, FILE_REF, ELEMENT_ID, STYLE_VARIABLE];
}

/// The join key: facts meet only when namespace, domain and name are all byte-equal.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NameKey {
    /// Namespace the name lives in.
    pub namespace: Namespace,
    /// Scope inside the worktree; empty means worktree-global. Non-empty scopes (a module path)
    /// must use one canonical spelling across every provider of the namespace.
    pub domain: Box<str>,
    /// Normalized name, without sigils or escapes.
    pub name: Box<str>,
}

impl NameKey {
    /// A worktree-global key.
    pub fn global(namespace: Namespace, name: &str) -> Self {
        Self {
            namespace,
            domain: "".into(),
            name: name.into(),
        }
    }
}

/// Whether a fact introduces the name or refers to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Role {
    /// Introduces the name under the namespace's semantics.
    Define,
    /// Refers to the name; never implies that it resolves at run time.
    Use,
}

/// How sure the extractor is about the occurrence (never about run-time applicability).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Certainty {
    /// The supported syntax contains exactly this complete, decoded name.
    Exact,
    /// Reading it as a name needed the stated bounded assumption (`template literal`).
    Heuristic(&'static str),
}

/// One syntactic statement about one name at one location of one file.
///
/// It carries no language (always the language owning the file) and no line text (read at render
/// time, which doubles as the freshness proof).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct NameFact {
    /// Join key.
    pub key: NameKey,
    /// Define or use.
    pub role: Role,
    /// 1-based line.
    pub line: u32,
    /// 1-based column in bytes.
    pub column: u32,
    /// Exact or heuristic, with the heuristic's reason.
    pub certainty: Certainty,
}

/// What one language states about one namespace: whether it may emit definitions and/or uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamespaceCoverage {
    /// The namespace.
    pub namespace: Namespace,
    /// The language may emit [`Role::Define`] facts in it.
    pub defines: bool,
    /// The language may emit [`Role::Use`] facts in it.
    pub uses: bool,
}

/// Outcome of extracting one file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileVerdict {
    /// The file was read; its facts (possibly none) are in the sink.
    Indexed,
    /// The file was not indexed, for the stated reason (`minified`, `generated`); facts already
    /// pushed are discarded.
    Skipped(&'static str),
}

/// Longest accepted name or domain, in bytes.
pub const MAX_NAME_BYTES: usize = 256;
/// Longest accepted name of a path-keyed namespace, in bytes.
pub const MAX_PATH_KEY_BYTES: usize = 4096;
/// Facts one file may contribute; the sink refuses more and marks itself capped.
pub const MAX_FACTS_PER_FILE: usize = 5_000;

/// Core-owned collector for one file's facts.
///
/// Validates every fact (name of 1..=[`MAX_NAME_BYTES`] bytes without control characters or
/// whitespace — a path-keyed namespace takes 1..=[`MAX_PATH_KEY_BYTES`] bytes with spaces but no
/// control characters and no domain — domain empty or valid by the same rule, 1-based position)
/// and enforces the per-file cap. Invalid facts are dropped and counted.
#[derive(Debug)]
pub struct FactSink {
    /// Accepted facts in push order.
    facts: Vec<NameFact>,
    /// Most facts this sink accepts.
    limit: usize,
    /// A push was refused because the sink was full.
    capped: bool,
    /// Facts dropped by validation.
    rejected: usize,
}

impl Default for FactSink {
    /// A sink with the per-file cap.
    fn default() -> Self {
        Self::with_limit(MAX_FACTS_PER_FILE)
    }
}

impl FactSink {
    /// A sink with the per-file cap.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink accepting at most `limit` facts (never more than the per-file cap).
    pub fn with_limit(limit: usize) -> Self {
        Self {
            facts: Vec::new(),
            limit: limit.min(MAX_FACTS_PER_FILE),
            capped: false,
            rejected: 0,
        }
    }

    /// Offers one fact. Returns `false` once the sink is full (the extractor should stop); an
    /// invalid fact is dropped and counted without ending extraction.
    pub fn push(&mut self, fact: NameFact) -> bool {
        if self.is_full() {
            self.capped = true;
            return false;
        }
        let valid = |text: &str| {
            text.len() <= MAX_NAME_BYTES
                && !text.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        };
        let name_valid = if fact.key.namespace.path_keys() {
            fact.key.name.len() <= MAX_PATH_KEY_BYTES
                && !fact.key.name.chars().any(char::is_control)
                && fact.key.domain.is_empty()
        } else {
            valid(&fact.key.name)
        };
        if fact.key.name.is_empty()
            || !name_valid
            || !valid(&fact.key.domain)
            || fact.line == 0
            || fact.column == 0
        {
            self.rejected += 1;
        } else {
            self.facts.push(fact);
        }
        true
    }

    /// Whether the sink accepts no more facts.
    pub fn is_full(&self) -> bool {
        self.facts.len() >= self.limit
    }

    /// Whether a push was refused because the sink was full (counts are then lower bounds).
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    /// Facts dropped by validation.
    pub fn rejected(&self) -> usize {
        self.rejected
    }

    /// Accepted facts in push order.
    pub fn facts(&self) -> &[NameFact] {
        &self.facts
    }

    /// Takes the accepted facts.
    pub fn into_facts(self) -> Vec<NameFact> {
        self.facts
    }
}

/// The worktree-relative path `reference` spells when written in `from` (a worktree-relative
/// file): joined to `from`'s directory and normalized lexically (`.` dropped, `..` resolved).
/// `None` when the reference is not a plain relative path (empty, absolute, a URL scheme, a
/// network path) or climbs above the worktree root. No file is consulted: the caller decides
/// whether the target exists. Components are joined with `/`.
pub fn relative_reference(from: &Path, reference: &str) -> Option<String> {
    let first = reference.split('/').next().unwrap_or("");
    if reference.is_empty()
        || reference.starts_with(['/', '\\'])
        || reference.contains(['\\', '\0'])
        || first.contains(':')
    {
        return None;
    }
    let mut parts: Vec<&str> = from
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(part) => part.to_str(),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for part in reference.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Static vocabulary with which a language's file references reach a file the reference does not
/// spell exactly (`./button` for `button.tsx`). It is compiled descriptor data, not a language
/// computation: the core applies it to its own file listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileProbe {
    /// Endings appended to a reference that names no file, in preference order (`.ts`).
    pub suffixes: &'static [&'static str],
    /// Endings appended to reach a directory's entry file, in preference order (`/index.ts`).
    pub index_suffixes: &'static [&'static str],
    /// `(written ending, ending on disk)` pairs tried in order (`.js` written for a `.ts` file).
    pub swaps: &'static [(&'static str, &'static str)],
}

impl FileProbe {
    /// A language whose references name their file exactly.
    pub const EXACT: FileProbe = FileProbe {
        suffixes: &[],
        index_suffixes: &[],
        swaps: &[],
    };

    /// Every worktree-relative path `reference` may reach, most certain first and without
    /// duplicates: the reference itself (no reason), then swaps, suffixes and index files, each
    /// with the label of the assumption that reaches it.
    pub fn candidates(&self, reference: &str) -> Vec<(String, Option<&'static str>)> {
        let swapped = self.swaps.iter().filter_map(|(written, disk)| {
            reference
                .strip_suffix(written)
                .map(|stem| (format!("{stem}{disk}"), "extension swap"))
        });
        let suffixed = self
            .suffixes
            .iter()
            .map(|suffix| (format!("{reference}{suffix}"), "extension probe"));
        let indexed = self
            .index_suffixes
            .iter()
            .map(|suffix| (format!("{reference}{suffix}"), "index probe"));
        let mut found: Vec<(String, Option<&'static str>)> = vec![(reference.to_owned(), None)];
        for (candidate, reason) in swapped.chain(suffixed).chain(indexed) {
            if !found.iter().any(|(seen, _)| *seen == candidate) {
                found.push((candidate, Some(reason)));
            }
        }
        found
    }

    /// The file `reference` reaches under `exists` (which answers for one worktree-relative file
    /// path): the first of its [`FileProbe::candidates`] that is a file, with the assumption that
    /// reached it.
    pub fn reach(
        &self,
        reference: &str,
        exists: impl Fn(&str) -> bool,
    ) -> Option<(String, Option<&'static str>)> {
        self.candidates(reference)
            .into_iter()
            .find(|(candidate, _)| exists(candidate))
    }
}

/// One answer of [`NameFacts::resolve`]: a key the raw reference may mean, with how sure the
/// language is. The core checks existence and builds any edge itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    /// Domain of the key (empty: worktree-global).
    pub domain: String,
    /// Normalized key.
    pub name: String,
    /// Exact, or the labelled assumption.
    pub certainty: Certainty,
}

/// Most candidates one [`NameFacts::resolve`] returns.
pub const MAX_RESOLUTIONS: usize = 32;

/// The default [`NameFacts::resolve`]: `raw` (a URL or specifier, query and fragment dropped)
/// joined to `from`'s directory and expanded by `probe`; nothing outside `file-ref/v1`, for a URL
/// or for a path leaving the worktree.
pub fn resolve_file_ref(
    probe: &FileProbe,
    namespace: Namespace,
    raw: &str,
    from: &Path,
    limit: usize,
) -> Vec<Resolution> {
    if namespace != ns::FILE_REF {
        return Vec::new();
    }
    let path = raw.split(['?', '#']).next().unwrap_or("");
    relative_reference(from, path)
        .map(|key| {
            probe
                .candidates(&key)
                .into_iter()
                .take(limit.min(MAX_RESOLUTIONS))
                .map(|(name, reason)| Resolution {
                    domain: String::new(),
                    name,
                    certainty: reason.map_or(Certainty::Exact, Certainty::Heuristic),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cross-language name facts of one language. Stateless, synchronous and pure over its inputs.
pub trait NameFacts: Send + Sync {
    /// The namespaces this language may define and/or use names in.
    fn coverage(&self) -> &'static [NamespaceCoverage];

    /// Revision of the extraction rules; part of the key of cached facts, so facts cached by an
    /// earlier extractor never survive a change. Bump it whenever [`NameFacts::extract`] would
    /// answer differently for some input.
    fn revision(&self) -> &'static str {
        "1"
    }

    /// How this language's `file-ref/v1` references reach files they do not spell exactly;
    /// exact-only by default.
    fn file_probe(&self) -> &'static FileProbe {
        &FileProbe::EXACT
    }

    /// The keys the reference `raw` written in `from` may mean in `namespace`, most certain first
    /// and at most `limit` (never more than [`MAX_RESOLUTIONS`]); nothing for a reference the
    /// language cannot interpret. The default interprets `file-ref/v1` lexically through
    /// [`NameFacts::file_probe`]; no file is consulted and no edge is made here.
    fn resolve(
        &self,
        namespace: Namespace,
        raw: &str,
        from: &Path,
        limit: usize,
    ) -> Vec<Resolution> {
        resolve_file_ref(self.file_probe(), namespace, raw, from, limit)
    }

    /// Facts of one file. `file` is worktree-relative (for resolving relative references);
    /// `source` is its complete UTF-8 text (the core skips non-UTF-8 files). No I/O, no language
    /// server, no subprocess.
    fn extract(&self, file: &Path, source: &str, sink: &mut FactSink) -> FileVerdict;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Namespaces compare by identifier; the core set has distinct ids and sigils.
    #[test]
    fn namespaces_compare_by_id() {
        assert_eq!(ns::CLASS, ns::CLASS);
        assert_ne!(ns::CLASS, ns::ELEMENT_ID);
        assert!(ns::CLASS < ns::FILE_REF && ns::FILE_REF < ns::STYLE_VARIABLE);
        assert_eq!(ns::ELEMENT_ID.sigil(), Some("##"));
        assert!(ns::FILE_REF.path_keys() && !ns::CLASS.path_keys());
        assert_eq!(format!("{:?}", ns::STYLE_VARIABLE), "style-variable/v1");
    }

    /// References join to the referencing file's directory, normalize lexically and never leave
    /// the worktree or name a URL, absolute path or empty target.
    #[test]
    fn relative_references_normalize_inside_the_worktree() {
        let from = Path::new("src/ui/App.tsx");
        let key = |reference| relative_reference(from, reference);
        assert_eq!(key("./Button"), Some("src/ui/Button".into()));
        assert_eq!(key("../lib/util.js"), Some("src/lib/util.js".into()));
        assert_eq!(key("a/./b/../c.css"), Some("src/ui/a/c.css".into()));
        assert_eq!(key("../../x.js"), Some("x.js".into()));
        assert_eq!(key("../../../x.js"), None);
        assert_eq!(key("/abs/x.js"), None);
        assert_eq!(key("https://cdn/x.js"), None);
        assert_eq!(key("data:text/javascript,1"), None);
        assert_eq!(key("//cdn/x.js"), None);
        assert_eq!(key("a\\b.js"), None);
        assert_eq!(key(""), None);
        assert_eq!(key("."), Some("src/ui".into()));
        assert_eq!(
            relative_reference(Path::new("index.html"), "app.js"),
            Some("app.js".into())
        );
        assert_eq!(key("my file.css"), Some("src/ui/my file.css".into()));
    }

    /// An exact file wins; otherwise swaps, suffixes and index files are tried in that order and
    /// the reason of the assumption comes back; a probe never reaches a directory or a miss.
    #[test]
    fn a_file_probe_reaches_files_in_order() {
        const TS: FileProbe = FileProbe {
            suffixes: &[".ts", ".js"],
            index_suffixes: &["/index.ts"],
            swaps: &[(".js", ".ts")],
        };
        let files = ["a.ts", "b.js", "b.ts", "c/index.ts", "d.js"];
        let reach = |reference| TS.reach(reference, |path| files.contains(&path));
        assert_eq!(reach("b.js"), Some(("b.js".into(), None)));
        assert_eq!(reach("a"), Some(("a.ts".into(), Some("extension probe"))));
        assert_eq!(reach("c"), Some(("c/index.ts".into(), Some("index probe"))));
        assert_eq!(reach("a.js"), Some(("a.ts".into(), Some("extension swap"))));
        assert_eq!(reach("d"), Some(("d.js".into(), Some("extension probe"))));
        assert_eq!(reach("e"), None);
        assert_eq!(FileProbe::EXACT.reach("a", |path| path == "a.ts"), None);
        let names: Vec<String> = TS
            .candidates("a")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a", "a.ts", "a.js", "a/index.ts"]);
    }

    /// The default `resolve` turns a written reference into probe candidates inside the worktree
    /// and nothing for other namespaces, escapes or URLs.
    #[test]
    fn resolve_enumerates_probe_candidates() {
        struct Probing;
        impl NameFacts for Probing {
            fn coverage(&self) -> &'static [NamespaceCoverage] {
                &[]
            }
            fn extract(&self, _: &Path, _: &str, _: &mut FactSink) -> FileVerdict {
                FileVerdict::Indexed
            }
            fn file_probe(&self) -> &'static FileProbe {
                &FileProbe {
                    suffixes: &[".ts"],
                    index_suffixes: &[],
                    swaps: &[],
                }
            }
        }
        let from = Path::new("src/a.tsx");
        let rows: Vec<(String, Certainty)> = Probing
            .resolve(ns::FILE_REF, "./b?x#y", from, 32)
            .into_iter()
            .map(|r| (r.name, r.certainty))
            .collect();
        assert_eq!(
            rows,
            [
                ("src/b".to_owned(), Certainty::Exact),
                (
                    "src/b.ts".to_owned(),
                    Certainty::Heuristic("extension probe")
                ),
            ]
        );
        assert_eq!(Probing.resolve(ns::FILE_REF, "./b", from, 1).len(), 1);
        assert!(Probing.resolve(ns::CLASS, "./b", from, 32).is_empty());
        assert!(
            Probing
                .resolve(ns::FILE_REF, "../../b", from, 32)
                .is_empty()
        );
        // A bare path is relative by default (an HTML URL); script languages override this.
        assert_eq!(Probing.resolve(ns::FILE_REF, "pkg", from, 32).len(), 2);
        assert!(
            Probing
                .resolve(ns::FILE_REF, "https://x/y", from, 32)
                .is_empty()
        );
    }

    /// A fact of `name` in `namespace` at line 1, column 1.
    fn fact(namespace: Namespace, domain: &str, name: &str) -> NameFact {
        NameFact {
            key: NameKey {
                namespace,
                domain: domain.into(),
                name: name.into(),
            },
            role: Role::Use,
            line: 1,
            column: 1,
            certainty: Certainty::Exact,
        }
    }

    /// Identifier namespaces refuse whitespace; a path-keyed namespace takes spaces and long
    /// paths but neither control characters, an empty path, a domain nor more than 4 KiB.
    #[test]
    fn the_sink_validates_per_namespace() {
        let mut sink = FactSink::new();
        assert!(sink.push(fact(ns::CLASS, "", "a b")));
        assert!(sink.push(fact(ns::FILE_REF, "", "src/my file.ts")));
        assert!(sink.push(fact(ns::FILE_REF, "", &"d/".repeat(1000))));
        assert!(sink.push(fact(ns::FILE_REF, "", "a\u{7}b")));
        assert!(sink.push(fact(ns::FILE_REF, "dom", "a.ts")));
        assert!(sink.push(fact(ns::FILE_REF, "", &"d".repeat(MAX_PATH_KEY_BYTES + 1))));
        assert!(sink.push(fact(ns::CLASS, "", &"c".repeat(MAX_NAME_BYTES + 1))));
        assert_eq!(sink.rejected(), 5);
        let names: Vec<&str> = sink.facts().iter().map(|fact| &*fact.key.name).collect();
        assert_eq!(names.len(), 2);
        assert_eq!(names[1].len(), 2000);
    }
}
