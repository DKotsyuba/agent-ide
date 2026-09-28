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
    };
    /// Descriptor of [`ELEMENT_ID`].
    static ELEMENT_ID_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "id/v1",
        label: "element id",
        define_word: "element",
        sigil: Some("##"),
    };
    /// Descriptor of [`STYLE_VARIABLE`].
    static STYLE_VARIABLE_DESCRIPTOR: NamespaceDescriptor = NamespaceDescriptor {
        id: "style-variable/v1",
        label: "style variable",
        define_word: "declaration",
        sigil: Some("--"),
    };

    /// Class names: defined by style rule selectors, used by markup and code class attributes.
    pub const CLASS: Namespace = Namespace::of(&CLASS_DESCRIPTOR);
    /// Element ids: defined by markup `id` attributes, used by selectors, fragment links and queries.
    pub const ELEMENT_ID: Namespace = Namespace::of(&ELEMENT_ID_DESCRIPTOR);
    /// Custom style properties: defined by `--x:` declarations, used by `var(--x)`.
    pub const STYLE_VARIABLE: Namespace = Namespace::of(&STYLE_VARIABLE_DESCRIPTOR);
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
/// Facts one file may contribute; the sink refuses more and marks itself capped.
pub const MAX_FACTS_PER_FILE: usize = 5_000;

/// Core-owned collector for one file's facts.
///
/// Validates every fact (name of 1..=[`MAX_NAME_BYTES`] bytes without control characters or
/// whitespace, domain empty or valid by the same rule, 1-based position) and enforces the per-file
/// cap. Invalid facts are dropped and counted.
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
        if fact.key.name.is_empty()
            || !valid(&fact.key.name)
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

/// Cross-language name facts of one language. Stateless, synchronous and pure over its inputs.
pub trait NameFacts: Send + Sync {
    /// The namespaces this language may define and/or use names in.
    fn coverage(&self) -> &'static [NamespaceCoverage];

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
        assert!(ns::CLASS < ns::STYLE_VARIABLE);
        assert_eq!(ns::ELEMENT_ID.sigil(), Some("##"));
        assert_eq!(format!("{:?}", ns::STYLE_VARIABLE), "style-variable/v1");
    }
}
