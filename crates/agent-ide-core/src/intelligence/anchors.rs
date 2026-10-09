//! Where the name index gets a file's facts when a language computes in its module: the module
//! answers `linkage/0` anchors, the core turns them back into the facts the index joins.
//!
//! The index never learns where a language runs. [`AnchorSource`] is the one seam: the daemon
//! installs a source that routes a language to its module (blocking the calling thread, which is
//! always a blocking-pool thread), and every language it does not route is extracted in process
//! through [`NameFacts`](crate::lang::names::NameFacts) exactly as before. An in-process
//! extraction and the same extraction served by a module and converted by [`facts_from_anchors`]
//! are equal fact for fact; that equality is what keeps a mixed setting (one language in its
//! module, another in process) joining with identical answers.

use std::{
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

use crate::{
    lang::{
        Language,
        names::{Certainty, NameFact, NameKey, Role, ns},
    },
    modules::{
        contract::ModuleUnavailable,
        payload::{AnchorBatch, AnchorCertainty, AnchorRole, FileVerdict},
    },
};

/// A module-side source of linkage anchors, installed once by the daemon.
pub trait AnchorSource: Send + Sync {
    /// Whether `language` computes its anchors in its module.
    fn routes(&self, language: Language) -> bool;

    /// The anchors of `text` as `path` in `worktree` from `language`'s module, validated against
    /// the module's declaration. Blocks the calling thread until the module answers or fails;
    /// callers run on blocking-pool threads. `Err` is the module's typed fault (unavailable,
    /// malformed, refused): the index and the symbol cards disclose it, they never substitute an
    /// in-process answer.
    fn anchors(
        &self,
        worktree: &Path,
        language: Language,
        path: &Path,
        text: &str,
    ) -> Result<AnchorBatch, ModuleUnavailable>;
}

/// The source tests install; the daemon uses [`Hosted`].
static SOURCE: OnceLock<Arc<dyn AnchorSource>> = OnceLock::new();

/// Installs an anchor source instead of the module host's (tests); later calls keep the first.
pub fn install(source: Arc<dyn AnchorSource>) {
    let _ = SOURCE.set(source);
}

/// The daemon's own source: the module host's routing ([`calls`](crate::modules::calls)).
struct Hosted;

impl AnchorSource for Hosted {
    fn routes(&self, language: Language) -> bool {
        crate::modules::calls::mode(language) == crate::modules::mode::Mode::Module
    }

    fn anchors(
        &self,
        worktree: &Path,
        language: Language,
        path: &Path,
        text: &str,
    ) -> Result<AnchorBatch, ModuleUnavailable> {
        let (worktree, path, text) = (worktree.to_path_buf(), path.to_path_buf(), text.to_owned());
        let answer =
            async move { crate::modules::calls::anchors(language, &worktree, &path, &text).await };
        // Callers run on blocking-pool threads, where waiting on the runtime is allowed.
        let handle = tokio::runtime::Handle::try_current().map_err(|_| unavailable(language))?;
        handle
            .block_on(answer)?
            .ok_or_else(|| unavailable(language))
    }
}

/// A module fault for `language` where the host gave no answer.
fn unavailable(language: Language) -> ModuleUnavailable {
    ModuleUnavailable {
        module_id: crate::modules::contract::ModuleId::bundled(language.name()),
        module_version: env!("CARGO_PKG_VERSION").to_owned(),
        role: crate::modules::contract::Role::Analyzer,
        stage: crate::modules::contract::Stage::Request,
        cause: crate::modules::contract::Cause::Exited,
        instance: None,
        retry_after_ms: None,
    }
}

/// The source in use: the one installed (tests), else the module host's.
pub fn installed() -> Option<Arc<dyn AnchorSource>> {
    SOURCE
        .get()
        .cloned()
        .or_else(|| Some(Arc::new(Hosted) as Arc<dyn AnchorSource>))
}

/// The source in use when it routes `language` to its module.
pub fn routed(language: Language) -> Option<Arc<dyn AnchorSource>> {
    installed().filter(|source| source.routes(language))
}

/// Most distinct reason labels kept; later ones share a generic label.
const MAX_REASONS: usize = 64;

/// A `'static` copy of a heuristic's reason label. Labels come from a few fixed extractor
/// phrases, so the set stays tiny; a module that sends more than [`MAX_REASONS`] distinct ones
/// gets the generic `heuristic` for the rest.
fn intern(reason: &str) -> &'static str {
    static KNOWN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = known.iter().find(|known| **known == reason) {
        return found;
    }
    if known.len() >= MAX_REASONS {
        return "heuristic";
    }
    let leaked: &'static str = Box::leak(reason.to_owned().into_boxed_str());
    known.push(leaked);
    leaked
}

