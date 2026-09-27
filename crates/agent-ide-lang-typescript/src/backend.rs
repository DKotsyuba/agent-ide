//! Worker-side TypeScript integration: one long-lived bridge session per binding and project.
//!
//! A session is bound to the exact project inputs (tsconfig and package files) observed when it
//! started. Every request re-observes them; a changed project retires the session and answers
//! `resolution_unverified` with the reason, so a reply never mixes two project configurations.

use std::{
    any::Any,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use serde::Deserialize;

use agent_ide_core::{
    assistance::{
        host_binding::BindingRef,
        launcher::{AcceptedExecutable, LauncherError, ProviderLaunch, absolute, identifier},
        reply::FailureCode,
    },
    checks::BoxFuture,
    execution::AdmissionClass,
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ProviderSettings},
    },
    lang::Language,
    workspace::{authority::WorktreeRef, observation::SourceObservation},
};

use crate::profile::{
    ProjectResolutionInputsV1, TypeScriptBundleFileV1, TypeScriptProfile, TypeScriptProfileError,
    TypeScriptProfiles, TypeScriptProtocolChild, TypeScriptProviderBundleV1,
    TypeScriptProviderBundleV1Identity, TypeScriptView, TypeScriptViewAdmission,
    TypeScriptWorktree,
};

/// Failure detail when TypeScript project inputs no longer match the snapshot a session was
/// started from; the next request observes them afresh.
pub const TYPESCRIPT_INPUTS_CHANGED: &str =
    "TypeScript project inputs (tsconfig/package files) changed since the session started";

/// Compiled Codex release record prefix for the exact TypeScript r3 macOS bundle cell.
pub const TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1: &str =
    "macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r3-2026-09-14";
/// Compiled Claude release record prefix for the same exact TypeScript r3 macOS bundle cell.
pub const TYPESCRIPT_CLAUDE_MACOS_EVIDENCE_V1: &str =
    "macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-claude-r3-2026-09-14";
/// Exact Node release admitted by the compiled Codex TypeScript record.
pub const TYPESCRIPT_NODE_VERSION_V1: &str = "24.4.0";
/// BLAKE3 identity of the accepted macOS Node 24.4.0 executable bytes.
pub const TYPESCRIPT_NODE_BLAKE3_V1: &str =
    "f3d5f7b7c7296b22889c5ca6a62fbfebc6f263190cefec97255d98a286e92ce9";
/// Exact TypeScript Language Server release admitted by the compiled Codex record.
pub const TYPESCRIPT_BRIDGE_VERSION_V1: &str = "6.0.0";
/// BLAKE3 identity of the accepted TypeScript Language Server 6.0.0 bridge bytes.
pub const TYPESCRIPT_BRIDGE_BLAKE3_V1: &str =
    "541877f06eff230f60b5ca90332d2db54128d0dd8b88bd7fdc987976e22a8c9b";
/// Exact accepted TypeScript Language Server bridge byte length.
pub const TYPESCRIPT_BRIDGE_BYTES_V1: u64 = 917_064;
/// Exact TypeScript release admitted by the compiled Codex record.
pub const TYPESCRIPT_VERSION_V1: &str = "5.9.3";
/// BLAKE3 identity of the accepted TypeScript 5.9.3 `tsserver.js` bytes.
pub const TYPESCRIPT_TSSERVER_BLAKE3_V1: &str =
    "fd205df6b7930ede592846b8aeabc046f75a76f8f4eaf74a2b6dba9b3bd6a1a8";
