//! Module-side adapter that serves a registered language's own in-process implementation over
//! `bundled-module/0`: the same crates answer through two transports, which is what makes the
//! parity transcript meaningful.
//!
//! [`SupportServer`] answers every capability its language's [`LanguageSupport`],
//! [`NameFacts`] and project checks implement; provider-backed capabilities (normalized
//! outline, semantic, calls, rename) are declared unsupported here and added by a language's own
//! server, which wraps this one. Commands the core runs (formatter, syntax probe) travel as the
//! `argv` recipe: present semantics, a spawn owner that becomes Execution.
//!
//! [`LanguageSupport`]: crate::lang::LanguageSupport
//! [`NameFacts`]: crate::lang::names::NameFacts

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    contract::{Capability, CapabilityDecl, Declaration, ErrorCode, ModuleId, Support},
    payload::{
        AnalysisScopeRequest, AnalyzeSource, Anchor, AnchorBatch, AnchorCertainty, AnchorRole,
        EffectRequest, Field, FileDocRequest, FileVerdict, FormatPlanRequest, InsertSiteRequest,
        LinkageCoverage, LinkageQuery, Location, MAX_RESOLVE_CANDIDATES, Param, ProjectQuery,
        ResolveAnswer, ResolveCandidate, SourceAnalysis, SourceField, SourceRef, SourceText,
        SyntaxQuery, TestFacts, TestParseRequest, TestPlanQuery, TestRun, decode, encode,
    },
    serve::{Answer, Effects, Incoming, ModuleServer, ServeError},
};
use crate::lang::{
    Language,
    environment::replace_selections,
    names::{Certainty, FactSink, Role},
};

/// Recipe id of a command the core runs exactly as the language computed it today.
pub const ARGV_RECIPE: &str = "argv";

