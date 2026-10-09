//! Conformance fixtures of `bundled-module/0`: a fake module that answers every capability with a
//! typed canned result, a fake host that connects any [`ModuleServer`] to a [`HostChannel`] in
//! memory or over a kernel socketpair, and one valid sample call per capability.
//!
//! Language module tasks test their own server against [`in_memory`] / [`socketpair`] before the
//! real host runtime spawns them; the host runtime tests its channel against [`FakeModule`].

use std::{path::PathBuf, time::Duration};

use serde_json::{Value, json};

use super::{
    contract::{
        Capability, CapabilityDecl, Declaration, ErrorCode, HelloOffer, HelloReply, Limits,
        ModuleConfig, ModuleId, ModuleUnavailable, PROTOCOL, Role, Support, VERSION,
    },
    host::{Call, EffectRunner, HostChannel},
    payload::{
        AnalysisScopeRequest, AnalyzeSource, Anchor, AnchorBatch, AnchorCertainty, AnchorRole,
        CallItem, CallsQuery, CheckParseRequest, CheckPlanRequest, ChecksDescription,
        ContextEvidence, DescribeQuery, DiagnosticsEvidence, EditProposal, EffectOutcome,
        EffectRequest, Field, FileDocRequest, FileEdit, FileVerdict, FormatPlanRequest, Hover,
        InsertSiteRequest, LaunchDescription, LinkageCoverage, LinkageQuery, Location,
        OutlineRequest, Param, ProjectQuery, RenameAnswer, RenameRequest, Replacement,
        ResolveAnswer, ResolveCandidate, ResolveRequest, SemanticQuery, SourceAnalysis,
        SourceField, SourceRef, SourceText, SyntaxQuery, TestFacts, TestParseRequest,
        TestPlanQuery, decode, encode,
    },
    serve::{Answer, Effects, Incoming, ModuleServer, ServeError, serve},
    wire::Attachment,
};
use crate::{
    checks::BoxFuture,
    checks::{CheckRequest, CheckState, ProblemSnapshot},
    lang::{
        InsertSite, InsertWhere, LangError, Language, LanguageProject, Outline, SymbolKind,
        SymbolPath, SyntaxVerdict, TestReport, TestSelection,
    },
};

/// A one-shot misbehaviour of [`FakeModule`] on one capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// Never answer.
    Stall,
    /// End the instance mid-request (the host sees the stream end).
    Exit,
    /// Answer that the module's provider exited.
    ProviderExit,
    /// Answer that the module's provider timed out.
    ProviderTimeout,
}

/// A module that answers every capability with a canned typed result after decoding its payload,
/// so a decode failure shows up as `invalid_request`.
pub struct FakeModule {
    /// Identity and capabilities.
    declaration: Declaration,
    /// Pending misbehaviour.
    fault: Option<(Capability, Fault)>,
}

impl FakeModule {
    /// A fake `module_id` at `package_version` declaring every capability supported.
    pub fn new(module_id: ModuleId, package_version: &str) -> Self {
        Self {
            declaration: Declaration {
                module_id,
                package_version: package_version.to_owned(),
                capabilities: Capability::ALL
                    .into_iter()
                    .map(|capability| CapabilityDecl::v0(capability, Support::Supported))
                    .collect(),
                linkage_kinds: vec![class_coverage()],
            },
            fault: None,
        }
    }

    /// Declares `capability` unsupported.
    pub fn without(mut self, capability: Capability) -> Self {
        for decl in &mut self.declaration.capabilities {
            if decl.capability == capability {
                decl.support = Support::Unsupported;
            }
        }
        self
    }

    /// Misbehaves once with `fault` on the next call of `capability`.
    pub fn with_fault(mut self, capability: Capability, fault: Fault) -> Self {
        self.fault = Some((capability, fault));
        self
    }

    /// The registered language this fake serves, if registered in this process.
    fn language(&self) -> Option<Language> {
        Language::by_id(self.declaration.module_id.language())
    }
}

/// The source text of `source`, from inline text or `request`'s attachment.
fn text_of(source: &SourceRef, request: &Incoming) -> Result<String, String> {
    match &source.text {
        SourceText::Inline(text) => Ok(text.clone()),
        SourceText::Missing => Ok(String::new()),
        SourceText::Attachment(id) => request
            .attachment(*id)
            .map(|attachment| String::from_utf8_lossy(&attachment.bytes).into_owned())
            .ok_or_else(|| format!("attachment {id} missing")),
    }
}

/// The fake's one linkage namespace: class uses.
fn class_coverage() -> LinkageCoverage {
    LinkageCoverage {
        namespace: "class/v1".into(),
        defines: false,
        uses: true,
    }
}

/// `value` when the field was requested, else [`Field::NotRequested`].
fn pick<T>(wanted: bool, value: Field<T>) -> Field<T> {
    if wanted { value } else { Field::NotRequested }
}

/// A location covering `[start, end)` of `source`.
fn located(source: &SourceRef, start: u64, end: u64) -> Location {
    Location {
        path: source.path.clone(),
        start_byte: start,
        end_byte: end,
        revision: Some(source.revision.clone()),
    }
}