/// Exact accepted TypeScript 5.9.3 `tsserver.js` byte length.
pub const TYPESCRIPT_TSSERVER_BYTES_V1: u64 = 272;
/// Ordered basename, BLAKE3 digest, and length of the accepted loaded runtime closure.
pub const TYPESCRIPT_CLOSURE_V1: [(&str, &str, u64); 4] = [
    (
        "_tsserver.js",
        "2f5f9a981943299237ca1a8f566aff95814508abf919027c4f5bb82dc9c5762f",
        27_888,
    ),
    (
        "typescript.js",
        "90519822fe3575779770b1e3a921528d30777e2be3c97cb68457caf2c22393e9",
        9_112_572,
    ),
    (
        "package.json",
        "822486c3f526033cfa7e628d2725e1ee968850b281194496ef733dd6b1d9096d",
        3_620,
    ),
    (
        "package.json",
        "d93faca38a6da90246cddcb64eaf2ec7537a1dd3973f74034fd33e6c667ceca7",
        2_542,
    ),
];

/// One restart-configured regular file in the immutable TypeScript runtime closure.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTypeScriptFileV1 {
    /// Absolute normalized file path selected only by trusted launcher configuration.
    pub path: PathBuf,
    /// Complete hexadecimal BLAKE3 digest of the accepted bytes.
    pub blake3: String,
    /// Exact accepted byte length, bounded and rechecked with the digest.
    pub bytes: u64,
}

impl AcceptedTypeScriptFileV1 {
    /// Rejects malformed paths, digests, and files above the TypeScript bundle member ceiling.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.path)
            || self.blake3.len() != 64
            || !self.blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.bytes > 64 * 1024 * 1024
        {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }

    /// Converts the validated launcher identity into Intelligence's immutable bundle member.
    fn bundle_file(&self) -> Result<TypeScriptBundleFileV1, LauncherError> {
        Ok(TypeScriptBundleFileV1 {
            path: self.path.clone(),
            blake3: blake3::Hash::from_hex(&self.blake3).map_err(|_| LauncherError::Rejected)?,
            bytes: self.bytes,
        })
    }
}

/// Closed TypeScript-specific portion of one fourth launcher provider declaration.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTypeScriptBundleV1 {
    /// Exact byte length of `ProviderLaunch::executable`, the bridge entry module.
    pub bridge_bytes: u64,
    /// Exact accepted TypeScript Language Server release identity.
    pub bridge_version: String,
    /// Explicit accepted `tsserver.js` entry module.
    pub tsserver: AcceptedTypeScriptFileV1,
    /// Exact accepted TypeScript release identity.
    pub typescript_version: String,
    /// Strictly sorted complete loaded runtime closure excluding bridge and `tsserver.js`.
    pub closure: Vec<AcceptedTypeScriptFileV1>,
    /// Exact compiled Codex macOS release record required to enable this provider for Codex.
    pub codex_macos_evidence: String,
    /// Optional separate Claude macOS record; `None` keeps the provider unavailable to Claude.
    pub claude_macos_evidence: Option<String>,
}

impl AcceptedTypeScriptBundleV1 {
    /// Validates the bounded closed declaration without granting either host execution authority.
    fn validate(&self) -> Result<(), LauncherError> {
        if self.bridge_bytes > 64 * 1024 * 1024
            || !identifier(&self.bridge_version)
            || !identifier(&self.typescript_version)
            || self.closure.is_empty()
            || self.closure.len() > 64
            || !identifier(&self.codex_macos_evidence)
            || self
                .claude_macos_evidence
                .as_deref()
                .is_some_and(|evidence| !identifier(evidence))
        {
            return Err(LauncherError::Rejected);
        }
        self.tsserver.validate()?;
        let mut previous: Option<&Path> = None;
        for file in &self.closure {
            file.validate()?;
            if previous.is_some_and(|path| path >= file.path.as_path())
                || file.path == self.tsserver.path
            {
                return Err(LauncherError::Rejected);
            }
            previous = Some(&file.path);
        }
        Ok(())
    }
}

/// TypeScript declaration fields beyond the common ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeScriptLaunchOptions {
    /// Absolute operator-declared Node executable, the only program permitted to start the
    /// bridge. Its measured identity must match the declaration's `toolchain`.
    pub node: Option<AcceptedExecutable>,
    /// Immutable TypeScript closure and host-specific release records.
    pub typescript: Option<AcceptedTypeScriptBundleV1>,
}