/// `argv` as an [`ARGV_RECIPE`] request: one `argv.NNNN` token per argument, program first.
pub fn argv_effect(argv: &[String]) -> EffectRequest {
    EffectRequest {
        recipe: ARGV_RECIPE.to_owned(),
        params: argv
            .iter()
            .enumerate()
            .map(|(index, token)| (format!("argv.{index:04}"), Param::Token(token.clone())))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// The argument vector of an [`ARGV_RECIPE`] request, or `None` when it is another recipe or
/// malformed (a gap, a non-token parameter, an empty vector).
pub fn effect_argv(effect: &EffectRequest) -> Option<Vec<String>> {
    if effect.recipe != ARGV_RECIPE || effect.params.is_empty() || effect.params.len() > 256 {
        return None;
    }
    effect
        .params
        .iter()
        .enumerate()
        .map(|(index, (key, param))| match param {
            Param::Token(token) if *key == format!("argv.{index:04}") => Some(token.clone()),
            _ => None,
        })
        .collect()
}

/// Serves `language`'s own support, name facts and checks inside its module process.
pub struct SupportServer {
    /// The served language.
    language: Language,
    /// The module's package version.
    version: String,
    /// Turns the language's formatter or syntax-probe argument vector into a request of one of
    /// its declared recipes; without it (or when it answers `None`) the plan is the generic
    /// `argv` request, which the core refuses to run.
    plans: Option<EffectPlans>,
}

/// Turns a formatter or probe argument vector into a request of the language's recipes.
pub type EffectPlans = fn(&[String]) -> Option<EffectRequest>;

impl SupportServer {
    /// The adapter for `language` at `version`.
    pub fn new(language: Language, version: &str) -> Self {
        Self {
            language,
            version: version.to_owned(),
            plans: None,
        }
    }

    /// The adapter whose formatter, syntax-probe and test-run plans become requests of the
    /// language's declared recipes through `plans`.
    pub fn with_effect_plans(mut self, plans: EffectPlans) -> Self {
        self.plans = Some(plans);
        self
    }

    /// The effect request of one formatter, probe or test-run argument vector; the generic `argv`
    /// request (which the core refuses) when `plans` has none.
    fn plan(&self, argv: &[String]) -> EffectRequest {
        self.plans
            .and_then(|plans| plans(argv))
            .unwrap_or_else(|| argv_effect(argv))
    }

    /// The served language.
    pub fn language(&self) -> Language {
        self.language
    }

    /// The text of `source`, inline or from `request`'s attachment.
    fn text(source: &SourceRef, request: &Incoming) -> Result<String, String> {
        match &source.text {
            SourceText::Inline(text) => Ok(text.clone()),
            SourceText::Missing => Ok(String::new()),
            SourceText::Attachment(id) => {
                let attachment = request
                    .attachment(*id)
                    .ok_or_else(|| format!("attachment {id} missing"))?;
                String::from_utf8(attachment.bytes.clone())
                    .map_err(|_| "source is not UTF-8".into())
            }
        }
    }

    /// The linkage coverage the language declares.
    fn coverage(&self) -> Vec<LinkageCoverage> {
        self.language
            .names()
            .map(|names| {
                names
                    .coverage()
                    .iter()
                    .map(|coverage| LinkageCoverage {
                        namespace: coverage.namespace.id().to_owned(),
                        defines: coverage.defines,
                        uses: coverage.uses,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The anchors of `text`, translated from the language's name facts without changing their
    /// namespaces, domains or certainty; a fact's 1-based line and byte column become a point
    /// location at that byte.
    fn anchors(&self, source: &SourceRef, text: &str) -> Option<AnchorBatch> {
        let names = self.language.names()?;
        let mut sink = FactSink::new();
        let verdict = names.extract(&source.path, text, &mut sink);
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        let capped = sink.is_capped();
        let rejected = sink.rejected() as u32;
        let anchors = sink
            .into_facts()
            .into_iter()
            .filter_map(|fact| {
                let start = line_starts.get(fact.line as usize - 1)? + fact.column as usize - 1;
                (start <= text.len()).then(|| Anchor {
                    namespace: fact.key.namespace.id().to_owned(),
                    domain: fact.key.domain.to_string(),
                    normalized_key: fact.key.name.to_string(),
                    role: match fact.role {
                        Role::Define => AnchorRole::Definition,
                        Role::Use => AnchorRole::Use,
                    },
                    location: Location {
                        path: source.path.clone(),
                        start_byte: start as u64,
                        end_byte: start as u64,
                        revision: Some(source.revision.clone()),
                    },
                    certainty: match fact.certainty {
                        Certainty::Exact => AnchorCertainty::Exact,
                        Certainty::Heuristic(_) => AnchorCertainty::Heuristic,
                    },
                    reason: match fact.certainty {
                        Certainty::Exact => None,
                        Certainty::Heuristic(reason) => Some(reason.to_owned()),
                    },
                })
            })
            .collect();
        Some(AnchorBatch {
            verdict: match verdict {
                crate::lang::names::FileVerdict::Indexed => FileVerdict::Indexed,
                crate::lang::names::FileVerdict::Skipped(reason) => {
                    FileVerdict::Skipped(reason.to_owned())
                }
            },
            capped,
            rejected,
            coverage: self.coverage(),
            anchors,
        })
    }

    /// The answer of one decoded request of a supported capability.
    pub fn answer(&self, request: &Incoming) -> Result<Value, String> {
        let support = self.language.support();
        let payload = request.payload.clone();
        Ok(match request.capability {
            Capability::Project => match decode::<ProjectQuery>(payload)? {
                ProjectQuery::Detect { root } => encode(&support.detect(&root)),
                ProjectQuery::Environments {
                    worktree,
                    selections,
                } => {
                    replace_selections(&worktree, self.language, selections);
                    encode(&support.environments(&worktree))
                }
                ProjectQuery::CheckSelection {
                    worktree,
                    root,
                    selector,
                } => encode(&support.check_selection(&worktree, &root, &selector)),
                ProjectQuery::CommandEnv {
                    worktree,
                    cwd,
                    program,
                    selections,
                } => {
                    replace_selections(&worktree, self.language, selections);
                    encode(&support.command_env(&worktree, &cwd, &program))
                }
                ProjectQuery::TestToolchain { program } => {
                    encode(&support.test_toolchain(&program))
                }
            },
            Capability::AnalyzeSource => {
                let query: AnalyzeSource = decode(payload)?;
                let text = Self::text(&query.source, request)?;
                let path = query.source.path.as_path();
                let wants = |field| query.fields.contains(&field);
                encode(&SourceAnalysis {
                    outline: pick(wants(SourceField::Outline), || {
                        Field::Available(support.outline_from_source(path, &text))
                    }),
                    file_doc: pick(wants(SourceField::FileDoc), || {
                        Field::Available(support.file_doc(&text))
                    }),
                    syntax: pick(wants(SourceField::Syntax), || {
                        Field::Available(support.syntax_verdict(path, &text))
                    }),
                    tests: pick(wants(SourceField::Tests), || {
                        Field::Available(TestFacts {
                            is_test_file: support.is_test_file(path),
                            test_binary: support.test_binary(path),
                        })
                    }),
                    anchors: pick(wants(SourceField::Anchors), || {
                        self.anchors(&query.source, &text)
                            .map_or(Field::Unsupported, Field::Available)
                    }),
                })
            }
            Capability::FileDoc => {
                let query: FileDocRequest = decode(payload)?;
                encode(&support.file_doc(&Self::text(&query.source, request)?))
            }
            Capability::InsertSite => {
                let query: InsertSiteRequest = decode(payload)?;
                let text = Self::text(&query.source, request)?;
                encode(&support.insert_site(&text, &query.outline, &query.anchor, query.placement))
            }
            Capability::Syntax => match decode::<SyntaxQuery>(payload)? {
                SyntaxQuery::Verdict { source } => {
                    let text = Self::text(&source, request)?;
                    encode(&support.syntax_verdict(&source.path, &text))
                }
                SyntaxQuery::ProbePlan {
                    project,
                    root,
                    file,
                    configured,
                } => encode(
                    &support
                        .syntax_probe_command(&project, &root, &file, configured.as_ref())
                        .map(|argv| self.plan(&argv)),
                ),
            },
            Capability::FormatPlan => {
                let query: FormatPlanRequest = decode(payload)?;
                encode(
                    &support
                        .format_stdin_command(&query.project, &query.file)
                        .map(|argv| self.plan(&argv)),
                )
            }
            Capability::TestPlan => match decode::<TestPlanQuery>(payload)? {
                TestPlanQuery::Selection { project, target } => {
                    encode(&support.test_selection(&project, &target))
                }
                TestPlanQuery::Run { project, target } => encode(
                    &support
                        .test_selection(&project, &target)
                        .map(|selection| TestRun {
                            effect: self.plan(&selection.command),
                            tests: selection.tests,
                        }),
                ),
                TestPlanQuery::TestIds {
                    file,
                    outline_paths,
                } => encode(
                    &outline_paths
                        .iter()
                        .map(|outline_path| support.test_id(&file, outline_path))
                        .collect::<Vec<_>>(),
                ),
            },
            Capability::TestParse => {
                let query: TestParseRequest = decode(payload)?;
                let stream = |id| {
                    request
                        .attachment(id)
                        .map(|attachment| String::from_utf8_lossy(&attachment.bytes).into_owned())
                        .ok_or_else(|| format!("attachment {id} missing"))
                };
                encode(&support.parse_test_output(&stream(query.stdout)?, &stream(query.stderr)?))
            }
            Capability::AnalysisScope => {
                let query: AnalysisScopeRequest = decode(payload)?;
                let checks = self.language.checks().ok_or("no project checks")?;
                encode(&checks.not_analysed(&query.worktree, &query.path))
            }
            Capability::Linkage => match decode::<LinkageQuery>(payload)? {
                LinkageQuery::Anchors { source } => {
                    let text = Self::text(&source, request)?;
                    encode(&self.anchors(&source, &text).ok_or("no linkage")?)
                }
                LinkageQuery::Resolve(query) => {
                    let names = self.language.names().ok_or("no linkage")?;
                    let namespace = crate::lang::names::ns::ALL
                        .into_iter()
                        .find(|namespace| namespace.id() == query.namespace)
                        .ok_or_else(|| format!("unknown namespace {}", query.namespace))?;
                    let limit = (query.candidate_limit as usize).min(MAX_RESOLVE_CANDIDATES);
                    // One more than the limit tells whether the limit cut the list.
                    // ponytail: at the 32-candidate maximum the language itself stops, so a
                    // longer list is not reported capped.
                    let mut candidates =
                        names.resolve(namespace, &query.raw_key, &query.from_path, limit + 1);
                    let capped = candidates.len() > limit;
                    candidates.truncate(limit);
                    encode(&ResolveAnswer {
                        capped,
                        candidates: candidates
                            .into_iter()
                            .map(|resolution| ResolveCandidate {
                                domain: resolution.domain,
                                normalized_key: resolution.name,
                                certainty: match resolution.certainty {
                                    Certainty::Exact => AnchorCertainty::Exact,
                                    Certainty::Heuristic(_) => AnchorCertainty::Heuristic,
                                },
                                evidence: match resolution.certainty {
                                    Certainty::Exact => "exact".to_owned(),
                                    Certainty::Heuristic(reason) => reason.to_owned(),
                                },
                            })
                            .collect(),
                    })
                }
            },
            other => return Err(format!("{other:?} is not served by the support adapter")),
        })
    }
}

/// `value()` when the field was requested, else [`Field::NotRequested`].
fn pick<T>(wanted: bool, value: impl FnOnce() -> Field<T>) -> Field<T> {
    if wanted { value() } else { Field::NotRequested }
}

impl ModuleServer for SupportServer {
    /// Supported: what the language's in-process implementation computes without a provider.
    fn declaration(&self) -> Declaration {
        let names = self.language.names().is_some();
        let checks = self.language.checks().is_some();
        let supported = |capability| match capability {
            Capability::Project
            | Capability::AnalyzeSource
            | Capability::FileDoc
            | Capability::InsertSite
            | Capability::Syntax
            | Capability::FormatPlan
            | Capability::TestPlan
            | Capability::TestParse => true,
            Capability::AnalysisScope => checks,
            Capability::Linkage => names,
            _ => false,
        };
        Declaration {
            module_id: ModuleId::bundled(self.language.name()),
            package_version: self.version.clone(),
            capabilities: Capability::ALL
                .into_iter()
                .map(|capability| {
                    CapabilityDecl::v0(
                        capability,
                        if supported(capability) {
                            Support::Supported
                        } else {
                            Support::Unsupported
                        },
                    )
                })
                .collect(),
            linkage_kinds: self.coverage(),
        }
    }

    /// Answers through the language's own implementation; a payload that does not decode is
    /// `invalid_request`.
    async fn call<'a>(
        &'a mut self,
        request: Incoming,
        _effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        Ok(match self.answer(&request) {
            Ok(value) => Answer::result(value),
            Err(message) => Answer::error(ErrorCode::InvalidRequest, message),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{
        contract::{Outcome, Role},
        fake::{in_memory, offer, sample_call},
        host::{Call, NoEffects},
    };
    use std::{path::Path, time::Duration};

    /// Argument vectors round-trip through the `argv` recipe; other shapes are refused.
    #[test]
    fn argv_recipe_round_trips() {
        let argv = vec!["black".to_owned(), "-q".to_owned(), "-".to_owned()];
        let effect = argv_effect(&argv);
        assert_eq!(effect_argv(&effect), Some(argv));
        let mut gap = effect.clone();
        gap.params.remove("argv.0001");
        assert_eq!(effect_argv(&gap), None);
        let mut other = effect;
        other.recipe = "check".into();
        assert_eq!(effect_argv(&other), None);
    }

    /// The adapter answers the test language's own support over the wire exactly as in process.
    #[tokio::test]
    async fn the_adapter_answers_like_the_language_in_process() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let (mut channel, reply) = in_memory(
            SupportServer::new(language, "1.0"),
            offer(ModuleId::bundled("alpha"), "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        assert!(
            reply
                .capabilities
                .iter()
                .any(|decl| decl.capability == Capability::Semantic
                    && decl.support == Support::Unsupported)
        );
        let file_doc = channel
            .call(
                sample_call(Capability::FileDoc),
                Duration::from_secs(5),
                &mut NoEffects,
            )
            .await
            .unwrap();
        assert_eq!(
            file_doc.outcome,
            Outcome::Result(encode(&language.support().file_doc("first line\nsecond\n")))
        );
        let call = Call {
            capability: Capability::Project,
            scope_key: "s".into(),
            revision_key: "r".into(),
            payload: encode(&ProjectQuery::Detect {
                root: "/nonexistent".into(),
            }),
            attachments: Vec::new(),
        };
        let detect = channel
            .call(call, Duration::from_secs(5), &mut NoEffects)
            .await
            .unwrap();
        assert_eq!(
            detect.outcome,
            Outcome::Result(encode(
                &language.support().detect(Path::new("/nonexistent"))
            ))
        );
        let semantic = channel
            .call(
                sample_call(Capability::Semantic),
                Duration::from_secs(5),
                &mut NoEffects,
            )
            .await
            .unwrap();
        assert!(
            matches!(semantic.outcome, Outcome::Error(error) if error.code == ErrorCode::Unsupported)
        );
    }

    /// Linkage `resolve` answers the language's own `NameFacts::resolve` in probe order with its
    /// certainty and evidence; a limit below the candidate count caps the list at exactly the
    /// limit; an unknown namespace is refused.
    #[tokio::test]
    async fn linkage_resolve_answers_the_language_probe() {
        use crate::modules::payload::ResolveRequest;
        crate::lang::testing::install();
        let (mut channel, _) = in_memory(
            SupportServer::new(crate::lang::testing::BETA, "1.0"),
            offer(ModuleId::bundled("beta"), "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        let mut resolve = async |namespace: &str, candidate_limit| {
            let call = Call {
                capability: Capability::Linkage,
                scope_key: "s".into(),
                revision_key: "r".into(),
                payload: encode(&LinkageQuery::Resolve(ResolveRequest {
                    namespace: namespace.into(),
                    raw_key: "./lib".into(),
                    from_path: "src/a.ts".into(),
                    source_revision: "r1".into(),
                    config_revision: "c1".into(),
                    candidate_limit,
                })),
                attachments: Vec::new(),
            };
            channel
                .call(call, Duration::from_secs(5), &mut NoEffects)
                .await
                .unwrap()
                .outcome
        };
        let Outcome::Result(value) = resolve("file-ref/v1", 32).await else {
            panic!("resolve answers");
        };
        let answer: ResolveAnswer = decode(value).unwrap();
        let rows: Vec<(&str, AnchorCertainty, &str)> = answer
            .candidates
            .iter()
            .map(|c| (c.normalized_key.as_str(), c.certainty, c.evidence.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("src/lib", AnchorCertainty::Exact, "exact"),
                (
                    "src/lib.alpha",
                    AnchorCertainty::Heuristic,
                    "extension probe"
                ),
                (
                    "src/lib.beta",
                    AnchorCertainty::Heuristic,
                    "extension probe"
                ),
                (
                    "src/lib/index.alpha",
                    AnchorCertainty::Heuristic,
                    "index probe"
                ),
            ]
        );
        assert!(!answer.capped);
        let Outcome::Result(value) = resolve("file-ref/v1", 2).await else {
            panic!("resolve answers");
        };
        let answer: ResolveAnswer = decode(value).unwrap();
        assert!(answer.capped);
        assert_eq!(answer.candidates.len(), 2);
        assert!(matches!(resolve("nope/v1", 32).await, Outcome::Error(_)));
    }
}