impl FakeModule {
    /// The canned answer of one decoded request.
    async fn answer(&self, request: &Incoming, effects: &mut Effects<'_>) -> Result<Value, String> {
        let payload = request.payload.clone();
        let value = match request.capability {
            Capability::Project => match decode::<ProjectQuery>(payload)? {
                ProjectQuery::Detect { .. } => json!(null),
                ProjectQuery::Environments { .. } => json!([]),
                ProjectQuery::CheckSelection { selector, .. } => {
                    encode(&Err::<(), String>(format!("unknown selector {selector}")))
                }
                ProjectQuery::CommandEnv { .. } | ProjectQuery::TestToolchain { .. } => json!(null),
            },
            Capability::AnalyzeSource => {
                let query: AnalyzeSource = decode(payload)?;
                let text = text_of(&query.source, request)?;
                let wants = |field| query.fields.contains(&field);
                encode(&SourceAnalysis {
                    outline: pick(wants(SourceField::Outline), Field::Available(None)),
                    file_doc: pick(
                        wants(SourceField::FileDoc),
                        Field::Available(text.lines().next().map(str::to_owned)),
                    ),
                    syntax: pick(
                        wants(SourceField::Syntax),
                        Field::Available(SyntaxVerdict::Clean),
                    ),
                    tests: pick(
                        wants(SourceField::Tests),
                        Field::Available(TestFacts {
                            is_test_file: query.source.path.starts_with("tests"),
                            test_binary: None,
                        }),
                    ),
                    anchors: pick(
                        wants(SourceField::Anchors),
                        Field::Available(AnchorBatch {
                            verdict: FileVerdict::Indexed,
                            capped: false,
                            rejected: 0,
                            coverage: vec![class_coverage()],
                            anchors: vec![Anchor {
                                namespace: "class/v1".into(),
                                domain: String::new(),
                                normalized_key: "btn".into(),
                                role: AnchorRole::Use,
                                location: located(&query.source, 0, text.len().min(3) as u64),
                                certainty: AnchorCertainty::Exact,
                                reason: None,
                            }],
                        }),
                    ),
                })
            }
            Capability::Outline => {
                let query: OutlineRequest = decode(payload)?;
                let text = text_of(&query.source, request)?;
                let language = self.language().ok_or("language not registered")?;
                encode(&Outline {
                    file: query.source.path,
                    language,
                    line_count: crate::lang::line_count(&text),
                    symbols: Vec::new(),
                })
            }
            Capability::FileDoc => {
                let query: FileDocRequest = decode(payload)?;
                json!(text_of(&query.source, request)?.lines().next())
            }
            Capability::InsertSite => {
                let query: InsertSiteRequest = decode(payload)?;
                let answer: Result<InsertSite, LangError> = match query.placement {
                    InsertWhere::First | InsertWhere::Last => {
                        Err(LangError::NotAContainer(query.anchor))
                    }
                    _ => Ok(InsertSite {
                        line: 1,
                        indent: String::new(),
                        blank_before: 0,
                        blank_after: 1,
                    }),
                };
                encode(&answer)
            }
            Capability::Syntax => match decode::<SyntaxQuery>(payload)? {
                SyntaxQuery::Verdict { .. } => encode(&SyntaxVerdict::Clean),
                SyntaxQuery::ProbePlan { .. } => encode(&Some(sample_effect("probe"))),
            },
            Capability::Semantic => match decode::<SemanticQuery>(payload)? {
                SemanticQuery::Context { source, .. } => encode(&ContextEvidence {
                    lexical: None,
                    document_version: Some(1),
                    // A UTF-16 provider; one reference spans the source's first line.
                    position_encoding: "utf-16".into(),
                    definitions: Some(vec![located(&source, 0, 1)]),
                    references: Some(vec![located(
                        &source,
                        0,
                        text_of(&source, request)?.find('\n').unwrap_or(0) as u64,
                    )]),
                    truncated: false,
                    diagnostics: DiagnosticsEvidence {
                        revision: Some(source.revision.clone()),
                        document_version: Some(1),
                        readiness: "clean".into(),
                        freshness: "current".into(),
                        diagnostics: Vec::new(),
                        truncated: false,
                    },
                }),
                SemanticQuery::Hover { source, .. } => encode(&Some(Hover {
                    contents: "fake".into(),
                    range: Some(located(&source, 0, 1)),
                })),
                SemanticQuery::Definitions { source, .. }
                | SemanticQuery::References { source, .. } => {
                    encode(&Some(vec![located(&source, 0, 1)]))
                }
                SemanticQuery::WorkspaceSymbols { .. } => json!([]),
                SemanticQuery::Readiness {} => encode(&super::contract::Readiness::Ready),
                SemanticQuery::Diagnostics { source } => encode(&DiagnosticsEvidence {
                    revision: Some(source.revision),
                    document_version: Some(1),
                    readiness: "clean".into(),
                    freshness: "current".into(),
                    diagnostics: Vec::new(),
                    truncated: false,
                }),
            },
            Capability::Calls => match decode::<CallsQuery>(payload)? {
                CallsQuery::Prepare { source, .. } => encode(&Some(vec![CallItem {
                    name: "f".into(),
                    kind: SymbolKind::Function,
                    detail: None,
                    location: located(&source, 0, 1),
                    selection: located(&source, 0, 1),
                    handle: Some("h1".into()),
                }])),
                CallsQuery::Incoming { .. } | CallsQuery::Outgoing { .. } => json!([]),
            },
            Capability::Rename => {
                let query: RenameRequest = decode(payload)?;
                let answer = if query.sources.is_empty() {
                    RenameAnswer::NeedSources(vec![PathBuf::from("other.txt")])
                } else {
                    RenameAnswer::Proposal(EditProposal {
                        files: std::iter::once(&query.source)
                            .chain(&query.sources)
                            .map(|source| FileEdit {
                                path: source.path.clone(),
                                base_revision: source.revision.clone(),
                                replacements: vec![Replacement {
                                    start_byte: 0,
                                    end_byte: 0,
                                    new_text: query.new_name.clone(),
                                }],
                            })
                            .collect(),
                    })
                };
                encode(&answer)
            }
            Capability::FormatPlan => {
                let _: FormatPlanRequest = decode(payload)?;
                encode(&Some(sample_effect("format")))
            }
            Capability::TestPlan => match decode::<TestPlanQuery>(payload)? {
                TestPlanQuery::Selection { .. } => {
                    encode(&Ok::<TestSelection, LangError>(TestSelection {
                        tests: Vec::new(),
                        command: vec!["runner".into()],
                    }))
                }
                TestPlanQuery::TestIds { outline_paths, .. } => json!(outline_paths),
            },
            Capability::TestParse => {
                let query: TestParseRequest = decode(payload)?;
                let stdout = request.attachment(query.stdout).ok_or("stdout missing")?;
                encode(&TestReport {
                    passed: stdout.bytes.len() as u32,
                    ..TestReport::default()
                })
            }
            Capability::CheckPlan => {
                let query: CheckPlanRequest = decode(payload)?;
                let (outcome, output) = effects
                    .run(check_effect(&query.config))
                    .await
                    .map_err(|error| error.to_string())?;
                let EffectOutcome::Completed { truncated, .. } = outcome else {
                    return Err(format!("check effect refused: {outcome:?}"));
                };
                let language = self.language().ok_or("language not registered")?;
                let bytes = output
                    .iter()
                    .map(|attachment| attachment.bytes.len())
                    .sum::<usize>();
                let mut snapshot = ProblemSnapshot::from_problems(
                    language,
                    CheckState::Ready,
                    Vec::new(),
                    query.request.input_generation,
                    0,
                );
                snapshot.detail = Some(format!(
                    "{bytes} output bytes{}",
                    if truncated { ", truncated" } else { "" }
                ));
                encode(&snapshot)
            }
            Capability::CheckParse => {
                let query: CheckParseRequest = decode(payload)?;
                let language = self.language().ok_or("language not registered")?;
                let stdout = request.attachment(query.stdout).ok_or("stdout missing")?;
                let mut snapshot = ProblemSnapshot::from_problems(
                    language,
                    CheckState::Ready,
                    Vec::new(),
                    query.request.input_generation,
                    0,
                );
                snapshot.detail = Some(format!("{} output bytes", stdout.bytes.len()));
                encode(&snapshot)
            }
            Capability::AnalysisScope => {
                let _: AnalysisScopeRequest = decode(payload)?;
                json!(null)
            }
            Capability::Describe => match decode::<DescribeQuery>(payload)? {
                DescribeQuery::Provider { .. } => {
                    encode(&Ok::<LaunchDescription, String>(LaunchDescription {
                        valid: true,
                        executables: Vec::new(),
                        toolchain_programs: Vec::new(),
                        probe_programs: None,
                    }))
                }
                DescribeQuery::VerifyProvider { .. } => encode(&Ok::<(), String>(())),
                DescribeQuery::Checks { section } => {
                    encode(&Ok::<ChecksDescription, String>(ChecksDescription {
                        valid: true,
                        programs: serde_json::from_value(section["programs"].clone())
                            .unwrap_or_default(),
                        launcher_roots: serde_json::from_value(section["launcher_roots"].clone())
                            .unwrap_or_default(),
                        developer_dirs: serde_json::from_value(section["developer_dirs"].clone())
                            .unwrap_or_default(),
                    }))
                }
                DescribeQuery::Presence { .. } => json!(true),
            },
            Capability::Linkage => match decode::<LinkageQuery>(payload)? {
                LinkageQuery::Anchors { .. } => encode(&AnchorBatch {
                    verdict: FileVerdict::Skipped("minified".into()),
                    capped: false,
                    rejected: 0,
                    coverage: vec![class_coverage()],
                    anchors: Vec::new(),
                }),
                LinkageQuery::Resolve(resolve) => encode(&ResolveAnswer {
                    candidates: vec![ResolveCandidate {
                        domain: String::new(),
                        normalized_key: resolve.raw_key,
                        certainty: AnchorCertainty::Unverified,
                        evidence: "fake".into(),
                    }],
                    capped: false,
                }),
            },
        };
        Ok(value)
    }
}