/// What a module's anchors of one file say, in the index's own terms.
#[derive(Debug, Eq, PartialEq)]
pub struct FromAnchors {
    /// Facts in anchor order; empty for a skipped file.
    pub facts: Vec<NameFact>,
    /// Why the file has no facts, if it was skipped.
    pub skipped: Option<&'static str>,
    /// The per-file limit cut the list.
    pub capped: bool,
}

/// The facts `batch` carries about `text`: namespaces by compiled id (an unknown one is dropped),
/// a heuristic keeps its label, an unverified anchor becomes the `unverified` heuristic, and a
/// byte location becomes the 1-based line and byte column the index stores. An anchor outside
/// `text` is dropped, as is any fact the sink rules refuse.
pub fn facts_from_anchors(text: &str, batch: &AnchorBatch) -> FromAnchors {
    if let FileVerdict::Skipped(reason) = &batch.verdict {
        return FromAnchors {
            facts: Vec::new(),
            skipped: Some(intern(reason)),
            capped: false,
        };
    }
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let mut sink = crate::lang::names::FactSink::new();
    for anchor in &batch.anchors {
        let Some(namespace) = ns::ALL
            .into_iter()
            .find(|namespace| namespace.id() == anchor.namespace)
        else {
            continue;
        };
        let at = anchor.location.start_byte as usize;
        if at > text.len() {
            continue;
        }
        let line = line_starts.partition_point(|start| *start <= at);
        let fact = NameFact {
            key: NameKey {
                namespace,
                domain: anchor.domain.as_str().into(),
                name: anchor.normalized_key.as_str().into(),
            },
            role: match anchor.role {
                AnchorRole::Definition => Role::Define,
                AnchorRole::Use => Role::Use,
            },
            line: line as u32,
            column: (at - line_starts[line - 1] + 1) as u32,
            certainty: match anchor.certainty {
                AnchorCertainty::Exact => Certainty::Exact,
                AnchorCertainty::Heuristic => {
                    Certainty::Heuristic(anchor.reason.as_deref().map_or("heuristic", intern))
                }
                AnchorCertainty::Unverified => Certainty::Heuristic("unverified"),
            },
        };
        if !sink.push(fact) {
            break;
        }
    }
    FromAnchors {
        capped: batch.capped || sink.is_capped(),
        facts: sink.into_facts(),
        skipped: None,
    }
}

/// A module stand-in for tests: serves anchors through the real [`SupportServer`] adapter for
/// the languages it routes, and fails every call while `down` is set.
#[cfg(test)]
pub(crate) mod stub {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::modules::{
        adapter::SupportServer,
        contract::Capability,
        payload::{
            AnalyzeSource, Field, SourceAnalysis, SourceField, SourceRef, SourceText, decode,
            encode,
        },
        serve::Incoming,
    };

    /// See the module docs.
    pub(crate) struct ModuleStub {
        /// Languages computed in the stand-in module.
        pub(crate) routed: Vec<Language>,
        /// Every call fails while set.
        pub(crate) down: AtomicBool,
    }

    impl ModuleStub {
        /// A stand-in routing `routed`.
        pub(crate) fn new(routed: &[Language]) -> Self {
            Self {
                routed: routed.to_vec(),
                down: AtomicBool::new(false),
            }
        }
    }