/// TypeScript-specific queries on a provider declaration: the compiled release records it must
/// match and the immutable bundle it declares.
pub trait TypeScriptLaunch {
    /// Derives the only Codex macOS record accepted for this exact declared TypeScript bundle, or
    /// `None` for any other provider, release, malformed bundle, or absent Node. Performs no
    /// filesystem read and grants no execution authority.
    fn expected_typescript_codex_macos_evidence(&self) -> Option<String>;

    /// Reconstructs the exact immutable TypeScript bundle from this validated declaration;
    /// `Rejected` for a missing or malformed declaration, `ExecutableChanged` when the declared
    /// files no longer measure as accepted.
    fn typescript_bundle(&self) -> Result<TypeScriptProviderBundleV1, LauncherError>;

    /// Returns whether the compiled Codex release record accepts this exact declaration.
    fn typescript_codex_accepted(&self) -> bool;

    /// Derives the Claude record for the same exact bundle already bound by the Codex digest, or
    /// `None` when the declaration has no accepted Codex record.
    fn expected_typescript_claude_macos_evidence(&self) -> Option<String>;

    /// Returns whether the independent Claude release record accepts this exact declaration.
    fn typescript_claude_accepted(&self) -> bool;
}

impl TypeScriptLaunch for ProviderLaunch {
    /// Derives the only Codex macOS record accepted for this exact declared TypeScript bundle.
    ///
    /// The public release prefix is accepted only with Node 24.4.0, TypeScript Language Server
    /// 6.0.0, TypeScript 5.9.3, the compiled accepted Node/bridge/tsserver/closure byte identities,
    /// an exact `tsserver.js` basename, and a BLAKE3 suffix over every declared path, digest, byte
    /// length, and identity. Returns `None` for any other provider kind, release, malformed bundle,
    /// or absent Node. This performs no filesystem read and grants no execution authority; startup
    /// and pre-spawn remeasurement remain separate mandatory checks.
    fn expected_typescript_codex_macos_evidence(&self) -> Option<String> {
        let options = self.options::<TypeScriptLaunchOptions>()?;
        let node = options.node.as_ref()?;
        let bundle = options.typescript.as_ref()?;
        if self.language != crate::LANGUAGE
            || node.validate().is_err()
            || self.executable.validate().is_err()
            || bundle.validate().is_err()
            || self.toolchain != TYPESCRIPT_NODE_VERSION_V1
            || node.identity != TYPESCRIPT_NODE_VERSION_V1
            || !node.blake3.eq_ignore_ascii_case(TYPESCRIPT_NODE_BLAKE3_V1)
            || self.executable.identity != TYPESCRIPT_BRIDGE_VERSION_V1
            || !self
                .executable
                .blake3
                .eq_ignore_ascii_case(TYPESCRIPT_BRIDGE_BLAKE3_V1)
            || bundle.bridge_bytes != TYPESCRIPT_BRIDGE_BYTES_V1
            || bundle.bridge_version != TYPESCRIPT_BRIDGE_VERSION_V1
            || bundle.typescript_version != TYPESCRIPT_VERSION_V1
            || !bundle
                .tsserver
                .blake3
                .eq_ignore_ascii_case(TYPESCRIPT_TSSERVER_BLAKE3_V1)
            || bundle.tsserver.bytes != TYPESCRIPT_TSSERVER_BYTES_V1
            || bundle
                .tsserver
                .path
                .file_name()
                .and_then(|name| name.to_str())
                != Some("tsserver.js")
            || bundle.closure.len() != TYPESCRIPT_CLOSURE_V1.len()
            || bundle.closure.iter().zip(TYPESCRIPT_CLOSURE_V1).any(
                |(file, (name, digest, bytes))| {
                    file.path.file_name().and_then(|value| value.to_str()) != Some(name)
                        || !file.blake3.eq_ignore_ascii_case(digest)
                        || file.bytes != bytes
                },
            )
        {
            return None;
        }
        let mut hash = blake3::Hasher::new();
        evidence_frame(&mut hash, b"typescript-codex-macos-bundle-v1");
        for value in [
            self.toolchain.as_bytes(),
            node.path.as_os_str().as_encoded_bytes(),
            node.identity.as_bytes(),
            node.blake3.as_bytes(),
            self.executable.path.as_os_str().as_encoded_bytes(),
            self.executable.identity.as_bytes(),
            self.executable.blake3.as_bytes(),
            bundle.bridge_version.as_bytes(),
            bundle.tsserver.path.as_os_str().as_encoded_bytes(),
            bundle.tsserver.blake3.as_bytes(),
            bundle.typescript_version.as_bytes(),
        ] {
            evidence_frame(&mut hash, value);
        }
        evidence_frame(&mut hash, &bundle.bridge_bytes.to_le_bytes());
        evidence_frame(&mut hash, &bundle.tsserver.bytes.to_le_bytes());
        for file in &bundle.closure {
            evidence_frame(&mut hash, file.path.as_os_str().as_encoded_bytes());
            evidence_frame(&mut hash, file.blake3.as_bytes());
            evidence_frame(&mut hash, &file.bytes.to_le_bytes());
        }
        Some(format!(
            "{TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1}:{}",
            hash.finalize().to_hex()
        ))
    }