impl ModuleServer for FakeModule {
    /// The fake declaration.
    fn declaration(&self) -> Declaration {
        self.declaration.clone()
    }

    /// Acts out a pending fault, else answers the canned result or `invalid_request`.
    async fn call<'a>(
        &'a mut self,
        request: Incoming,
        mut effects: Effects<'a>,
    ) -> Result<Answer, ServeError> {
        if let Some((capability, fault)) = self.fault
            && capability == request.capability
        {
            self.fault = None;
            return match fault {
                Fault::Stall => std::future::pending().await,
                Fault::Exit => Err(ServeError::Protocol("fake exit".into())),
                Fault::ProviderExit => Ok(Answer::unavailable(
                    super::contract::Stage::Provider,
                    super::contract::Cause::Exited,
                    "provider exited",
                )),
                Fault::ProviderTimeout => Ok(Answer::unavailable(
                    super::contract::Stage::Provider,
                    super::contract::Cause::Timeout,
                    "provider timed out",
                )),
            };
        }
        Ok(match self.answer(&request, &mut effects).await {
            Ok(value) => Answer::result(value),
            Err(message) => Answer::error(ErrorCode::InvalidRequest, message),
        })
    }
}

/// The fake's check recipe: with `{"roots": n}` in its configuration, `n` ancestor files; the
/// configuration's `params` object adds typed parameters (and its `programs`, the named programs
/// `describe` reports), so a test can drive any declared recipe.
fn check_effect(config: &Value) -> EffectRequest {
    let mut effect = sample_effect("check");
    if let Ok(params) = serde_json::from_value::<std::collections::BTreeMap<String, Param>>(
        config["params"].clone(),
    ) {
        effect.params.extend(params);
    }
    if let Some(count) = config["roots"].as_u64() {
        let roots = (0..count)
            .map(|n| PathBuf::from(format!("/deep/{n:05}/Cargo.toml")))
            .collect();
        effect.params.insert("roots".into(), Param::Paths(roots));
    }
    effect
}