    /// The anchors the module-side adapter serves for `text` as `path` in `language`.
    pub(crate) fn served(language: Language, path: &Path, text: &str) -> AnchorBatch {
        let incoming = Incoming {
            fence: Default::default(),
            capability: Capability::AnalyzeSource,
            budget_ms: 1000,
            payload: encode(&AnalyzeSource {
                source: SourceRef {
                    path: path.into(),
                    revision: "r".into(),
                    text: SourceText::Inline(text.into()),
                },
                fields: vec![SourceField::Anchors],
            }),
            attachments: Vec::new(),
        };
        let value = SupportServer::new(language, "1").answer(&incoming).unwrap();
        match decode::<SourceAnalysis>(value).unwrap().anchors {
            Field::Available(batch) => batch,
            other => panic!("anchors: {other:?}"),
        }
    }

    impl AnchorSource for ModuleStub {
        fn routes(&self, language: Language) -> bool {
            self.routed.contains(&language)
        }

        fn anchors(
            &self,
            _worktree: &Path,
            language: Language,
            path: &Path,
            text: &str,
        ) -> Result<AnchorBatch, ModuleUnavailable> {
            if self.down.load(Ordering::SeqCst) {
                return Err(ModuleUnavailable {
                    module_id: crate::modules::contract::ModuleId::bundled(language.name()),
                    module_version: "test".into(),
                    role: crate::modules::contract::Role::Analyzer,
                    stage: crate::modules::contract::Stage::Request,
                    cause: crate::modules::contract::Cause::Exited,
                    instance: None,
                    retry_after_ms: None,
                });
            }
            Ok(served(language, path, text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{stub::served, *};
    use crate::{
        lang::{
            names::FactSink,
            testing::{self, ALPHA, BETA, GAMMA},
        },
        modules::payload::{SourceRef, SourceText},
    };

    /// What the language's own extraction gives for `text`.
    fn direct(language: Language, path: &str, text: &str) -> Vec<NameFact> {
        let mut sink = FactSink::new();
        language
            .names()
            .unwrap()
            .extract(Path::new(path), text, &mut sink);
        sink.into_facts()
    }

    /// A module's anchors and the language's in-process extraction are the same facts: namespaces,
    /// domains, roles, certainty with its label, lines and byte columns (multibyte text, file
    /// references with spaces), so a mixed setting joins identically.
    #[test]
    fn served_anchors_equal_the_in_process_facts() {
        testing::install();
        let cases = [
            (ALPHA, "a.alpha", "@btn @card/mod #main\n  @é #x\nplain\n"),
            (
                BETA,
                "b.beta",
                "use:btn é ~card ref:src/my file ref:a/b\nuse:x/dom\n",
            ),
            (GAMMA, "c.gamma", "#main\n\n   #other\n"),
        ];
        for (language, path, text) in cases {
            let batch = served(language, Path::new(path), text);
            batch
                .validate(
                    &SourceRef {
                        path: path.into(),
                        revision: "r".into(),
                        text: SourceText::Inline(text.into()),
                    },
                    text,
                    &batch.coverage,
                )
                .unwrap();
            let converted = facts_from_anchors(text, &batch);
            assert_eq!(converted.facts, direct(language, path, text), "{path}");
            assert_eq!((converted.skipped, converted.capped), (None, false));
        }
    }

    /// A skipped file carries no facts and keeps its reason; an unverified anchor is the
    /// `unverified` heuristic; unknown namespaces and out-of-text locations are dropped.
    #[test]
    fn conversion_keeps_skips_and_drops_what_cannot_be_trusted() {
        testing::install();
        let text = "#!skip\n@x\n";
        let skipped = served(ALPHA, Path::new("a.alpha"), text);
        let converted = facts_from_anchors(text, &skipped);
        assert_eq!(
            (converted.facts.len(), converted.skipped),
            (0, Some("generated"))
        );

        let mut batch = served(ALPHA, Path::new("a.alpha"), "@x\n");
        let mut unknown = batch.anchors[0].clone();
        unknown.namespace = "invented/v1".into();
        let mut outside = batch.anchors[0].clone();
        outside.location.start_byte = 99;
        batch.anchors[0].certainty = AnchorCertainty::Unverified;
        batch.anchors.extend([unknown, outside]);
        let converted = facts_from_anchors("@x\n", &batch);
        assert_eq!(converted.facts.len(), 1);
        assert_eq!(
            converted.facts[0].certainty,
            Certainty::Heuristic("unverified")
        );
    }
}