    /// Reconstructs the exact immutable TypeScript bundle from this validated launcher provider.
    fn typescript_bundle(&self) -> Result<TypeScriptProviderBundleV1, LauncherError> {
        let options = self
            .options::<TypeScriptLaunchOptions>()
            .ok_or(LauncherError::Rejected)?;
        let configured = options.typescript.as_ref().ok_or(LauncherError::Rejected)?;
        let node = options.node.as_ref().ok_or(LauncherError::Rejected)?;
        TypeScriptProviderBundleV1::new(TypeScriptProviderBundleV1Identity {
            node: node.path.clone(),
            node_blake3: blake3::Hash::from_hex(&node.blake3)
                .map_err(|_| LauncherError::Rejected)?,
            node_version: node.identity.clone(),
            bridge: TypeScriptBundleFileV1 {
                path: self.executable.path.clone(),
                blake3: blake3::Hash::from_hex(&self.executable.blake3)
                    .map_err(|_| LauncherError::Rejected)?,
                bytes: configured.bridge_bytes,
            },
            bridge_version: configured.bridge_version.clone(),
            tsserver: configured.tsserver.bundle_file()?,
            typescript_version: configured.typescript_version.clone(),
            closure: configured
                .closure
                .iter()
                .map(AcceptedTypeScriptFileV1::bundle_file)
                .collect::<Result<Vec<_>, _>>()?,
        })
        .map_err(|_| LauncherError::ExecutableChanged)
    }

    /// Returns whether the compiled Codex release record accepts this exact provider declaration.
    fn typescript_codex_accepted(&self) -> bool {
        self.expected_typescript_codex_macos_evidence()
            .is_some_and(|expected| {
                self.options::<TypeScriptLaunchOptions>()
                    .and_then(|options| options.typescript.as_ref())
                    .is_some_and(|bundle| bundle.codex_macos_evidence == expected)
            })
    }

    /// Derives the Claude record for the same exact bundle already bound by the Codex digest.
    ///
    /// The distinct prefix names the independently exercised Claude host cell. Reusing the exact
    /// bundle suffix keeps both records bound to identical declared paths, bytes, and releases;
    /// malformed or non-Codex-accepted declarations return `None` without filesystem I/O.
    fn expected_typescript_claude_macos_evidence(&self) -> Option<String> {
        let codex = self.expected_typescript_codex_macos_evidence()?;
        let (_, bundle_digest) = codex.rsplit_once(':')?;
        Some(format!(
            "{TYPESCRIPT_CLAUDE_MACOS_EVIDENCE_V1}:{bundle_digest}"
        ))
    }