/// A recipe request with no parameters.
fn sample_effect(recipe: &str) -> EffectRequest {
    EffectRequest {
        recipe: recipe.to_owned(),
        params: Default::default(),
    }
}

/// An [`EffectRunner`] that completes every effect with `stdout` and an empty stderr, counting
/// the runs.
pub struct FakeEffects {
    /// Stdout of every run.
    pub stdout: Vec<u8>,
    /// Effects run so far.
    pub runs: u32,
    /// The latest effect requested.
    pub last: Option<EffectRequest>,
}

impl EffectRunner for FakeEffects {
    /// Completes with the configured output.
    fn run<'a>(
        &'a mut self,
        _fence: &'a super::contract::Fence,
        effect: EffectRequest,
    ) -> BoxFuture<'a, (EffectOutcome, Vec<Attachment>)> {
        self.runs += 1;
        self.last = Some(effect);
        let runs = self.runs;
        let stdout = self.stdout.clone();
        Box::pin(async move {
            (
                EffectOutcome::Completed {
                    effect_id: format!("effect-{runs}"),
                    status: Some(0),
                    timed_out: false,
                    truncated: false,
                    stdout_bytes: stdout.len() as u64,
                    stderr_bytes: 0,
                },
                vec![
                    Attachment::octets(1, stdout),
                    Attachment::octets(2, Vec::new()),
                ],
            )
        })
    }
}

/// An offer for `module_id` at `package_version` in `role`, requesting every capability.
pub fn offer(module_id: ModuleId, package_version: &str, role: Role, instance: u64) -> HelloOffer {
    HelloOffer {
        protocol: PROTOCOL.to_owned(),
        versions: vec![VERSION],
        module_id,
        package_version: package_version.to_owned(),
        executable_digest: "0".repeat(64),
        instance,
        role,
        limits: Limits::default(),
        requested_caps: Capability::ALL.to_vec(),
        config: ModuleConfig::default(),
    }
}

/// Serves `server` in `offer`'s role on an in-memory duplex pair and opens a host channel on it.
pub async fn in_memory<S: ModuleServer + 'static>(
    server: S,
    offer: HelloOffer,
) -> Result<(HostChannel, HelloReply), ModuleUnavailable> {
    let (core_out, module_in) = tokio::io::duplex(1 << 16);
    let (module_out, core_in) = tokio::io::duplex(1 << 16);
    let role = offer.role;
    tokio::spawn(async move {
        let _ = serve(server, role, module_in, module_out).await;
    });
    HostChannel::open(core_in, core_out, offer, Duration::from_secs(5)).await
}

/// Serves `server` in `offer`'s role on one end of a kernel socketpair and opens a host channel
/// on the other.
pub async fn socketpair<S: ModuleServer + 'static>(
    server: S,
    offer: HelloOffer,
) -> Result<(HostChannel, HelloReply), ModuleUnavailable> {
    let (core, module) = tokio::net::UnixStream::pair().expect("a socketpair for the fake module");
    let (module_in, module_out) = module.into_split();
    let (core_in, core_out) = core.into_split();
    let role = offer.role;
    tokio::spawn(async move {
        let _ = serve(server, role, module_in, module_out).await;
    });
    HostChannel::open(core_in, core_out, offer, Duration::from_secs(5)).await
}

/// A small source `a.txt` at revision `r1`.
fn sample_source() -> SourceRef {
    SourceRef {
        path: "a.txt".into(),
        revision: "r1".into(),
        text: SourceText::Inline("first line\nsecond\n".into()),
    }
}

/// The first registered language; sample payloads that name a language need one.
fn language() -> Language {
    *crate::lang::registered()
        .first()
        .expect("register the languages before building sample calls")
}