    /// Returns whether the independent Claude release record accepts this exact declaration.
    fn typescript_claude_accepted(&self) -> bool {
        self.expected_typescript_claude_macos_evidence()
            .is_some_and(|expected| {
                self.options::<TypeScriptLaunchOptions>()
                    .and_then(|options| options.typescript.as_ref())
                    .is_some_and(|bundle| {
                        bundle.claude_macos_evidence.as_deref() == Some(expected.as_str())
                    })
            })
    }
}

/// Appends one unambiguous raw field to the declared TypeScript evidence digest.
fn evidence_frame(hash: &mut blake3::Hasher, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

/// The TypeScript language server bridge integration.
pub struct TypeScriptServer;

impl LanguageServer for TypeScriptServer {
    /// The TypeScript language (JavaScript files included).
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    /// TypeScript defaults, versioned.
    fn settings_key(&self) -> &'static str {
        "typescript_defaults_v1"
    }

    /// The Node executable and the TypeScript bundle declaration.
    fn option_fields(&self) -> &'static [&'static str] {
        &["node", "typescript"]
    }

    /// Decodes [`TypeScriptLaunchOptions`].
    fn parse_options(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn Any + Send + Sync>, serde_json::Error> {
        let options: TypeScriptLaunchOptions =
            serde_json::from_value(serde_json::Value::Object(fields))?;
        Ok(Arc::new(options))
    }

    /// Requires a valid Node whose identity is the toolchain, a valid bundle whose bridge version
    /// is the executable identity, an accepted Codex record, and — when declared — an accepted
    /// Claude record.
    fn validate_launch(&self, launch: &ProviderLaunch) -> bool {
        let Some(options) = launch.options::<TypeScriptLaunchOptions>() else {
            return false;
        };
        options
            .node
            .as_ref()
            .is_some_and(|node| node.validate().is_ok() && launch.toolchain == node.identity)
            && options.typescript.as_ref().is_some_and(|bundle| {
                bundle.validate().is_ok() && launch.executable.identity == bundle.bridge_version
            })
            && launch.typescript_codex_accepted()
            && !options.typescript.as_ref().is_some_and(|bundle| {
                bundle.claude_macos_evidence.is_some() && !launch.typescript_claude_accepted()
            })
    }

    /// The declared Node executable.
    fn launch_executables<'a>(&self, launch: &'a ProviderLaunch) -> Vec<&'a AcceptedExecutable> {
        launch
            .options::<TypeScriptLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .into_iter()
            .collect()
    }

    /// Reconstructs (and so re-measures) the declared bundle, unless startup was cancelled.
    fn verify_launch(
        &self,
        launch: &ProviderLaunch,
        cancel: &AtomicBool,
    ) -> Result<(), LauncherError> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(LauncherError::Cancelled);
        }
        launch.typescript_bundle()?;
        Ok(())
    }

    /// The bridge and `tsserver.js` (both run by Node) and Node itself.
    fn toolchain_programs(&self, launch: &ProviderLaunch) -> Vec<(PathBuf, Option<PathBuf>)> {
        let options = launch.options::<TypeScriptLaunchOptions>();
        let node = options
            .and_then(|options| options.node.as_ref())
            .map(|node| node.path.clone());
        let mut programs = vec![(launch.executable.path.clone(), node.clone())];
        programs.extend(node.clone().map(|node| (node, None)));
        programs.extend(
            options
                .and_then(|options| options.typescript.as_ref())
                .map(|bundle| (bundle.tsserver.path.clone(), node)),
        );
        programs
    }

    /// The bridge's own name.
    fn name(&self) -> &'static str {
        "typescript-language-server"
    }

    /// TypeScript defaults, versioned.
    fn cache_settings(&self) -> &'static str {
        "typescript-defaults-v1"
    }

    /// TypeScript defaults, versioned.
    fn effective_configuration(&self) -> &'static str {
        "typescript-defaults-v1"
    }

    /// Only a private temporary directory.
    fn cache_directories(&self) -> &'static [&'static str] {
        &["tmp"]
    }

    /// `.js`, `.jsx`, `.ts` and `.tsx` sources.
    fn context_extensions(&self) -> &'static [&'static str] {
        &["js", "jsx", "ts", "tsx"]
    }

    /// Symbol tools use the live bridge session for every JavaScript/TypeScript module extension.
    fn session_extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"]
    }

    /// Starts with no sessions and an empty quarantine.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(TypeScriptBackend::default())
    }
}