/// One valid call of `capability`, for conformance smoke tests against any module. Payloads that
/// carry a language name the first registered one.
pub fn sample_call(capability: Capability) -> Call {
    let source = sample_source();
    let check = CheckRequest {
        worktree: "/w".into(),
        cache_dir: "/c".into(),
        input_generation: 7,
        read_denies: Vec::new(),
    };
    let mut attachments = Vec::new();
    let payload = match capability {
        Capability::Project => encode(&ProjectQuery::Detect { root: "/w".into() }),
        Capability::AnalyzeSource => {
            attachments.push(Attachment {
                id: 1,
                content_type: "text/plain; charset=utf-8".into(),
                bytes: b".btn {}\n".to_vec(),
            });
            encode(&AnalyzeSource {
                source: SourceRef {
                    text: SourceText::Attachment(1),
                    ..source
                },
                fields: vec![
                    SourceField::Outline,
                    SourceField::FileDoc,
                    SourceField::Syntax,
                    SourceField::Tests,
                    SourceField::Anchors,
                ],
            })
        }
        Capability::Outline => encode(&OutlineRequest { source }),
        Capability::FileDoc => encode(&FileDocRequest { source }),
        Capability::InsertSite => encode(&InsertSiteRequest {
            source,
            outline: Outline {
                file: "a.txt".into(),
                language: language(),
                line_count: 2,
                symbols: Vec::new(),
            },
            anchor: SymbolPath::parse("a.txt#f").expect("a valid path"),
            placement: InsertWhere::After,
        }),
        Capability::Syntax => encode(&SyntaxQuery::Verdict { source }),
        Capability::Semantic => encode(&SemanticQuery::References {
            source,
            byte_offset: 0,
        }),
        Capability::Calls => encode(&CallsQuery::Prepare {
            source,
            byte_offset: 0,
        }),
        Capability::Rename => encode(&RenameRequest {
            source,
            byte_offset: 0,
            new_name: "g".into(),
            sources: Vec::new(),
        }),
        Capability::FormatPlan => encode(&FormatPlanRequest {
            project: LanguageProject {
                language: language(),
                manifests: Vec::new(),
                environment: Vec::new(),
                interpreter: None,
                commands: Default::default(),
                entry_points: Vec::new(),
            },
            file: "a.txt".into(),
        }),
        Capability::TestPlan => encode(&TestPlanQuery::TestIds {
            file: "tests/a.txt".into(),
            outline_paths: vec!["t".into()],
        }),
        Capability::TestParse => {
            attachments.push(Attachment::octets(1, b"ok\n".to_vec()));
            attachments.push(Attachment::octets(2, Vec::new()));
            encode(&TestParseRequest {
                stdout: 1,
                stderr: 2,
            })
        }
        Capability::CheckPlan => encode(&CheckPlanRequest {
            request: check,
            config: json!({}),
            timeout_ms: 1000,
        }),
        Capability::CheckParse => {
            attachments.push(Attachment::octets(1, vec![0xff; 3]));
            attachments.push(Attachment::octets(2, Vec::new()));
            encode(&CheckParseRequest {
                request: check,
                outcome: EffectOutcome::Completed {
                    effect_id: "e1".into(),
                    status: Some(1),
                    timed_out: false,
                    truncated: false,
                    stdout_bytes: 3,
                    stderr_bytes: 0,
                },
                stdout: 1,
                stderr: 2,
            })
        }
        Capability::AnalysisScope => encode(&AnalysisScopeRequest {
            worktree: "/w".into(),
            path: "a.txt".into(),
        }),
        Capability::Describe => encode(&DescribeQuery::Checks {
            section: json!({"timeout_ms": 1000}),
        }),
        Capability::Linkage => encode(&LinkageQuery::Resolve(ResolveRequest {
            namespace: "file-ref/v1".into(),
            raw_key: "./a.style".into(),
            from_path: "a.txt".into(),
            source_revision: "r1".into(),
            config_revision: "c1".into(),
            candidate_limit: 32,
        })),
    };
    Call {
        capability,
        scope_key: "scope".into(),
        revision_key: "rev".into(),
        payload,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{
        contract::{Cause, Outcome, Stage},
        host::NoEffects,
        payload::DetectAnswer,
        wire::{MAX_CHUNK, MAX_CONTROL, read_frame, write_attachment, write_control},
    };

    /// `bundled.alpha`, registered by the core's test languages.
    fn alpha() -> ModuleId {
        crate::lang::testing::install();
        ModuleId::bundled("alpha")
    }

    /// Runs `hello` plus one sample call per capability over `channel` and checks every reply is
    /// a typed result.
    async fn one_call_per_capability(mut channel: HostChannel) {
        let mut effects = FakeEffects {
            stdout: vec![7; MAX_CHUNK * 3 + 1],
            runs: 0,
            last: None,
        };
        for capability in Capability::ALL {
            let reply = channel
                .call(
                    sample_call(capability),
                    Duration::from_secs(5),
                    &mut effects,
                )
                .await
                .unwrap_or_else(|error| panic!("{capability:?}: {error}"));
            let Outcome::Result(value) = reply.outcome else {
                panic!("{capability:?} answered {:?}", reply.outcome);
            };
            match capability {
                Capability::Project => {
                    assert_eq!(decode::<DetectAnswer>(value), Ok(None));
                }
                Capability::AnalyzeSource => {
                    let analysis: SourceAnalysis = decode(value).unwrap();
                    assert_eq!(analysis.file_doc, Field::Available(Some(".btn {}".into())));
                    let source = SourceRef {
                        path: "a.txt".into(),
                        revision: "r1".into(),
                        text: SourceText::Attachment(1),
                    };
                    let Field::Available(batch) = analysis.anchors else {
                        panic!("anchors expected");
                    };
                    let declared = [class_coverage()];
                    let check =
                        |batch: &AnchorBatch| batch.validate(&source, ".btn {}\n", &declared);
                    assert_eq!(check(&batch), Ok(()));
                    let mut moved = batch.clone();
                    moved.anchors[0].location.path = "b.txt".into();
                    assert!(check(&moved).is_err());
                    let mut stale = batch.clone();
                    stale.anchors[0].location.revision = Some("r0".into());
                    assert!(check(&stale).is_err());
                    let mut past = batch.clone();
                    past.anchors[0].location.end_byte = 99;
                    assert!(check(&past).is_err());
                    let mut undeclared = batch.clone();
                    undeclared.anchors[0].namespace = "id/v1".into();
                    assert!(check(&undeclared).is_err(), "namespace not declared");
                    let mut invented = batch.clone();
                    invented.anchors[0].namespace = "invented/v1".into();
                    invented.coverage = vec![LinkageCoverage {
                        namespace: "invented/v1".into(),
                        defines: true,
                        uses: true,
                    }];
                    assert!(check(&invented).is_err(), "self-claimed coverage");
                    let invented_declared = [invented.coverage[0].clone()];
                    assert!(
                        invented
                            .validate(&source, ".btn {}\n", &invented_declared)
                            .is_err(),
                        "unregistered namespace even when declared"
                    );
                    let mut definition = batch.clone();
                    definition.anchors[0].role = AnchorRole::Definition;
                    assert!(check(&definition).is_err(), "declaration covers uses only");
                    let mut skipped = batch.clone();
                    skipped.verdict = FileVerdict::Skipped("minified".into());
                    assert!(check(&skipped).is_err(), "no facts on a skipped file");
                    let mut point = batch;
                    point.anchors[0].location.end_byte = point.anchors[0].location.start_byte;
                    assert_eq!(check(&point), Ok(()), "point anchors stay valid");
                }
                Capability::Rename => {
                    assert!(matches!(
                        decode::<RenameAnswer>(value),
                        Ok(RenameAnswer::NeedSources(_))
                    ));
                }
                Capability::CheckPlan => {
                    let snapshot: ProblemSnapshot = decode(value).unwrap();
                    assert_eq!(snapshot.input_generation, 7);
                    let bytes = (MAX_CHUNK * 3 + 1).to_string();
                    assert_eq!(snapshot.detail, Some(format!("{bytes} output bytes")));
                }
                Capability::TestParse => {
                    assert_eq!(decode::<TestReport>(value).unwrap().passed, 3);
                }
                _ => {}
            }
        }
        assert_eq!(effects.runs, 1, "only check_plan runs an effect");
        channel.shutdown().await;
    }

    /// The fake host and fake module complete `hello` plus one call per capability in memory.
    #[tokio::test]
    async fn fake_pair_answers_every_capability_in_memory() {
        let id = alpha();
        let (channel, reply) = in_memory(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        assert_eq!(reply.capabilities.len(), Capability::ALL.len());
        one_call_per_capability(channel).await;
    }

    /// The same over a kernel socketpair.
    #[tokio::test]
    async fn fake_pair_answers_every_capability_over_a_socketpair() {
        let id = alpha();
        let (channel, _) = socketpair(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Checker, 2),
        )
        .await
        .unwrap();
        one_call_per_capability(channel).await;
    }

    /// A version or role mismatch is refused at `hello` as `incompatible`; an unsupported
    /// capability answers a typed `unsupported` error and leaves the instance usable; the
    /// placeholder module declares everything unsupported.
    #[tokio::test]
    async fn hello_refusals_and_unsupported_capabilities() {
        let id = alpha();
        let error = in_memory(
            FakeModule::new(id.clone(), "0.9"),
            offer(id.clone(), "1.0", Role::Analyzer, 1),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Hello, Cause::Incompatible)
        );
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0").without(Capability::Calls),
            offer(id.clone(), "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        for capability in [Capability::Calls, Capability::Outline] {
            let reply = channel
                .call(
                    sample_call(capability),
                    Duration::from_secs(5),
                    &mut NoEffects,
                )
                .await
                .unwrap();
            assert_eq!(
                matches!(reply.outcome, Outcome::Error(ref error) if error.code == ErrorCode::Unsupported),
                capability == Capability::Calls
            );
        }
        let (mut placeholder, _) = in_memory(
            super::super::serve::Unimplemented::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        let reply = placeholder
            .call(
                sample_call(Capability::Outline),
                Duration::from_secs(5),
                &mut NoEffects,
            )
            .await
            .unwrap();
        assert!(
            matches!(reply.outcome, Outcome::Error(error) if error.code == ErrorCode::Unsupported)
        );
    }

    /// A stall past the budget and an exit mid-request each poison the channel with their
    /// typed cause, and every later call reports the same fault.
    #[tokio::test]
    async fn stalls_and_exits_are_typed_and_poison() {
        let id = alpha();
        for (fault, cause) in [(Fault::Stall, Cause::Timeout), (Fault::Exit, Cause::Exited)] {
            let (mut channel, _) = in_memory(
                FakeModule::new(id.clone(), "1.0").with_fault(Capability::Outline, fault),
                offer(id.clone(), "1.0", Role::Analyzer, 1),
            )
            .await
            .unwrap();
            let error = channel
                .call(
                    sample_call(Capability::Outline),
                    Duration::from_millis(200),
                    &mut NoEffects,
                )
                .await
                .unwrap_err();
            assert_eq!((error.stage, error.cause), (Stage::Request, cause));
            assert_eq!(
                error.to_string(),
                format!("module_unavailable (bundled.alpha:request:{cause})")
            );
            let again = channel
                .call(
                    sample_call(Capability::FileDoc),
                    Duration::from_secs(5),
                    &mut NoEffects,
                )
                .await
                .unwrap_err();
            assert_eq!(again.cause, cause);
        }
    }

    /// A scripted module body: reads from its first stream, writes on the second.
    type Script = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

    /// Opens a host channel against a scripted raw module that first answers `hello` correctly.
    async fn scripted(
        script: impl FnOnce(tokio::io::DuplexStream, tokio::io::DuplexStream, u64) -> Script
        + Send
        + 'static,
    ) -> HostChannel {
        let id = alpha();
        let offer = offer(id.clone(), "1.0", Role::Analyzer, 4);
        let declaration = FakeModule::new(id, "1.0").declaration;
        let (core_out, mut module_in) = tokio::io::duplex(1 << 20);
        let (mut module_out, core_in) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let Ok(super::super::wire::Frame::Control(hello)) = read_frame(&mut module_in).await
            else {
                return;
            };
            let super::super::contract::Control::Hello(offer) =
                serde_json::from_value(hello).unwrap()
            else {
                return;
            };
            let reply = declaration.answer(&offer).unwrap();
            write_control(
                &mut module_out,
                &super::super::contract::Control::HelloReply(reply),
            )
            .await
            .unwrap();
            script(module_in, module_out, offer.instance).await;
        });
        HostChannel::open(core_in, core_out, offer, Duration::from_secs(5))
            .await
            .unwrap()
            .0
    }

    /// Wrong fences, undeclared or broken attachment deliveries, malformed and oversized frames
    /// each poison the channel with their own cause.
    #[tokio::test]
    async fn host_refuses_bad_replies() {
        use super::super::contract::{Control, Coverage, Fence, Readiness, Response};
        use super::super::wire::{AttachmentDecl, Chunk, write_chunk};
        /// A response to `request_id` with `fence` changes and `decls`.
        fn response(fence: Fence, decls: Vec<AttachmentDecl>) -> Control {
            Control::Response(Response {
                fence,
                outcome: Outcome::Result(json!(null)),
                body_attachment: None,
                readiness: Readiness::Ready,
                coverage: Coverage::Complete,
                attachments: decls,
            })
        }
        let decl = |length| AttachmentDecl {
            id: 1,
            length,
            content_type: "application/octet-stream".into(),
        };
        let cases: Vec<(&str, Cause)> = vec![
            ("wrong-request", Cause::WrongFence),
            ("wrong-scope", Cause::WrongFence),
            ("wrong-instance", Cause::WrongFence),
            ("gap", Cause::Malformed),
            ("overlap", Cause::Malformed),
            ("overrun", Cause::Malformed),
            ("truncated", Cause::Exited),
            ("chunk-for-other-request", Cause::WrongFence),
            ("malformed", Cause::Malformed),
            ("oversized", Cause::Oversized),
            ("unknown-kind", Cause::Malformed),
            ("zero-length", Cause::Malformed),
            ("over-budget", Cause::Oversized),
        ];
        for (case, cause) in cases {
            let mut channel = scripted(move |mut input, mut output, instance| {
                Box::pin(async move {
                    let Ok(super::super::wire::Frame::Control(request)) =
                        read_frame(&mut input).await
                    else {
                        return;
                    };
                    let Control::Request(request) = serde_json::from_value(request).unwrap() else {
                        return;
                    };
                    let mut fence = request.fence.clone();
                    let chunk = |offset, bytes: &[u8]| Chunk {
                        request_id: fence.request_id,
                        attachment: 1,
                        offset,
                        bytes: bytes.to_vec(),
                    };
                    let (control, chunks): (Option<Control>, Vec<Chunk>) = match case {
                        "wrong-request" => {
                            fence.request_id += 1;
                            (Some(response(fence, vec![])), vec![])
                        }
                        "wrong-scope" => {
                            fence.scope_key.push('x');
                            (Some(response(fence, vec![])), vec![])
                        }
                        "wrong-instance" => {
                            fence.instance = instance + 1;
                            (Some(response(fence, vec![])), vec![])
                        }
                        "gap" => (
                            Some(response(fence.clone(), vec![decl(4)])),
                            vec![chunk(1, b"a")],
                        ),
                        "overlap" => (
                            Some(response(fence.clone(), vec![decl(4)])),
                            vec![chunk(0, b"ab"), chunk(1, b"b")],
                        ),
                        "overrun" => (
                            Some(response(fence.clone(), vec![decl(1)])),
                            vec![chunk(0, b"ab")],
                        ),
                        "truncated" => (
                            Some(response(fence.clone(), vec![decl(4)])),
                            vec![chunk(0, b"ab")],
                        ),
                        "chunk-for-other-request" => {
                            let mut other = chunk(0, b"ab");
                            other.request_id += 1;
                            (Some(response(fence.clone(), vec![decl(2)])), vec![other])
                        }
                        "over-budget" => {
                            (Some(response(fence.clone(), vec![decl(u64::MAX)])), vec![])
                        }
                        _ => (None, vec![]),
                    };
                    match (control, case) {
                        (Some(control), _) => {
                            write_control(&mut output, &control).await.unwrap();
                            for chunk in chunks {
                                write_chunk(&mut output, &chunk).await.unwrap();
                            }
                        }
                        (None, "malformed") => {
                            use tokio::io::AsyncWriteExt;
                            output.write_all(b"\0\0\0\x06\0{not}").await.unwrap();
                        }
                        (None, "oversized") => {
                            use tokio::io::AsyncWriteExt;
                            output
                                .write_all(&(MAX_CONTROL as u32 + 2).to_be_bytes())
                                .await
                                .unwrap();
                            output.write_all(&[0]).await.unwrap();
                        }
                        (None, "unknown-kind") => {
                            use tokio::io::AsyncWriteExt;
                            output.write_all(b"\0\0\0\x01\x09").await.unwrap();
                        }
                        (None, _) => {
                            use tokio::io::AsyncWriteExt;
                            output.write_all(b"\0\0\0\0").await.unwrap();
                        }
                    }
                    // Hold the stream open so only the delivery itself can fail the call,
                    // except for the truncated case, which ends it.
                    if case != "truncated" {
                        std::future::pending::<()>().await;
                    }
                })
            })
            .await;
            let error = channel
                .call(
                    sample_call(Capability::FileDoc),
                    Duration::from_secs(5),
                    &mut NoEffects,
                )
                .await
                .unwrap_err();
            assert_eq!(error.cause, cause, "{case}");
            assert!(channel.fault().is_some(), "{case}");
        }
    }

    /// A provider exit and a provider timeout inside a live module arrive as distinct typed
    /// `unavailable` errors (stage `provider`, cause `exited` or `timeout`), never as text; the
    /// instance stays usable. An `unavailable` error without its stage and cause is malformed.
    #[tokio::test]
    async fn provider_failures_are_typed_not_text() {
        use super::super::contract::{Cause, ErrorCode, Stage, Unavailable};
        let id = alpha();
        for (fault, cause) in [
            (Fault::ProviderExit, Cause::Exited),
            (Fault::ProviderTimeout, Cause::Timeout),
        ] {
            let (mut channel, _) = in_memory(
                FakeModule::new(id.clone(), "1.0").with_fault(Capability::Semantic, fault),
                offer(id.clone(), "1.0", Role::Analyzer, 1),
            )
            .await
            .unwrap();
            let reply = channel
                .call(
                    sample_call(Capability::Semantic),
                    Duration::from_secs(5),
                    &mut NoEffects,
                )
                .await
                .unwrap();
            let Outcome::Error(error) = reply.outcome else {
                panic!("typed error expected");
            };
            assert_eq!(error.code, ErrorCode::Unavailable);
            assert_eq!(
                error.unavailable,
                Some(Unavailable {
                    stage: Stage::Provider,
                    cause
                })
            );
            assert!(
                channel
                    .call(
                        sample_call(Capability::Semantic),
                        Duration::from_secs(5),
                        &mut NoEffects
                    )
                    .await
                    .is_ok(),
                "the instance stays usable"
            );
        }
        let untyped = super::super::contract::ModuleError {
            code: ErrorCode::Unavailable,
            message: "provider exited".into(),
            unavailable: None,
        };
        assert!(!untyped.well_formed());
    }

    /// The readiness query answers the hosted provider's status barrier as a typed readiness.
    #[tokio::test]
    async fn the_readiness_query_answers_the_provider_barrier() {
        let id = alpha();
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        let mut call = sample_call(Capability::Semantic);
        call.payload = encode(&SemanticQuery::Readiness {});
        let reply = channel
            .call(call, Duration::from_secs(5), &mut NoEffects)
            .await
            .unwrap();
        let super::super::contract::Outcome::Result(value) = reply.outcome else {
            panic!("a readiness answer");
        };
        assert_eq!(
            decode::<super::super::contract::Readiness>(value).unwrap(),
            super::super::contract::Readiness::Ready
        );
    }

    /// A late reply after an abandoned call can never settle the next request: the abandoned
    /// call poisons the channel.
    #[tokio::test]
    async fn an_abandoned_call_poisons_the_next() {
        let id = alpha();
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0").with_fault(Capability::Outline, Fault::Stall),
            offer(id, "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            channel.call(
                sample_call(Capability::Outline),
                Duration::from_secs(5),
                &mut NoEffects,
            ),
        )
        .await;
        assert!(abandoned.is_err());
        let error = channel
            .call(
                sample_call(Capability::FileDoc),
                Duration::from_secs(5),
                &mut NoEffects,
            )
            .await
            .unwrap_err();
        assert_eq!((error.stage, error.cause), (Stage::Request, Cause::Timeout));
    }

    /// A structured payload and result over the control-frame ceiling spill into
    /// `application/json` attachments both ways and arrive decoded, unchanged.
    #[tokio::test]
    async fn large_structured_bodies_spill_both_ways() {
        let id = alpha();
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Analyzer, 1),
        )
        .await
        .unwrap();
        let paths: Vec<String> = (0..60_000).map(|n| format!("tests::case_{n:08}")).collect();
        let mut call = sample_call(Capability::TestPlan);
        call.payload = encode(&TestPlanQuery::TestIds {
            file: "tests/a.txt".into(),
            outline_paths: paths.clone(),
        });
        assert!(serde_json::to_vec(&call.payload).unwrap().len() > MAX_CONTROL);
        let reply = channel
            .call(call, Duration::from_secs(20), &mut NoEffects)
            .await
            .unwrap();
        assert!(reply.attachments.is_empty(), "the spilled body is consumed");
        assert_eq!(reply.outcome, Outcome::Result(json!(paths)));
    }

    /// An effect carrying 30,000 ancestor files, well over the 512 KiB inline limit and any small
    /// fixed count, reaches the core's effect runner unchanged: parameter lists spill into a JSON
    /// attachment instead of being cut.
    #[tokio::test]
    async fn large_effect_parameter_lists_reach_the_core_whole() {
        let id = alpha();
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Checker, 1),
        )
        .await
        .unwrap();
        let mut call = sample_call(Capability::CheckPlan);
        call.payload["config"] = json!({"roots": 30_000});
        let mut effects = FakeEffects {
            stdout: b"{}".to_vec(),
            runs: 0,
            last: None,
        };
        let reply = channel
            .call(call, Duration::from_secs(20), &mut effects)
            .await
            .unwrap();
        assert!(
            matches!(reply.outcome, Outcome::Result(_)),
            "{:?}",
            reply.outcome
        );
        let last = effects.last.expect("the effect ran");
        assert!(encode(&last).to_string().len() > crate::modules::contract::MAX_INLINE_BODY);
        assert_eq!(last, check_effect(&json!({"roots": 30_000})));
        let Some(Param::Paths(roots)) = last.params.get("roots") else {
            panic!("roots expected");
        };
        assert_eq!(roots.len(), 30_000);
    }

    /// Large attachments travel as raw chunks in both directions without JSON number arrays.
    #[tokio::test]
    async fn large_attachments_round_trip() {
        let id = alpha();
        let (mut channel, _) = in_memory(
            FakeModule::new(id.clone(), "1.0"),
            offer(id, "1.0", Role::Checker, 1),
        )
        .await
        .unwrap();
        let big = vec![0xfe; 3 * 1024 * 1024];
        let mut call = sample_call(Capability::TestParse);
        call.attachments[0] = Attachment::octets(1, big.clone());
        let reply = channel
            .call(call, Duration::from_secs(20), &mut NoEffects)
            .await
            .unwrap();
        let Outcome::Result(value) = reply.outcome else {
            panic!("typed result expected");
        };
        assert_eq!(
            decode::<TestReport>(value).unwrap().passed,
            big.len() as u32
        );
        let mut sink = Vec::new();
        write_attachment(&mut sink, 1, 1, &big).await.unwrap();
        assert!(
            sink.len() < big.len() + big.len() / 100,
            "raw bytes, not a JSON array"
        );
        let _ = write_control(&mut sink, &json!({})).await;
    }
}