/// One binding's live bridge: child, exclusive view, the project inputs that selected it and the
/// session driver.
struct TypeScriptLive {
    /// Bridge process owned until reap.
    child: TypeScriptProtocolChild,
    /// Exclusive view admitted for this child.
    view: TypeScriptView,
    /// Resolution inputs observed when the session started.
    inputs: Box<ProjectResolutionInputsV1>,
    /// Transport driver and synchronized document state.
    live: LiveSession,
}

/// Exclusive TypeScript generations, the owner-lifetime profile quarantine and every binding's
/// retained bridge session.
#[derive(Default)]
struct TypeScriptBackend {
    /// Exclusive TypeScript generations and owner-lifetime exact-profile quarantine.
    profiles: TypeScriptProfiles,
    /// Live sessions, one per binding, kept until stop, project change or transport failure.
    live: BTreeMap<BindingRef, TypeScriptLive>,
}

/// Observes bounded TypeScript config and package files away from the single worker thread.
///
/// `job` receives the refusal text as its failure detail when observation rejects the document, so
/// the `resolution_unverified` reply can name the tsconfig consulted and the reason. `document`
/// is the absolute source path; `bundle` and `roots` are moved into the blocking task. A join
/// failure maps to `Internal`, a rejection to `ResolutionUnverified`.
async fn observe_typescript_inputs(
    job: &mut dyn ProviderJob,
    worktree: WorktreeRef,
    document: std::path::PathBuf,
    bundle: TypeScriptProviderBundleV1,
    roots: Vec<std::path::PathBuf>,
) -> Result<ProjectResolutionInputsV1, FailureCode> {
    let observed = tokio::task::spawn_blocking(move || {
        let worktree_root = worktree.worktree_path().to_path_buf();
        let path_proof = |path: &Path| {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                worktree_root.join(path)
            };
            agent_ide_core::assistance::launcher::admit_path(&roots, &absolute).is_ok()
        };
        ProjectResolutionInputsV1::observe(worktree, document, &bundle, &path_proof)
    })
    .await
    .map_err(|_| FailureCode::Internal)?;
    observed.map_err(|rejection| {
        job.set_failure_detail(rejection.to_string());
        FailureCode::ResolutionUnverified
    })
}

impl TypeScriptBackend {
    /// Starts the accepted TypeScript session for this binding or reuses its project session.
    ///
    /// `job` supplies cancellation and binding ownership; `launch` provides the accepted bundle;
    /// `source` selects and verifies project inputs. A different worktree, bundle, or captured
    /// project file set shuts down the old session before a new one is admitted. Unverified inputs,
    /// authority, capacity, spawn, handshake, and cancellation failures return a bounded code.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        if !launch.typescript_codex_accepted() {
            return Err(FailureCode::ExecutionProfile);
        }
        let binding = job.binding().clone();
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = host.allowed_roots();
        let inputs = observe_typescript_inputs(
            job,
            authority.worktree().clone(),
            authority.worktree().worktree_path().join(source.path()),
            bundle.clone(),
            roots.clone(),
        )
        .await?;
        let worktree_root = authority.worktree().worktree_path().to_path_buf();
        let path_proof = |path: &Path| {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                worktree_root.join(path)
            };
            agent_ide_core::assistance::launcher::admit_path(&roots, &absolute).is_ok()
        };
        if self
            .live
            .get(&binding)
            .is_some_and(|entry| entry.inputs.same_project(&inputs) && entry.live.is_alive())
        {
            return Ok(());
        }
        self.release(host, &binding).await;
        let profile = match TypeScriptProfile::new(
            bundle,
            inputs.clone(),
            launch.trust.clone(),
            Path::new(&cache_namespace).to_path_buf(),
        ) {
            Ok(profile) => profile,
            Err(TypeScriptProfileError::InvalidResolution) => {
                job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                return Err(FailureCode::ResolutionUnverified);
            }
            Err(_) => return Err(FailureCode::ExecutionProfile),
        };
        let worktree = TypeScriptWorktree::new(
            authority.worktree().clone(),
            server::execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = match profile.command(&worktree, &path_proof) {
            Ok(command) => command,
            Err(TypeScriptProfileError::InvalidResolution) => {
                job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                return Err(FailureCode::ResolutionUnverified);
            }
            Err(_) => return Err(FailureCode::ExecutionProfile),
        };
        let node = launch
            .options::<TypeScriptLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .ok_or(FailureCode::ExecutionProfile)?;
        let request = host
            .execution_request(&*job, &authority, command, node)
            .await?;
        let active = host.active(&binding)?;
        let view = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.profiles.request(
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                server::owner(&binding)?,
                AdmissionClass::Interactive,
            ) {
                TypeScriptViewAdmission::Granted(view) => view,
                TypeScriptViewAdmission::Queued(ticket) => {
                    host.registry().cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Quarantined) => {
                    return Err(FailureCode::ProviderUnavailable);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let output_bytes = host.output_bytes();
        let child = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match TypeScriptProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                view.lease(),
                Some(active),
                output_bytes,
                &path_proof,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let _ = self.profiles.release(view, host.registry());
                    let failure = if matches!(&error, TypeScriptProfileError::InvalidResolution) {
                        job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                        FailureCode::ResolutionUnverified
                    } else {
                        FailureCode::ProviderUnavailable
                    };
                    if let TypeScriptProfileError::Process(error) = error {
                        host.spawn_failure(error, &binding);
                    }
                    return Err(failure);
                }
            }
        };
        let mut child = child;
        let generation = view.generation();
        let opened = match child.take_pipes() {
            Some((stdin, stdout)) => {
                let open = LiveSession::open(
                    stdout,
                    stdin,
                    source.worktree().clone(),
                    source.authority_epoch(),
                    ViewGeneration {
                        backend: generation,
                        configuration: 1,
                        toolchain: 1,
                        view: generation,
                    },
                    ProviderSettings::new(profile.clone()),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.live.insert(
                    binding,
                    TypeScriptLive {
                        child,
                        view,
                        inputs: Box::new(inputs),
                        live,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap(host, &binding, child, view, false).await;
                if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Answers TypeScript context requests through the binding's persistent server session.
    ///
    /// The exchange always waits (bounded) for the diagnostics push. A failed or cancelled
    /// exchange retires the session. The result is then checked against the session's project
    /// snapshot and answers `ResolutionUnverified` (with the failure detail set) when the project
    /// changed.
    async fn answer(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.binding().clone();
        self.ensure(host, job, launch, source).await?;
        let (result, diagnostics) = {
            let entry = self.live.get_mut(&binding).ok_or(FailureCode::Internal)?;
            server::exchange_context(&mut entry.live, job, source, bytes, query, true).await
        };
        let context = match result {
            Ok(context) => context,
            Err(_) => {
                self.release(host, &binding).await;
                return if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                };
            }
        };
        let outcome = ProviderContext {
            context,
            diagnostics,
        };
        if !self
            .verify_inputs(host, job, launch, &binding, source)
            .await?
        {
            self.release(host, &binding).await;
            return Err(FailureCode::ResolutionUnverified);
        }
        server::record_provider(
            &*host,
            crate::LANGUAGE,
            server::diagnostic_state(outcome.diagnostics.readiness),
        );
        host.active(&binding)?;
        Ok(outcome)
    }

    /// Re-observes project files after a TypeScript request and checks them against the retained
    /// session's project snapshot.
    ///
    /// `launch` supplies the accepted TypeScript bundle, `binding` selects the live snapshot, and
    /// `source` identifies the requested document. Returns `false` (and sets the job's failure
    /// detail) when project evidence changed; unobservable or invalid evidence returns
    /// `ResolutionUnverified`.
    async fn verify_inputs(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        binding: &BindingRef,
        source: &SourceObservation,
    ) -> Result<bool, FailureCode> {
        let authority = host.authority(binding).await?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = host.allowed_roots();
        let current = observe_typescript_inputs(
            job,
            authority.worktree().clone(),
            authority.worktree().worktree_path().join(source.path()),
            bundle,
            roots,
        )
        .await?;
        let matches = self
            .live
            .get(binding)
            .is_some_and(|entry| entry.inputs.same_project(&current));
        if !matches {
            job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
        }
        Ok(matches)
    }

    /// Shuts down and reaps `binding`'s bridge session, if any.
    async fn release(&mut self, host: &mut dyn ProviderHost, binding: &BindingRef) {
        if let Some(TypeScriptLive {
            child, view, live, ..
        }) = self.live.remove(binding)
        {
            let shutdown_completed = live.shutdown().await;
            self.reap(host, binding, child, view, shutdown_completed)
                .await;
        }
    }

    /// Reaps a TypeScript bridge and settles its exact view.
    ///
    /// `shutdown_completed` reports whether the LSP shutdown exchange was answered while the session
    /// was alive. Only then is the child given one second to exit on its own, and only an observed
    /// nonzero exit quarantines the profile key. A shutdown that never completed (client-initiated
    /// teardown after invalidation, a hung server, a failed handshake) and a graceful wait that
    /// times out go straight to abnormal termination without quarantining, because neither proves
    /// the bridge itself is broken. A reap that cannot settle marks the binding uncertain.
    async fn reap(
        &mut self,
        host: &mut dyn ProviderHost,
        binding: &BindingRef,
        mut child: TypeScriptProtocolChild,
        view: TypeScriptView,
        shutdown_completed: bool,
    ) {
        let result = if shutdown_completed {
            match child.wait_for_exit(Duration::from_secs(1)).await {
                Ok(waited) => {
                    self.profiles
                        .quarantine_after_unsuccessful_wait(&view, &waited);
                    child.finish_reap(waited, Duration::from_millis(500)).await
                }
                Err(_) => {
                    child
                        .terminate_abnormally(
                            Duration::from_millis(100),
                            Duration::from_millis(500),
                        )
                        .await
                }
            }
        } else {
            child
                .terminate_abnormally(Duration::from_millis(100), Duration::from_millis(500))
                .await
        };
        match result {
            Ok(reaped) => {
                if let Ok(capability) = self.profiles.release(view, host.registry()) {
                    let admission = host.admission();
                    let mut admission = admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = host
                        .registry()
                        .complete_reap(&mut admission, capability, reaped.proof);
                }
            }
            Err(_) => {
                host.mark_uncertain(binding);
                let _ = self.profiles.release(view, host.registry());
            }
        }
    }
}

impl ServerBackend for TypeScriptBackend {
    /// Serves context from the binding's bridge session (see [`TypeScriptBackend::answer`]).
    fn context<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
        bytes: &'a [u8],
        query: ContextQuery,
    ) -> BoxFuture<'a, Result<ProviderContext, FailureCode>> {
        Box::pin(self.answer(host, job, launch, source, bytes, query))
    }

    /// Starts or keeps the binding's bridge session (see [`TypeScriptBackend::ensure`]).
    fn ensure_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        Box::pin(self.ensure(host, job, launch, source))
    }

    /// Returns the binding's retained bridge session.
    fn live_session(&mut self, binding: &BindingRef) -> Option<&mut LiveSession> {
        self.live.get_mut(binding).map(|entry| &mut entry.live)
    }

    /// Shuts down and reaps the binding's bridge session.
    fn release_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.release(host, binding))
    }

    /// Bindings with a retained bridge session.
    fn live_bindings(&self) -> Vec<BindingRef> {
        self.live.keys().cloned().collect()
    }
}
