//! Claude foreground-helper correlation: ticket minting, exact launch recognition, one-use claim.
//!
//! Claude has no Codex sandbox metadata and no long-lived daemon-side execution path. A Claude
//! operation therefore runs inside a short foreground `Bash` helper that inherits the host's own
//! native sandbox. This module owns only the correlation and admission mechanics for that helper:
//!
//! * the daemon mints a `LaunchTicket` bound to one action-scoped `detail_ref` and returns the
//!   exact helper command in the ordinary bounded reply text;
//! * the ordinary `Bash` `PreToolUse` hook recognizes that command by comparing it, byte-exact or
//!   as the same POSIX words under equivalent quoting, against the daemon-stored expected command
//!   (`LaunchLedger::recognize`) and stays silent, so the host's own permission and sandbox
//!   evaluation of the unchanged command is what actually decides;
//! * the helper claims the bound operation exactly once over the private socket
//!   (`LaunchLedger::claim`) and receives one closed daemon-selected `HelperJob`.
//!
//! Nothing here performs Git, source, provider or process effects, and nothing here is sandbox
//! attestation. A ticket establishes correlation and replay exclusion only; the authority contract
//! is the operator-managed strict Claude configuration declared by `ClaudeOperatorProfile`.

use super::host_binding::BindingRef;
use super::reply::FailureCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Maximum complete serialized helper frame in either direction, before JSON decoding.
///
/// Sized to carry one complete `MAX_RESULT_TEXT_BYTES` result even in the worst case where every
/// byte is a control character that JSON escapes to six bytes (`\u00XX`), plus the bounded
/// discovery/source fields alongside it (T16B).
pub const MAX_HELPER_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Only this closed helper wire revision is accepted in either direction.
pub const HELPER_PROTOCOL: u32 = 3;
/// Bounds concurrently outstanding launch tickets within one daemon boot.
pub const MAX_TICKETS: usize = 64;
/// Maximum accepted length of one exact expected helper command.
const MAX_COMMAND_BYTES: usize = 4096;
/// Maximum accepted length of any single identity field carried on the helper wire.
const MAX_IDENTIFIER_BYTES: usize = 256;
/// Maximum bounded owner text one helper result may carry back to the daemon.
///
/// One capture must be large enough to hold a full bounded Context or Diff render for the daemon
/// to page through on later `ide.inspect` calls, rather than repeatedly re-launching the Claude
/// foreground helper (T13B). A source up to `MAX_SOURCE_BYTES` (1 MiB) must fit whole beside its
/// fixed header, so this is that ceiling plus 256 KiB of header room (T16B).
pub(super) const MAX_RESULT_TEXT_BYTES: usize = 1024 * 1024 + 256 * 1024;
/// Maximum raw stdout bytes one reported discovery stream may carry.
///
/// `worktree list --porcelain -z` grows with every worktree (~200 bytes each); 8 KiB refused a
/// repository with ~50 worktrees and the helper then dropped its whole result (T39G). Bytes travel
/// JSON-encoded (up to ~4x), so six frames at this cap still fit one `MAX_HELPER_FRAME_BYTES` frame.
// ponytail: fixed cap (~600 worktrees); stream or trim the list if a repository ever exceeds it.
const MAX_DISCOVERY_STDOUT_BYTES: usize = 128 * 1024;
/// Maximum raw stderr bytes one reported discovery stream may carry.
const MAX_DISCOVERY_STDERR_BYTES: usize = 16 * 1024;
/// Fixed helper subcommand; the model never selects an executable, argument or shell fragment.
const HELPER_SUBCOMMAND: &str = "claude-worker";

/// Renders one daemon-selected UTF-8 argument as a POSIX shell word.
///
/// A nonempty value made up only of characters that never require shell quoting (`[A-Za-z0-9]`
/// and `_./:=@%+,-`) is emitted bare; every other value is single-quoted, with single quotes
/// closed, emitted through a quoted backslash escape, then reopened. The caller measures the
/// expanded command afterward, so escaping cannot bypass the command byte ceiling.
fn shell_word(value: &str) -> String {
    let is_bare = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c));
    if is_bare {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Splits one raw observed shell command into POSIX argv words, conservatively.
///
/// Accepts unquoted bare words separated by spaces or tabs, single-quoted strings (no escapes
/// recognized inside), and double-quoted strings where only `\"` and `\\` are valid escapes.
/// Returns `None` — never an approximate parse — on an unterminated quote, on `$` or a backtick
/// inside double quotes, or on any of the shell metacharacters
/// `; & | < > ( ) $ \` \n * ? [ ] { } ~ # !` or a backslash appearing outside quotes. This is
/// intentionally narrower than a real shell grammar: anything it cannot parse with full
/// confidence is rejected rather than guessed at.
fn helper_words(command: &str) -> Option<Vec<String>> {
    const REJECTED_OUTSIDE_QUOTES: &[char] = &[
        ';', '&', '|', '<', '>', '(', ')', '$', '`', '\n', '*', '?', '[', ']', '{', '}', '~', '#',
        '!', '\\',
    ];
    fn is_separator(c: char) -> bool {
        c == ' ' || c == '\t'
    }
    let mut words = Vec::new();
    let mut chars = command.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(&c) if is_separator(c)) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        loop {
            match chars.peek().copied() {
                None => break,
                Some(c) if is_separator(c) => break,
                Some('\'') => {
                    chars.next();
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(c) => word.push(c),
                            None => return None,
                        }
                    }
                }
                Some('"') => {
                    chars.next();
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some('\\') => match chars.next() {
                                Some('"') => word.push('"'),
                                Some('\\') => word.push('\\'),
                                _ => return None,
                            },
                            Some('$') | Some('`') => return None,
                            Some(c) => word.push(c),
                            None => return None,
                        }
                    }
                }
                Some(c) if REJECTED_OUTSIDE_QUOTES.contains(&c) => return None,
                Some(c) => {
                    word.push(c);
                    chars.next();
                }
            }
        }
        words.push(word);
    }
    Some(words)
}

/// Declares the operator-managed strict Claude configuration this profile is accepted under.
///
/// The daemon never inspects, guesses or mutates live host settings. An operator states the
/// closed facts below in launcher configuration; an absent or mismatched profile leaves the
/// Claude execution path unavailable rather than silently degrading to unrestricted execution.
///
/// Every field is an **operator assertion**, not a measurement. The daemon does not observe the
/// containment a helper or its children actually run under, and neither the ticket nor the private
/// claim socket attests it: correlation and replay exclusion are all they establish. Accordingly no
/// part of this path may report that the daemon observed inherited containment. Local controlled
/// process fixtures likewise prove wiring and settlement only; real host containment acceptance is
/// a separate live exercise against a real Claude host and is never implied by a passing fixture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeOperatorProfile {
    /// Operator states the host sandbox is enabled for this account and project.
    pub enabled: bool,
    /// Operator states the host refuses to run a command when sandboxing is unavailable.
    pub fail_if_unavailable: bool,
    /// Operator states unsandboxed command escape is not permitted; `true` is never accepted.
    pub allow_unsandboxed_commands: bool,
    /// Operator states no `excludedCommands` entry matches the fixed helper command.
    pub no_matching_excluded_commands: bool,
    /// Operator states the read/write/network/socket scope matches the documented helper needs.
    pub scope_declared: bool,
    /// Optional explicit read grants; absence preserves the strict profile's declared project read.
    #[serde(default)]
    pub read_roots: Option<Vec<PathBuf>>,
    /// Absolute path or glob exclusions from those grants; any possible worktree overlap restricts checks.
    #[serde(default)]
    pub read_denies: Vec<String>,
    /// Host platform this evidence was accepted on; only macOS is currently supported.
    pub platform: HelperPlatform,
}

/// Names the host platforms for which an operator profile can currently be accepted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperPlatform {
    /// macOS Seatbelt-backed Claude sandboxing, the only proven configuration.
    MacOs,
    /// Linux remains unavailable pending equivalent proof of the same guarantees.
    Linux,
}

impl ClaudeOperatorProfile {
    /// Accepts only a complete strict macOS profile; every other shape is unavailable.
    ///
    /// Failure is reported as [`FailureCode::ExecutionProfile`] so a missing or weakened operator
    /// declaration is never confused with a runtime provider or authority failure. Read paths
    /// must be absolute and lexically clean; a relative or parent-traversing exclusion is refused.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let clean = |path: &str| {
            path.starts_with('/')
                && !path.contains('\0')
                && (path == "/"
                    || !path[1..]
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == ".."))
        };
        let strict = self.enabled
            && self.fail_if_unavailable
            && !self.allow_unsandboxed_commands
            && self.no_matching_excluded_commands
            && self.scope_declared
            && self.platform == HelperPlatform::MacOs
            && self
                .read_roots
                .as_ref()
                .is_none_or(|roots| roots.iter().all(|root| root.to_str().is_some_and(clean)))
            && self.read_denies.iter().all(|deny| clean(deny));
        strict.then_some(()).ok_or(FailureCode::ExecutionProfile)
    }

    /// Returns the canonical helper cache-rights identity for the accepted strict profile.
    ///
    /// Validation must succeed first. The value names the fixed helper execution contract rather
    /// than model or hook input; check-only read declarations do not authorize helper caches.
    pub fn rights_identity(&self) -> Result<&'static str, FailureCode> {
        self.validate()?;
        Ok("claude-strict-macos-v1")
    }

    /// Proves a target's complete tree readable from the accepted operator assertion.
    /// Explicit grants must contain the tree, and any deny whose literal prefix can overlap it
    /// refuses the proof. Missing grants mean the legacy `scope_declared` project-wide grant.
    pub fn declares_whole_tree_read(&self, worktree: &Path) -> bool {
        self.validate().is_ok()
            && self
                .read_roots
                .as_ref()
                .is_none_or(|roots| roots.iter().any(|root| worktree.starts_with(root)))
            && self.read_denies.iter().all(|deny| {
                let literal = deny
                    .find(['*', '?', '[', '{', '!', '\\'])
                    .map_or(deny.as_str(), |end| &deny[..end]);
                let prefix = if literal.len() == deny.len() {
                    Path::new(literal)
                } else if literal.ends_with('/') && literal != "/" {
                    Path::new(literal.trim_end_matches('/'))
                } else {
                    Path::new(literal).parent().unwrap_or(Path::new("/"))
                };
                !worktree.starts_with(prefix) && !prefix.starts_with(worktree)
            })
    }
}

/// Names the closed operations a Claude helper may be launched for.
///
/// Inspect is deliberately absent: retrieval of an already retained result is same-binding daemon
/// bookkeeping and must never launch a helper or read source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperOperation {
    /// Discovery plus canonical durable Workspace activation for one candidate.
    Start,
    /// Semantic context over currently registered source bytes.
    Context,
    /// Composed comparison evidence for the current Workspace scope.
    Diff,
    /// One full-content descriptor-safe edit performed only inside the claimed foreground helper.
    Edit,
}

/// Names the per-operation exclusive language profile a helper may run.
///
/// A single helper never runs both. Shared multi-worktree gopls is unavailable for Claude because
/// this foreground lifecycle cannot retain a safe shared listener across operations; the existing
/// Codex shared-listener implementation continues to cover that matrix cell unchanged.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperLanguage {
    /// gopls on a helper-private, non-shared logical view.
    Go,
    /// rust-analyzer with cache priming and proc-macro expansion both disabled.
    Rust,
    /// Pyright over one helper-private, worktree-isolated stdio child.
    Python,
    /// Release-pinned TypeScript Language Server for JS, JSX, TS, or TSX.
    #[serde(rename = "typescript")]
    TypeScript,
}

/// One accepted regular-file identity in a Claude helper's TypeScript bundle frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperTypeScriptFileV1 {
    /// Absolute normalized file path selected by the daemon from trusted launcher configuration.
    pub path: PathBuf,
    /// Complete hexadecimal BLAKE3 digest of the accepted bytes.
    pub blake3: String,
    /// Exact accepted byte length.
    pub bytes: u64,
}

impl HelperTypeScriptFileV1 {
    /// Converts one structurally valid wire identity into Intelligence's immutable bundle member.
    ///
    /// This performs no file I/O: launcher startup already measured the accepted bytes, and the
    /// helper reconstructs the complete bundle immediately before spawn to remeasure them. A
    /// relative, non-normal path, malformed digest, or oversized declared file is rejected here.
    fn bundle_file(
        &self,
    ) -> Result<crate::intelligence::typescript::TypeScriptBundleFileV1, FailureCode> {
        if !self.path.is_absolute()
            || !self.path.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || self.bytes > 64 * 1024 * 1024
        {
            return Err(FailureCode::ExecutionProfile);
        }
        Ok(crate::intelligence::typescript::TypeScriptBundleFileV1 {
            path: self.path.clone(),
            blake3: blake3::Hash::from_hex(&self.blake3)
                .map_err(|_| FailureCode::ExecutionProfile)?,
            bytes: self.bytes,
        })
    }
}

/// Complete daemon-selected TypeScript bundle identity carried to one foreground helper.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperTypeScriptProfileV1 {
    /// Absolute accepted Node executable used as the only launched program.
    pub node: PathBuf,
    /// Accepted Node release identity.
    pub node_version: String,
    /// Complete BLAKE3 digest of the accepted Node executable.
    pub node_blake3: String,
    /// Accepted bridge entry module.
    pub bridge: HelperTypeScriptFileV1,
    /// Accepted TypeScript Language Server release identity.
    pub bridge_version: String,
    /// Explicit accepted `tsserver.js` entry module.
    pub tsserver: HelperTypeScriptFileV1,
    /// Accepted TypeScript release identity.
    pub typescript_version: String,
    /// Strictly sorted complete loaded closure excluding bridge and `tsserver.js`.
    pub closure: Vec<HelperTypeScriptFileV1>,
}

impl HelperTypeScriptProfileV1 {
    /// Reconstructs and validates the immutable bundle before the helper creates a command.
    pub fn bundle(
        &self,
    ) -> Result<crate::intelligence::typescript::TypeScriptProviderBundleV1, FailureCode> {
        let identity = crate::intelligence::typescript::TypeScriptProviderBundleV1Identity {
            node: self.node.clone(),
            node_blake3: blake3::Hash::from_hex(&self.node_blake3)
                .map_err(|_| FailureCode::ExecutionProfile)?,
            node_version: self.node_version.clone(),
            bridge: self.bridge.bundle_file()?,
            bridge_version: self.bridge_version.clone(),
            tsserver: self.tsserver.bundle_file()?,
            typescript_version: self.typescript_version.clone(),
            closure: self
                .closure
                .iter()
                .map(HelperTypeScriptFileV1::bundle_file)
                .collect::<Result<Vec<_>, _>>()?,
        };
        crate::intelligence::typescript::TypeScriptProviderBundleV1::new(identity)
            .map_err(|_| FailureCode::ExecutionProfile)
    }

    /// Rejects malformed bundle identities at the daemon/helper frame boundary without file I/O.
    ///
    /// Startup has already measured the launcher-owned bundle. The receiving helper calls
    /// [`Self::bundle`] immediately before constructing its one-shot command, which performs the
    /// required second byte measurement; avoiding it here keeps ticket minting within MCP ingress.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let identity = |value: &str| {
            !value.is_empty()
                && value.len() <= MAX_IDENTIFIER_BYTES
                && !value.chars().any(char::is_control)
        };
        if !self.node.is_absolute()
            || !self.node.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || blake3::Hash::from_hex(&self.node_blake3).is_err()
            || !identity(&self.node_version)
            || !identity(&self.bridge_version)
            || !identity(&self.typescript_version)
            || self.bridge.bundle_file().is_err()
            || self.tsserver.bundle_file().is_err()
            || self.closure.is_empty()
            || self.closure.len() > 64
        {
            return Err(FailureCode::ExecutionProfile);
        }
        let mut previous: Option<&std::path::Path> = None;
        for file in &self.closure {
            if file.bundle_file().is_err()
                || previous.is_some_and(|path| path >= file.path.as_path())
                || file.path == self.bridge.path
                || file.path == self.tsserver.path
            {
                return Err(FailureCode::ExecutionProfile);
            }
            previous = Some(&file.path);
        }
        Ok(())
    }
}

/// Carries the complete launcher-accepted Pyright and Node identities for one helper child.
///
/// Both paths and byte digests come directly from the restart-loaded launcher configuration. The
/// helper never discovers either program from project files or its inherited environment; it only
/// reconstructs the existing fixed Pyright profile and rechecks these identities before spawning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperPyrightProfile {
    /// Absolute accepted `pyright-langserver` script path.
    pub script: PathBuf,
    /// Nonempty launcher-accepted Pyright script identity.
    pub script_identity: String,
    /// Complete hexadecimal BLAKE3 digest of the accepted script bytes.
    pub script_blake3: String,
    /// Absolute accepted Node program that alone may execute `script`.
    pub node: PathBuf,
    /// Nonempty launcher-accepted Node identity.
    pub node_identity: String,
    /// Complete hexadecimal BLAKE3 digest of the accepted Node bytes.
    pub node_blake3: String,
}

impl HelperPyrightProfile {
    /// Rejects non-absolute paths, blank identities, and malformed accepted byte digests.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let digest =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        (self.script.is_absolute()
            && !self.script_identity.is_empty()
            && digest(&self.script_blake3)
            && self.node.is_absolute()
            && !self.node_identity.is_empty()
            && digest(&self.node_blake3))
        .then_some(())
        .ok_or(FailureCode::ExecutionProfile)
    }
}

/// Carries the effective Rust analyzer settings a Claude helper is permitted to run under.
///
/// Both switches are forced off. Disabling `procMacro` means semantics generated by procedural
/// macros — derive-generated methods, macro-expanded items and their references — are not visible
/// to the analyzer, so Rust results under this profile are honestly incomplete for such symbols.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RustEffectiveSettings {
    /// Always false; priming would outlive the foreground helper it was started for.
    pub cache_priming: bool,
    /// Always false; see the proc-macro semantics limitation documented on this type.
    pub proc_macro: bool,
}

impl RustEffectiveSettings {
    /// Rejects any settings pair that re-enables priming or proc-macro expansion.
    pub fn validate(&self) -> Result<(), FailureCode> {
        (!self.cache_priming && !self.proc_macro)
            .then_some(())
            .ok_or(FailureCode::ExecutionProfile)
    }

    /// Returns the exact analyzer initialization payload a Claude helper must use.
    ///
    /// This is deliberately a separate value from the shared managed-path configuration, which
    /// disables cache priming only. A Claude helper additionally disables proc-macro expansion,
    /// because a foreground helper cannot own the long-lived expansion server that setting starts.
    /// Callers must not merge or extend it: the payload is the complete accepted configuration.
    pub fn configuration(&self) -> Value {
        serde_json::json!({
            "cachePriming": {"enable": self.cache_priming},
            "procMacro": {"enable": self.proc_macro},
        })
    }
}

/// Names one daemon-selected provider a helper may spawn as its own direct child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperProvider {
    /// Absolute accepted analyzer executable chosen by the daemon, never by model input.
    pub executable: PathBuf,
    /// Accepted provider version string paired with the executable identity.
    pub version: String,
    /// Exclusive language profile for this one operation.
    pub language: HelperLanguage,
    /// Effective Rust settings; present only for [`HelperLanguage::Rust`].
    pub rust_settings: Option<RustEffectiveSettings>,
    /// Fixed launcher-accepted Pyright/Node identity; present only for [`HelperLanguage::Python`].
    pub pyright: Option<HelperPyrightProfile>,
    /// Release-pinned TypeScript bundle; present only for [`HelperLanguage::TypeScript`].
    pub typescript: Option<HelperTypeScriptProfileV1>,
    /// Accepted Go executable path or Rust toolchain selector.
    pub toolchain: String,
    /// Accepted Cargo executable for Rust; absent for Go.
    pub cargo: Option<PathBuf>,
    /// Accepted Cargo version for Rust; absent for Go.
    pub cargo_version: Option<String>,
    /// Accepted rustc executable for Rust; absent for Go.
    pub rustc: Option<PathBuf>,
    /// Accepted rustc version for Rust; absent for Go.
    pub rustc_version: Option<String>,
    /// Operator trust identity retained in the provider profile.
    pub trust: String,
    /// Absolute persistent IDE-owned cache namespace retained across stop and handoff.
    pub cache_namespace: String,
}

impl HelperProvider {
    /// Checks absolute executable, language/settings agreement and a bounded cache namespace.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if !self.executable.is_absolute()
            || self.version.is_empty()
            || self.toolchain.is_empty()
            || self.trust.is_empty()
            || !PathBuf::from(&self.cache_namespace).is_absolute()
            || self.cache_namespace.len() > 4096
        {
            return Err(FailureCode::ExecutionProfile);
        }
        match (
            self.language,
            self.rust_settings.as_ref(),
            self.pyright.as_ref(),
            self.typescript.as_ref(),
            self.cargo.as_ref(),
            self.cargo_version.as_ref(),
            self.rustc.as_ref(),
            self.rustc_version.as_ref(),
        ) {
            (
                HelperLanguage::Rust,
                Some(settings),
                None,
                None,
                Some(cargo),
                Some(_),
                Some(rustc),
                Some(_),
            ) if cargo.is_absolute() && rustc.is_absolute() => settings.validate(),
            (HelperLanguage::Go, None, None, None, None, None, None, None)
                if PathBuf::from(&self.toolchain).is_absolute() =>
            {
                Ok(())
            }
            (HelperLanguage::Python, None, Some(pyright), None, None, None, None, None)
                if self.executable == pyright.script
                    && self.version == pyright.script_identity
                    && self.toolchain == pyright.node_identity =>
            {
                pyright.validate()
            }
            (HelperLanguage::TypeScript, None, None, Some(typescript), None, None, None, None)
                if self.executable == typescript.bridge.path
                    && self.version == typescript.bridge_version
                    && self.toolchain == typescript.node_version =>
            {
                typescript.validate()
            }
            _ => Err(FailureCode::ExecutionProfile),
        }
    }
}

/// Carries the durable scope a post-activation helper may inspect without minting authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperScope {
    /// Opaque durable worktree identity supplied by Workspace.
    pub worktree_id: String,
    /// Durable lifecycle incarnation for this exact root object.
    pub incarnation: u64,
    /// Canonical worktree root.
    pub root: PathBuf,
    /// Canonical repository root returned by Git discovery.
    pub repository_root: PathBuf,
    /// Raw Git common directory, which may be relative to `root`.
    pub git_common_dir: PathBuf,
    /// Descriptor-derived root identity for replacement checks inside the helper.
    pub native_root_identity: [u8; 32],
    /// Current durable Workspace authority epoch.
    pub authority_epoch: u64,
}

impl HelperScope {
    /// Rejects incomplete or non-canonical daemon scope before a helper reads source or Git state.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let normal = |path: &PathBuf| {
            path.is_absolute()
                && path.components().all(|component| {
                    matches!(
                        component,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                })
        };
        (!self.worktree_id.is_empty()
            && self.worktree_id.len() <= MAX_IDENTIFIER_BYTES
            && self.incarnation > 0
            && self.authority_epoch > 0
            && normal(&self.root)
            && normal(&self.repository_root)
            && !self.git_common_dir.as_os_str().is_empty())
        .then_some(())
        .ok_or(FailureCode::WorkspaceAuthority)
    }
}

/// Carries one durable activation baseline into a later helper Diff without exposing source bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperBaseline {
    /// Stable Workspace baseline operation reference.
    pub reference: String,
    /// True only when Workspace committed a partial v0.1 capture with an unverified window.
    pub captured: bool,
    /// Stored capture digest; present exactly when `captured` is true.
    pub digest: Option<[u8; 32]>,
}

impl HelperBaseline {
    /// Checks the closed partial-or-unknown baseline shape.
    pub fn validate(&self) -> Result<(), FailureCode> {
        (!self.reference.is_empty()
            && self.reference.len() <= 128
            && self.captured == self.digest.is_some())
        .then_some(())
        .ok_or(FailureCode::WorkspaceAuthority)
    }
}

/// Bounds every finite resource one helper operation may consume.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperBudgets {
    /// Maximum captured bytes per stream.
    pub output_bytes: usize,
    /// Maximum direct child processes the helper may spawn and must itself reap.
    pub processes: u32,
    /// Total helper lifetime, measured from claim.
    ///
    /// [`LaunchLedger::claim`] clamps this to the issuing ticket's own remaining time before its
    /// absolute `deadline_ms`, so a helper that starts working after a delayed foreground launch
    /// never computes a fresh full-length deadline that outlives the ticket the daemon will expire.
    pub deadline_ms: u64,
}

impl HelperBudgets {
    /// Rejects zero or unbounded budgets so no helper can run without a finite ceiling.
    ///
    /// The process ceiling admits the Diff helper's bounded snapshot walk, whose per-path blob
    /// read and verification children scale with the snapshot's own `MAX_SNAPSHOT_PATHS` bound,
    /// while every operation keeps a finite, launcher-independent maximum.
    pub fn validate(&self) -> Result<(), FailureCode> {
        let bounded = (1..=1024 * 1024).contains(&self.output_bytes)
            && (1..=1024).contains(&self.processes)
            && (1..=300_000).contains(&self.deadline_ms);
        bounded.then_some(()).ok_or(FailureCode::ExecutionProfile)
    }
}

/// One closed daemon-selected job handed to a helper that has successfully claimed its ticket.
///
/// Every executable, path, scope and budget in this frame is chosen by the daemon from trusted
/// launcher configuration. Model input contributes only the already validated method
/// [`HelperJob::parameters`]; it never selects an executable, shell fragment, scope or permission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperJob {
    /// Closed wire revision; only [`HELPER_PROTOCOL`] is accepted.
    pub protocol: u32,
    /// Closed operation this helper was launched for.
    pub operation: HelperOperation,
    /// Absolute candidate worktree; the helper still proves its native Git identity itself.
    pub candidate: PathBuf,
    /// Absolute accepted Git executable for the fixed discovery and snapshot commands.
    pub git: PathBuf,
    /// Canonical Workspace root when the daemon already knows it; absent before activation.
    pub canonical_root: Option<PathBuf>,
    /// Durable post-activation scope; absent only for Start.
    pub scope: Option<HelperScope>,
    /// Durable activation baseline supplied only to Diff.
    pub baseline: Option<HelperBaseline>,
    /// Per-operation exclusive provider; absent when the operation needs no analyzer.
    pub provider: Option<HelperProvider>,
    /// Daemon-selected exact pre-edit source facts, present only for Edit.
    pub edit_source: Option<HelperEditSource>,
    /// Already validated closed method parameters carrying no target, profile or authority data.
    pub parameters: Value,
    /// Finite byte, process and deadline ceilings for this operation.
    pub budgets: HelperBudgets,
}

impl HelperJob {
    /// Validates the complete outbound frame before it is written to the helper socket.
    ///
    /// This is the daemon-side half of the two-boundary rule: a frame that cannot be validated is
    /// never sent, so a helper never has to trust a malformed job.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if self.protocol != HELPER_PROTOCOL
            || !self.candidate.is_absolute()
            || !self.git.is_absolute()
            || self
                .canonical_root
                .as_ref()
                .is_some_and(|root| !root.is_absolute())
            || !self.parameters.is_object()
        {
            return Err(FailureCode::ExecutionProfile);
        }
        self.budgets.validate()?;
        match &self.provider {
            Some(provider) => provider.validate(),
            None => Ok(()),
        }?;
        match self.operation {
            HelperOperation::Start
                if self.canonical_root.is_none()
                    && self.scope.is_none()
                    && self.baseline.is_none()
                    && self.provider.is_none()
                    && self.edit_source.is_none() =>
            {
                Ok(())
            }
            HelperOperation::Context
                if self.scope.as_ref().is_some_and(|scope| {
                    scope.validate().is_ok() && self.canonical_root.as_ref() == Some(&scope.root)
                }) && self.baseline.is_none()
                    && self.edit_source.is_none() =>
            {
                Ok(())
            }
            HelperOperation::Diff
                if self.scope.as_ref().is_some_and(|scope| {
                    scope.validate().is_ok() && self.canonical_root.as_ref() == Some(&scope.root)
                }) && self
                    .baseline
                    .as_ref()
                    .is_some_and(|baseline| baseline.validate().is_ok())
                    && self.provider.is_none()
                    && self.edit_source.is_none() =>
            {
                Ok(())
            }
            HelperOperation::Edit
                if self.scope.as_ref().is_some_and(|scope| {
                    scope.validate().is_ok() && self.canonical_root.as_ref() == Some(&scope.root)
                }) && self.baseline.is_none()
                    && self.edit_source.as_ref().is_some_and(|source| {
                        source.validate().is_ok()
                            && self.parameters["path"].as_str() == Some(source.path.as_str())
                    })
                    && serde_json::from_value::<crate::changes::edit::EditRequest>(
                        self.parameters.clone(),
                    )
                    .is_ok_and(|request| request.validate().is_ok()) =>
            {
                Ok(())
            }
            _ => Err(FailureCode::ExecutionProfile),
        }
    }

    /// Encodes one bounded outbound frame, failing closed above [`MAX_HELPER_FRAME_BYTES`].
    pub fn encode(&self) -> Result<String, FailureCode> {
        self.validate()?;
        let frame = serde_json::to_string(self).map_err(|_| FailureCode::Internal)?;
        (frame.len() <= MAX_HELPER_FRAME_BYTES)
            .then_some(frame)
            .ok_or(FailureCode::Capacity)
    }

    /// Decodes one bounded inbound frame on the helper side, validating before use.
    pub fn decode(frame: &str) -> Result<Self, FailureCode> {
        if frame.len() > MAX_HELPER_FRAME_BYTES {
            return Err(FailureCode::Capacity);
        }
        let job: Self = serde_json::from_str(frame).map_err(|_| FailureCode::ExecutionProfile)?;
        job.validate()?;
        Ok(job)
    }
}

/// Names the fixed Git discovery queries a helper may report evidence for.
///
/// This mirrors Execution's closed discovery query set on the helper wire so the daemon can
/// reconstruct canonical evidence without accepting an arbitrary command identity from a helper.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperQuery {
    /// `rev-parse --show-toplevel`.
    ShowTopLevel,
    /// `rev-parse --path-format=absolute --git-common-dir`.
    GitCommonDir,
    /// `worktree list --porcelain -z`.
    WorktreeListPorcelainZ,
}

/// One fixed query's bounded raw result, exactly as the helper's own child produced it.
///
/// The helper reports raw bytes and the observed exit code and interprets nothing. Canonical
/// interpretation stays with the daemon's existing discovery validator, so a helper cannot assert
/// a worktree identity by claiming one — it can only supply the bytes that identity is derived
/// from. `stdout`/`stderr` are `-z`-safe raw bytes, not text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryFrame {
    /// Fixed query these bytes came from.
    pub query: HelperQuery,
    /// Raw captured stdout, capped by the job's output budget.
    pub stdout: Vec<u8>,
    /// Raw captured stderr, capped by the job's output budget.
    pub stderr: Vec<u8>,
    /// Observed process exit code; `None` when the child was signalled.
    pub exit_code: Option<i32>,
    /// Whether the caps discarded trailing output on either stream.
    pub truncated: bool,
}

impl DiscoveryFrame {
    /// Rejects an over-bound stream or non-Unix exit code before discovery status conversion.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if self.stdout.len() > MAX_DISCOVERY_STDOUT_BYTES
            || self.stderr.len() > MAX_DISCOVERY_STDERR_BYTES
        {
            return Err(FailureCode::Capacity);
        }
        self.exit_code
            .is_none_or(|code| (0..=255).contains(&code))
            .then_some(())
            .ok_or(FailureCode::ExecutionProfile)
    }
}

/// Reports how many direct children a helper spawned and how many it actually reaped.
///
/// The helper alone owns, cancels, drains and reaps its Git and provider children. The daemon
/// treats the helper endpoint as host-managed and borrowed: it never claims a direct-child reap
/// for the helper's PID and never kills a borrowed numeric PID.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSettlement {
    /// Direct children the helper started during this operation.
    pub spawned: u32,
    /// Direct children the helper observed exiting and reaped before closing.
    pub reaped: u32,
}

impl ChildSettlement {
    /// Returns whether every spawned direct child was actually reaped.
    ///
    /// A completed helper result may only be reported when this holds; otherwise the operation is
    /// uncertain and its admission stays quarantined. Stop itself is daemon-owned and launches no
    /// helper.
    pub fn settled(&self) -> bool {
        self.spawned == self.reaped
    }
}

/// Names one fixed activation-baseline query returned by the helper.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperBaselineQuery {
    /// Complete recursive HEAD tree listing.
    HeadTree,
    /// Complete NUL-delimited untracked path listing.
    UntrackedPaths,
    /// Exact HEAD object identity.
    HeadIdentity,
}

/// Carries one fixed baseline child's bounded raw output and actual exit status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperBaselineFrame {
    /// Fixed query whose output is carried.
    pub query: HelperBaselineQuery,
    /// Complete bounded stdout bytes.
    pub stdout: Vec<u8>,
    /// Complete bounded stderr bytes.
    pub stderr: Vec<u8>,
    /// Direct-child exit status when observed.
    pub exit_code: Option<i32>,
    /// True when either stream exceeded its configured bound.
    pub truncated: bool,
}

/// Identifies exact source bytes observed by a Context helper without returning the full file twice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperSource {
    /// Validated relative UTF-8 path selected by the Context call.
    pub path: String,
    /// Whether the path was a regular file; false means a bounded missing-path observation.
    pub present: bool,
    /// Digest of the complete bounded file, absent for a missing path.
    pub digest: Option<[u8; 32]>,
    /// Complete file byte length, zero for a missing path.
    pub length: u64,
}

/// Daemon-selected completed-context facts a helper must match before an Edit effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperEditSource {
    /// Exact relative UTF-8 path bound to the source reference.
    pub path: String,
    /// Whether the completed context observed a regular file rather than an eligible absence.
    pub present: bool,
    /// Exact digest of present pre-edit bytes, absent only for a missing target.
    pub digest: Option<[u8; 32]>,
    /// Exact present byte length, or zero for a missing target.
    pub length: u64,
    /// Durable Workspace observation ordering value.
    pub sequence: u64,
    /// Nonempty bounded Workspace source revision.
    pub source_revision: String,
    /// Opaque completed-context observation reference.
    pub observation_ref: String,
}

impl HelperEditSource {
    /// Rejects incomplete, oversized or internally inconsistent source facts before helper launch.
    pub fn validate(&self) -> Result<(), FailureCode> {
        (!self.path.is_empty()
            && self.path.len() <= 1024
            && self.present == self.digest.is_some()
            && self.length <= crate::workspace::observation::MAX_SOURCE_BYTES as u64
            && (self.present || self.length == 0)
            && self.sequence > 0
            && !self.source_revision.is_empty()
            && self.source_revision.len() <= 128
            && !self.observation_ref.is_empty()
            && self.observation_ref.len() <= 128)
            .then_some(())
            .ok_or(FailureCode::SourceUnavailable)
    }
}

/// Adds operation-specific evidence to a completed helper result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperPayload {
    /// Raw fixed baseline reads collected with Start discovery.
    Start {
        /// Exactly the three baseline frames, in the daemon-selected query set.
        baseline: Vec<HelperBaselineFrame>,
    },
    /// Current bounded source context and optional provisional diagnostic delta.
    Context {
        /// Exact source digest/length and selected path.
        source: HelperSource,
        /// Bounded fact/evidence/action delta; absent without matched diagnostics.
        feedback: Option<String>,
        /// Content fingerprint of the exact ordered raw diagnostic message set backing
        /// `feedback`, absent exactly when `feedback` is. Computed by the helper from the same
        /// typed diagnostics before rendering, never from `feedback`'s rendered text, so the
        /// daemon can recognize a later, unchanged repeat of this same issue without re-parsing
        /// presentation text. `#[serde(default)]` keeps decoding an older helper frame lacking
        /// this field closed rather than rejected.
        #[serde(default)]
        diagnostic_fingerprint: Option<[u8; 32]>,
        /// Whether source/context/diagnostic selection omitted bounded material.
        truncated: bool,
    },
    /// Current composed Git comparison text.
    Diff {
        /// Whether bounded hunk selection omitted material.
        truncated: bool,
    },
    /// Typed single-file edit outcome plus exact post-read source facts for known success.
    Edit {
        /// Closed Changes outcome derived from the helper's Workspace result.
        outcome: crate::changes::edit::EditOutcome,
        /// Exact post-read digest/length for success, absent for no-effect or unknown outcomes.
        source: Option<HelperSource>,
        /// Assistance-only diagnostic evidence derived from the exact helper post-read generation.
        ///
        /// Helpers cannot retain inspectable detail, so they never carry `Pending`; older frames
        /// decode conservatively as unknown rather than inventing clean state.
        #[serde(default)]
        diagnostics: super::reply::EditDiagnostics,
    },
}

impl HelperPayload {
    /// Returns the closed operation this payload can settle.
    pub const fn operation(&self) -> HelperOperation {
        match self {
            Self::Start { .. } => HelperOperation::Start,
            Self::Context { .. } => HelperOperation::Context,
            Self::Diff { .. } => HelperOperation::Diff,
            Self::Edit { .. } => HelperOperation::Edit,
        }
    }
}

/// The bounded outcome one helper reports before closing its socket and exiting.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperOutcome {
    /// The operation produced owner evidence rendered as bounded text.
    Complete {
        /// Bounded owner text containing only the selected source/diff and diagnostic messages,
        /// with no launcher, attachment, host configuration or unbounded provider payload.
        text: String,
    },
    /// The operation reached a closed failure category without owner evidence.
    Failed {
        /// Stable actionable failure category.
        code: FailureCode,
    },
}

/// One bounded inbound helper frame carrying its result and real child-settlement evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelperResult {
    /// Closed wire revision; only [`HELPER_PROTOCOL`] is accepted.
    pub protocol: u32,
    /// The exact action-scoped handle this helper claimed.
    pub detail_ref: String,
    /// Bounded operation outcome.
    pub outcome: HelperOutcome,
    /// Measured direct-child settlement, never inferred from an exit status alone.
    pub children: ChildSettlement,
    /// Raw fixed-query discovery bytes, empty for an operation that performed no discovery.
    ///
    /// The daemon reconstructs canonical evidence from these bytes; the helper asserts nothing
    /// about what they mean.
    #[serde(default)]
    pub discovery: Vec<DiscoveryFrame>,
    /// Operation-specific evidence produced by the verified helper binary.
    pub payload: Option<HelperPayload>,
}

impl HelperResult {
    /// Validates the complete inbound frame before any daemon state is changed by it.
    pub fn validate(&self) -> Result<(), FailureCode> {
        if self.protocol != HELPER_PROTOCOL
            || self.detail_ref.is_empty()
            || self.detail_ref.len() > MAX_IDENTIFIER_BYTES
            || self.children.reaped > self.children.spawned
        {
            return Err(FailureCode::ExecutionProfile);
        }
        if let HelperOutcome::Complete { text } = &self.outcome {
            if text.len() > MAX_RESULT_TEXT_BYTES {
                return Err(FailureCode::Capacity);
            }
            // Positive settlement is a precondition of any completed result, not a later report.
            // Without it a helper claiming spawned=3/reaped=0 would still deliver a success, so the
            // frame is refused here, before it can change any daemon state.
            if !self.children.settled() {
                return Err(FailureCode::Deadline);
            }
        }
        if self.discovery.len() > 3 {
            return Err(FailureCode::Capacity);
        }
        for frame in &self.discovery {
            frame.validate()?;
        }
        if let Some(HelperPayload::Start { baseline }) = &self.payload
            && (baseline.len() != 3
                || baseline.iter().any(|frame| {
                    frame.stdout.len() > MAX_DISCOVERY_STDOUT_BYTES
                        || frame.stderr.len() > MAX_DISCOVERY_STDERR_BYTES
                }))
        {
            return Err(FailureCode::Capacity);
        }
        if let Some(HelperPayload::Context {
            source, feedback, ..
        }) = &self.payload
            && (source.path.is_empty()
                || source.path.len() > 1024
                || source.length > 1024 * 1024
                || source.present != source.digest.is_some()
                || (!source.present && source.length != 0)
                || feedback.as_ref().is_some_and(|text| text.len() > 4096))
        {
            return Err(FailureCode::Capacity);
        }
        if let Some(HelperPayload::Edit {
            outcome,
            source,
            diagnostics,
        }) = &self.payload
        {
            let success = outcome.has_post_source();
            if success != source.is_some()
                || source.as_ref().is_some_and(|source| {
                    source.path.is_empty()
                        || source.path.len() > 1024
                        || !source.present
                        || source.digest.is_none()
                        || source.length > crate::workspace::observation::MAX_SOURCE_BYTES as u64
                })
            {
                return Err(FailureCode::Capacity);
            }
            if !diagnostics.valid()
                || matches!(diagnostics, super::reply::EditDiagnostics::Pending { .. })
                || (!success && !matches!(diagnostics, super::reply::EditDiagnostics::Unknown {}))
            {
                return Err(FailureCode::Capacity);
            }
        }
        Ok(())
    }

    /// Encodes one bounded outbound frame on the helper side.
    pub fn encode(&self) -> Result<String, FailureCode> {
        self.validate()?;
        let frame = serde_json::to_string(self).map_err(|_| FailureCode::Internal)?;
        (frame.len() <= MAX_HELPER_FRAME_BYTES)
            .then_some(frame)
            .ok_or(FailureCode::Capacity)
    }

    /// Decodes one bounded inbound frame on the daemon side, validating before use.
    pub fn decode(frame: &str) -> Result<Self, FailureCode> {
        if frame.len() > MAX_HELPER_FRAME_BYTES {
            return Err(FailureCode::Capacity);
        }
        let result: Self =
            serde_json::from_str(frame).map_err(|_| FailureCode::ExecutionProfile)?;
        result.validate()?;
        Ok(result)
    }
}

/// Identifies the exact Claude actor a ticket is bound to.
///
/// `actor` is the hook's own distinguishing identity: a subagent's exact `agent_id`, or the root
/// `session_id` for a parent event. `session` is the root session retained as context only and is
/// deliberately excluded from equality, so a parent and one of its subagents never compare equal.
/// Both come from the trusted native hook binding, never from tool arguments.
#[derive(Clone, Debug)]
pub struct HelperActor {
    /// Exact distinguishing hook actor: a subagent's `agent_id`, or a parent's `session_id`.
    actor: String,
    /// Root session identity retained as context; excluded from identity comparison.
    #[allow(dead_code)]
    session: Option<String>,
}

impl PartialEq for HelperActor {
    /// Compares only the distinguishing hook actor; retained session context is not identity.
    fn eq(&self, other: &Self) -> bool {
        self.actor == other.actor
    }
}

impl Eq for HelperActor {}

impl HelperActor {
    /// Builds one bounded actor identity, rejecting empty or over-limit fields.
    ///
    /// `actor` is required; `session` is optional context that never affects comparison.
    pub fn new(actor: &str, session: Option<&str>) -> Result<Self, FailureCode> {
        let bounded = |value: &str| !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES;
        if !bounded(actor) || session.is_some_and(|value| !bounded(value)) {
            return Err(FailureCode::Conflict);
        }
        Ok(Self {
            actor: actor.to_owned(),
            session: session.map(str::to_owned),
        })
    }

    /// Returns the exact distinguishing hook actor identity.
    pub fn actor(&self) -> &str {
        &self.actor
    }
}

/// Tracks how far exactly one action-scoped helper handle has progressed.
///
/// The order is fixed: a ticket must be recognized as an actual native launch before it can be
/// claimed. A bare copied handle presented without its matching native pre-hook is still `Minted`
/// and is therefore rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TicketState {
    /// Returned to the model; no native launch has been recognized yet.
    Minted,
    /// The exact expected command was recognized in an ordinary foreground `Bash` pre-hook.
    Launched {
        /// Native tool-call identity of the recognized `Bash` invocation.
        tool_use_id: String,
    },
    /// A helper claimed this handle exactly once; a second claim is rejected.
    Claimed(ClaimedWork),
    /// Claimed work whose result authority is permanently suppressed.
    ///
    /// The claimed correlation is *retained*, not discarded: without the exact `tool_use_id`,
    /// frame slot and lease flag there is nothing a late cleanup frame or post could correlate
    /// against, so positive cleanup settlement would be impossible and the bounded lease could
    /// never be released on proof. Suppression is a property of [`LaunchLedger::delivery`] and
    /// [`LaunchLedger::settled`], not of destroying the identity.
    Uncertain(ClaimedWork),
    /// Edit ticket expired before claim, proving the prepared request had zero target effects.
    ExpiredNoEffect,
}

/// The correlated state one claimed helper operation accumulates, in either arrival order.
///
/// The same value survives revocation and expiry unchanged, which is what makes cleanup-only
/// settlement possible after authority has been suppressed.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ClaimedWork {
    /// Native tool-call identity carried over from recognition; correlates the matching post.
    tool_use_id: String,
    /// Final helper frame, once it has arrived.
    frame: Option<HelperResult>,
    /// Whether the matching successful `Bash` post-hook has arrived.
    post: Option<bool>,
    /// Whether this operation still holds its slot in the shared Execution budget.
    ///
    /// The lease itself lives in [`LeasePool`] under this operation's `detail_ref`; this flag is
    /// the per-operation half of the same fact, so a cloned work record cannot duplicate capacity.
    /// Reserved at claim, released exactly once and only on positive proof: the final frame with
    /// exactly settled children plus the matching successful post. An abandoned, revoked or
    /// unsettled operation keeps the lease, so lost capacity is honest rather than reclaimed on a
    /// disappearance.
    lease: bool,
}

/// One action-scoped, single-use helper handle bound to exactly one Claude operation.
///
/// The handle value is the existing `Pending.detail_ref`. No additional public tool, reply state
/// or launch flag is introduced: the ticket is daemon-private bookkeeping hanging off that handle.
#[derive(Debug)]
pub struct LaunchTicket {
    /// Exact binding generation this ticket is fenced to.
    ///
    /// The whole reference is retained, not just its fingerprint, so a claim can be checked against
    /// the *current* liveness of that generation instead of against a value the caller supplied.
    binding: BindingRef,
    /// Accepted identity of the helper executable the expected command names.
    helper: AcceptedIdentity,
    /// Accepted identities of every executable this job's children may be, preserved for settlement.
    children: Vec<AcceptedIdentity>,
    /// Exact Claude actor permitted to claim this handle.
    actor: HelperActor,
    /// Opaque private transport channel this handle was minted on.
    channel: String,
    /// Exact expected helper command bytes; recognition compares against these, never a parser.
    command: String,
    /// Closed daemon-selected job released on a successful claim.
    job: HelperJob,
    /// Absolute expiry on the caller-supplied monotonic clock.
    deadline_ms: u64,
    /// Current single-use progress.
    state: TicketState,
}

impl LaunchTicket {
    /// Returns the exact command the model must run before inspecting this handle.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Returns whether this ticket has been claimed and is awaiting or holding settlement.
    pub fn claimed(&self) -> bool {
        matches!(self.state, TicketState::Claimed { .. })
    }

    /// Returns whether one native tool-call identity is this ticket's own helper invocation.
    ///
    /// Quarantined work still correlates, so a late post arriving after revocation is recognized
    /// as the helper's own settlement rather than as an unrelated native edit.
    fn correlates(&self, tool_use_id: &str) -> bool {
        matches!(
            &self.state,
            TicketState::Claimed(work) | TicketState::Uncertain(work)
                if work.tool_use_id == tool_use_id
        )
    }

    /// Returns whether this ticket's claimed work already carries full positive settlement proof:
    /// the final frame together with a matching successful post and exactly settled children.
    ///
    /// This mirrors the exact guard [`LaunchLedger::delivery`] uses to report [`Delivery::Ready`].
    /// [`LaunchLedger::expire`] consults it so a ticket that reached this proof before its deadline
    /// is never downgraded to `Uncertain` merely because nobody retrieved it first — that downgrade
    /// would discard a known outcome for `outcome_unknown` even though the physical work is already
    /// proven.
    fn positively_settled(&self) -> bool {
        matches!(
            &self.state,
            TicketState::Claimed(ClaimedWork {
                frame: Some(frame),
                post: Some(true),
                ..
            }) if frame.children.settled()
        )
    }

    /// Suppresses this ticket's authority, returning whether it must be retained.
    ///
    /// Work that provably never ran (`Minted`, `Launched`) is normally retired and returns `false`.
    /// Edit is retained as `ExpiredNoEffect` so its already-prepared Changes receipt can settle
    /// durably rather than becoming replayable. Claimed work retains its correlation and returns
    /// `true`.
    ///
    /// Quarantining also releases the bounded lease when the retained work *already* carries
    /// positive settlement proof. Nothing will ever consume that evidence now, so continuing to
    /// hold its capacity would strand a slot on work that is provably finished. Work without such
    /// proof keeps its lease, which is the honest outcome for cleanup that was never observed.
    fn quarantine(&mut self, detail_ref: &str, leases: &mut LeasePool) -> bool {
        if matches!(
            self.state,
            TicketState::Minted | TicketState::Launched { .. }
        ) && self.job.operation == HelperOperation::Edit
        {
            self.state = TicketState::ExpiredNoEffect;
            return true;
        }
        let retained = match &self.state {
            TicketState::Minted | TicketState::Launched { .. } => None,
            TicketState::Claimed(work) => Some(Some(work.clone())),
            TicketState::Uncertain(_) => Some(None),
            TicketState::ExpiredNoEffect => Some(None),
        };
        match retained {
            None => false,
            Some(None) => true,
            Some(Some(mut work)) => {
                LaunchLedger::release_settled_lease(&mut work, detail_ref, leases);
                self.state = TicketState::Uncertain(work);
                true
            }
        }
    }
}

/// One accepted executable identity carried into a helper ticket.
///
/// This is the ticket-side copy of the launcher's already accepted `AcceptedExecutable` evidence:
/// the absolute path, its nonempty accepted identity string and its 64-hex BLAKE3 digest. It is
/// deliberately not deserializable, so a helper frame can never introduce an executable identity;
/// only the daemon, from trusted launcher configuration, can.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedIdentity {
    /// Absolute accepted executable path chosen by the launcher.
    path: PathBuf,
    /// Nonempty accepted binary/version identity.
    identity: String,
    /// 64-character lowercase-or-uppercase hex BLAKE3 digest of the accepted bytes.
    blake3: String,
}

impl AcceptedIdentity {
    /// Builds one identity, rejecting a relative path, empty identity or non-hex/short digest.
    ///
    /// Returns [`FailureCode::ExecutionProfile`] on any malformed field, because a missing or
    /// unusable executable identity is an execution-profile problem and never a runtime failure.
    pub fn new(path: PathBuf, identity: &str, blake3: &str) -> Result<Self, FailureCode> {
        if !path.is_absolute()
            || identity.is_empty()
            || identity.len() > MAX_IDENTIFIER_BYTES
            || blake3.len() != 64
            || !blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(FailureCode::ExecutionProfile);
        }
        Ok(Self {
            path,
            identity: identity.to_owned(),
            blake3: blake3.to_owned(),
        })
    }

    /// Returns the absolute accepted executable path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns the accepted binary/version identity string.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Returns the accepted BLAKE3 digest of the executable's bytes.
    pub fn blake3(&self) -> &str {
        &self.blake3
    }
}

/// Daemon-private proof that exactly one Claude helper operation settled positively.
///
/// This token is the only thing that may carry helper evidence into the Worker authority path. It
/// is produced solely by [`LaunchLedger::settled`], which requires, all at once: exact ticket and
/// binding ownership; the final frame correlated to that exact handle; a matching successful post;
/// a `Complete` outcome with exactly settled child accounting; and verified helper and child
/// executable identities. Its admission lease may already have been released after positive proof.
/// It deliberately implements neither
/// `Deserialize` nor `Default`, and its fields are private with no public constructor, so no amount
/// of arbitrary `HelperResult` JSON can mint one.
///
/// Holding the token asserts settlement only. It is not workspace authority, not a sandbox
/// observation and not a durable identity: the Worker still resolves and activates a worktree
/// itself, and still reconsumes binding liveness after every await.
#[derive(Debug)]
pub struct SettledClaudeOperation {
    /// The exact closed job this daemon released to the helper.
    job: HelperJob,
    /// The validated final frame the helper reported for that job.
    result: HelperResult,
    /// The binding generation the settled ticket was fenced to.
    binding: BindingRef,
    /// Verified accepted identity of the helper executable.
    helper: AcceptedIdentity,
    /// Verified accepted identities of the job's permitted child executables.
    children: Vec<AcceptedIdentity>,
}

impl SettledClaudeOperation {
    /// Returns the exact daemon-selected job the settled evidence belongs to.
    pub fn job(&self) -> &HelperJob {
        &self.job
    }

    /// Returns the validated final helper frame.
    pub fn result(&self) -> &HelperResult {
        &self.result
    }

    /// Returns the binding generation this settled operation is fenced to.
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    /// Returns the closed operation kind this evidence settles.
    pub fn operation(&self) -> HelperOperation {
        self.job.operation
    }

    /// Returns the raw fixed-query discovery bytes, uninterpreted.
    pub fn discovery(&self) -> &[DiscoveryFrame] {
        &self.result.discovery
    }

    /// Returns the verified helper executable identity.
    pub fn helper(&self) -> &AcceptedIdentity {
        &self.helper
    }

    /// Returns the verified child executable identities preserved from the accepted job.
    pub fn children(&self) -> &[AcceptedIdentity] {
        &self.children
    }
}

/// Reports what an ordinary `Bash` pre-hook observation meant for helper correlation.
///
/// Both variants are silent at the hook boundary. The hook never returns a permission decision,
/// never rewrites the command and never attaches updated input, so the host's own permission and
/// sandbox evaluation of the unchanged command is what actually authorizes the launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchRecognition {
    /// The payload was not a byte-exact foreground helper command for a live ticket.
    ///
    /// All such tool payloads are discarded; nothing is retained about them.
    Ignored,
    /// The exact expected bytes were recognized and the ticket became claimable.
    Recognized,
}

/// Holds the Claude route's share of the daemon's single Execution admission budget.
///
/// The ledger owns no ceiling of its own. Every claimed operation is charged against the same
/// [`crate::execution::AdmissionController`] the worker uses for discovery, snapshots and language
/// providers, so an ordinary operation and a Claude helper child contend for one configured global
/// budget and neither can widen it. The reservation is taken before a job is released, which is why
/// a helper process can never exist without a lease that was granted first.
///
/// Leases are keyed by the owning `detail_ref` rather than stored in `ClaimedWork`, because
/// [`crate::execution::AdmissionLease`] is deliberately not `Clone`: capacity is authority, and the
/// quarantine path clones the work record.
#[derive(Debug)]
pub struct LeasePool {
    /// The daemon's single admission owner, shared with the worker.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// One granted lease for each operation still charged to the budget.
    held: BTreeMap<String, crate::execution::AdmissionLease>,
}

impl LeasePool {
    /// Wraps the daemon's single admission controller; creates no capacity of its own.
    pub fn new(admission: Arc<Mutex<crate::execution::AdmissionController>>) -> Self {
        Self {
            admission,
            held: BTreeMap::new(),
        }
    }

    /// Reserves one global slot for `detail_ref`, owned by `binding`'s generation.
    ///
    /// Returns `false` when the shared budget is exhausted or queued: the Claude route never waits
    /// behind a queue, because the model is already blocked in a foreground `Bash` call, so a
    /// queued ticket is cancelled and reported as capacity. A second reservation for the same
    /// handle is refused, so a replayed claim can never take two slots.
    fn reserve(&mut self, detail_ref: &str, binding: &BindingRef) -> bool {
        use crate::execution::{Admission, AdmissionClass, OwnerId};
        if self.held.contains_key(detail_ref) {
            return false;
        }
        let Ok(owner) = OwnerId::new(
            blake3::Hash::from_bytes(binding.fingerprint())
                .to_hex()
                .to_string(),
        ) else {
            return false;
        };
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match admission.submit(owner, AdmissionClass::Interactive) {
            Admission::Granted(lease) => {
                self.held.insert(detail_ref.to_owned(), lease);
                true
            }
            Admission::Queued(ticket) => {
                admission.cancel_ticket(ticket);
                false
            }
            Admission::Refused(_) => false,
        }
    }

    /// Returns one held slot to the shared budget; unknown handles change nothing.
    fn release(&mut self, detail_ref: &str) {
        let Some(lease) = self.held.remove(detail_ref) else {
            return;
        };
        let _ = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(lease);
    }

    /// Returns how many global slots this route currently holds.
    fn len(&self) -> usize {
        self.held.len()
    }
}

/// Reports the closed outcome of one helper claim attempt over the private socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    /// The claim was atomic, first, in time and correctly correlated.
    Granted(Box<HelperJob>),
    /// The claim was refused; no job, source, provider or Git effect occurred.
    Rejected(FailureCode),
}

/// Reports whether a settled operation may become visible to the model yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Both the final helper frame and the exact successful post-hook have arrived.
    Ready(Box<HelperResult>),
    /// One of the two required settlement halves is still missing.
    Waiting,
    /// Settlement failed, expired or the launch was denied; the outcome is honest and finite.
    Failed(FailureCode),
    /// An Edit ticket expired before claim, proving its prepared request had no target effect.
    DeadlineNoEffect(Box<HelperJob>),
    /// A claimed Edit lost settlement after an effect became possible.
    OutcomeUnknown(Box<HelperJob>),
}

/// Holds the bounded set of outstanding Claude helper tickets for one daemon boot.
///
/// The ledger performs no I/O. It exists so that ticket admission, exact command recognition,
/// single-use claiming, expiry and post-order settlement are one testable atomic unit rather than
/// state spread across the transport, hook and worker paths.
#[derive(Debug)]
pub struct LaunchLedger {
    /// Outstanding handles keyed by their action-scoped `detail_ref`.
    tickets: BTreeMap<String, LaunchTicket>,
    /// This route's slots in the daemon's single shared Execution budget.
    leases: LeasePool,
}

impl LaunchLedger {
    /// Creates an empty ledger that charges every claim to the daemon's shared admission owner.
    ///
    /// The controller is supplied rather than created here: this route must never own a budget of
    /// its own, or a Claude helper child could run outside the configured global ceiling.
    pub fn new(admission: Arc<Mutex<crate::execution::AdmissionController>>) -> Self {
        Self {
            tickets: BTreeMap::new(),
            leases: LeasePool::new(admission),
        }
    }

    /// Renders the exact fixed foreground helper command for one handle.
    ///
    /// The shape is fixed and fully daemon-chosen. Each value is one POSIX shell word, rendered
    /// bare where that never requires quoting and single-quoted otherwise (see `shell_word`);
    /// the full expanded command is then bounded by [`Self::mint`], stored as the ticket's
    /// expected command, and returned verbatim. Recognition later accepts this exact command by
    /// bytes, or any other command that splits into the same POSIX words (see `helper_words`),
    /// so an equivalently requoted reproduction of the same argv still matches.
    pub fn helper_command(
        binary: &std::path::Path,
        runtime_dir: &std::path::Path,
        attachment: &str,
        detail_ref: &str,
    ) -> String {
        format!(
            "{} {HELPER_SUBCOMMAND} --runtime-dir {} --attachment {} --detail-ref {}",
            shell_word(&binary.to_string_lossy()),
            shell_word(&runtime_dir.to_string_lossy()),
            shell_word(attachment),
            shell_word(detail_ref),
        )
    }

    /// Mints one single-use ticket for an already validated Claude operation.
    ///
    /// Minting has no source, Git or provider effect: it only records what a later helper would be
    /// permitted to do. Capacity and job validity are checked before the handle becomes live.
    #[allow(clippy::too_many_arguments)]
    pub fn mint(
        &mut self,
        detail_ref: &str,
        binding: BindingRef,
        actor: HelperActor,
        channel: &str,
        command: String,
        job: HelperJob,
        deadline_ms: u64,
        helper: AcceptedIdentity,
        children: Vec<AcceptedIdentity>,
    ) -> Result<(), FailureCode> {
        if self.tickets.len() >= MAX_TICKETS {
            return Err(FailureCode::Capacity);
        }
        if detail_ref.is_empty()
            || detail_ref.len() > MAX_IDENTIFIER_BYTES
            || command.is_empty()
            || command.len() > MAX_COMMAND_BYTES
            || channel.is_empty()
            || channel.len() > MAX_IDENTIFIER_BYTES
        {
            return Err(FailureCode::ExecutionProfile);
        }
        if self.tickets.contains_key(detail_ref) {
            return Err(FailureCode::Conflict);
        }
        job.validate()?;
        self.tickets.insert(
            detail_ref.to_owned(),
            LaunchTicket {
                binding,
                helper,
                children,
                actor,
                channel: channel.to_owned(),
                command,
                job,
                deadline_ms,
                state: TicketState::Minted,
            },
        );
        Ok(())
    }

    /// Recognizes an ordinary foreground `Bash` pre-hook as the launch of one live ticket.
    ///
    /// Recognition accepts either byte-exact equality with the stored expected command, or the
    /// observed command splitting into the exact same POSIX words as the stored command under
    /// `helper_words`'s conservative parser — so a shell-equivalent requoting of the identical
    /// argv still matches. `run_in_background == false` is still required. A background launch, a
    /// compound command, an added or missing argument, a shell expansion or any other tool
    /// payload or a hook on another attachment is [`LaunchRecognition::Ignored`] and discarded.
    /// `channel` is the exact lease attachment carried by the trusted hook transport.
    /// Recognition is not authorization: it only records that the native launch happened.
    pub fn recognize(
        &mut self,
        channel: &str,
        command: &str,
        run_in_background: bool,
        tool_use_id: &str,
        actor: &HelperActor,
        now_ms: u64,
    ) -> LaunchRecognition {
        if run_in_background || tool_use_id.is_empty() {
            return LaunchRecognition::Ignored;
        }
        let observed_words = helper_words(command);
        // Actor equality is enforced here, at the trusted native pre-hook, because this is where
        // the host itself states who ran the command. A helper process cannot be asked for its own
        // actor identity: it would only be repeating a value it was handed.
        let Some(ticket) = self.tickets.values_mut().find(|ticket| {
            (ticket.command.as_bytes() == command.as_bytes()
                || observed_words
                    .as_deref()
                    .is_some_and(|words| helper_words(&ticket.command).as_deref() == Some(words)))
                && &ticket.actor == actor
                && ticket.channel == channel
        }) else {
            return LaunchRecognition::Ignored;
        };
        if ticket.state != TicketState::Minted || now_ms >= ticket.deadline_ms {
            return LaunchRecognition::Ignored;
        }
        ticket.state = TicketState::Launched {
            tool_use_id: tool_use_id.to_owned(),
        };
        LaunchRecognition::Recognized
    }

    /// Classifies why an ignored `Bash` pre-hook did not recognize a live launch, for the error
    /// log only (T107); never mutates the ledger and is never consulted by [`Self::recognize`].
    ///
    /// Returns `None` for a command that does not even superficially look like a foreground helper
    /// launch (does not contain the fixed helper subcommand word), so an ordinary `Bash` call never
    /// produces log noise. The command text itself is never returned or logged, only the closed
    /// class. A matching command from another channel is reported as `NoTicket`.
    pub fn diagnose_ignored(
        &self,
        channel: &str,
        command: &str,
        run_in_background: bool,
        tool_use_id: &str,
        actor: &HelperActor,
        now_ms: u64,
    ) -> Option<crate::errorlog::ReasonCode> {
        use crate::errorlog::ReasonCode;
        if !command.contains(HELPER_SUBCOMMAND) {
            return None;
        }
        if run_in_background || tool_use_id.is_empty() {
            return Some(ReasonCode::NoTicket);
        }
        let observed_words = helper_words(command);
        let matched = self.tickets.values().find(|ticket| {
            ticket.command.as_bytes() == command.as_bytes()
                || observed_words
                    .as_deref()
                    .is_some_and(|words| helper_words(&ticket.command).as_deref() == Some(words))
        });
        let Some(ticket) = matched else {
            return Some(ReasonCode::NoTicket);
        };
        if ticket.channel != channel {
            return Some(ReasonCode::NoTicket);
        }
        if &ticket.actor != actor {
            return Some(ReasonCode::ActorMismatch);
        }
        if now_ms >= ticket.deadline_ms {
            return Some(ReasonCode::Expired);
        }
        if ticket.state != TicketState::Minted {
            return Some(ReasonCode::NotMinted);
        }
        None
    }

    /// Atomically claims one recognized ticket exactly once and releases its closed job.
    ///
    /// Rejected: a handle whose native launch was never recognized (a bare copied reference, or a
    /// launch by a different actor, which never reaches `Launched`), a different channel, a
    /// binding generation that is no longer live, an expired deadline, any second or replayed
    /// claim, and an exhausted shared Execution budget. Every rejection happens before a job is
    /// released, so a refused claim has no Git, source or provider effect whatsoever.
    ///
    /// `live` is asked about the ticket's *own* retained [`BindingRef`], never about a value the
    /// caller supplied. Comparing a caller-supplied fingerprint against the same ticket's stored
    /// fingerprint validates nothing — it is trivially satisfiable by reading the ticket first —
    /// so generation validation is exactly two independent facts: the native launch this ledger
    /// itself recognized for this exact actor, and the current liveness of that generation as the
    /// binding guard reports it now.
    ///
    /// A granted claim reserves one bounded admission lease, which is retained until positive
    /// settlement proves it may be released.
    pub fn claim(
        &mut self,
        detail_ref: &str,
        channel: &str,
        now_ms: u64,
        live: impl FnOnce(&BindingRef) -> bool,
    ) -> ClaimOutcome {
        let Some(ticket) = self.tickets.get(detail_ref) else {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        };
        if ticket.channel != channel {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        }
        if now_ms >= ticket.deadline_ms {
            return ClaimOutcome::Rejected(FailureCode::Deadline);
        }
        let TicketState::Launched { tool_use_id } = &ticket.state else {
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        };
        let tool_use_id = tool_use_id.clone();
        if !live(&ticket.binding) {
            return ClaimOutcome::Rejected(FailureCode::WorkspaceAuthority);
        }
        // The lease is taken from the daemon's one shared Execution budget before the job is
        // released, so concurrent physical helper work contends with ordinary worker work rather
        // than being bounded by a private counter or by counts a helper reports.
        if !self.leases.reserve(detail_ref, &ticket.binding) {
            return ClaimOutcome::Rejected(FailureCode::Capacity);
        }
        let Some(ticket) = self.tickets.get_mut(detail_ref) else {
            self.leases.release(detail_ref);
            return ClaimOutcome::Rejected(FailureCode::InvalidDetail);
        };
        ticket.state = TicketState::Claimed(ClaimedWork {
            tool_use_id,
            frame: None,
            post: None,
            lease: true,
        });
        // The configured budget was fixed at mint time, before the model's own delay running the
        // foreground command. Clamping it to the ticket's actual remaining time here — the only
        // point that shares the mint-time clock with `deadline_ms` — is what keeps the helper's own
        // diagnostic wait from outliving this exact issued ticket instead of a fresh full window.
        let mut job = ticket.job.clone();
        job.budgets.deadline_ms = job
            .budgets
            .deadline_ms
            .min(ticket.deadline_ms.saturating_sub(now_ms));
        ClaimOutcome::Granted(Box::new(job))
    }

    /// Releases the bounded lease exactly once, and only on positive settlement proof.
    ///
    /// Positive proof is the final frame with exactly settled child accounting *and* the matching
    /// successful post. Anything less leaves the lease held, so capacity lost to unprovable
    /// cleanup stays lost instead of being silently reclaimed.
    ///
    /// Callers decide *when* to ask. Quarantined work releases as soon as cleanup proves settled,
    /// because nothing further will ever consume it. Live claimed work retains its lease until the
    /// daemon consumes its proof or an absolute-deadline sweep releases capacity while retaining
    /// the known result; [`Self::settled`] validates that proof directly.
    fn release_settled_lease(work: &mut ClaimedWork, detail_ref: &str, leases: &mut LeasePool) {
        if work.lease
            && work.post == Some(true)
            && work
                .frame
                .as_ref()
                .is_some_and(|frame| frame.children.settled())
        {
            work.lease = false;
            leases.release(detail_ref);
        }
    }

    /// Records the helper's final frame; ordering against the post-hook does not matter.
    ///
    /// A frame is accepted for claimed work and for quarantined `TicketState::Uncertain` work
    /// alike. The second case is cleanup-only settlement: the documented contract is that a
    /// revoked operation still accepts proof that its physical work finished, and refusing that
    /// proof would make the bounded lease unreleasable forever. Acceptance here never revives
    /// authority — [`Self::delivery`] and [`Self::settled`] both refuse quarantined work
    /// unconditionally.
    pub fn settle_frame(&mut self, result: HelperResult) -> Result<(), FailureCode> {
        result.validate()?;
        let reference = result.detail_ref.clone();
        let Some(ticket) = self.tickets.get_mut(&reference) else {
            return Err(FailureCode::InvalidDetail);
        };
        let cleanup_only = matches!(ticket.state, TicketState::Uncertain(_));
        let (TicketState::Claimed(work) | TicketState::Uncertain(work)) = &mut ticket.state else {
            return Err(FailureCode::InvalidDetail);
        };
        if work.frame.is_some() {
            return Err(FailureCode::Conflict);
        }
        if matches!(result.outcome, HelperOutcome::Complete { .. })
            && result
                .payload
                .as_ref()
                .is_none_or(|payload| payload.operation() != ticket.job.operation)
        {
            return Err(FailureCode::ExecutionProfile);
        }
        work.frame = Some(result);
        if cleanup_only {
            Self::release_settled_lease(work, &reference, &mut self.leases);
        }
        Ok(())
    }

    /// Returns how many shared-budget slots claimed or quarantined work currently retains.
    pub fn active_claims(&self) -> usize {
        self.leases.len()
    }

    /// Releases the lease of one live claimed operation whose settled token the daemon consumed.
    ///
    /// Requires the same positive proof as any other release, plus ownership by `binding`. On
    /// release it retires the fully consumed ticket, freeing both its admission slot and bounded
    /// ticket capacity. Quarantined work is untouched here: it released on cleanup proof and has
    /// no token to consume.
    pub fn release_settled(&mut self, detail_ref: &str, binding: [u8; 32]) -> bool {
        let released = {
            let Some(ticket) = self.tickets.get_mut(detail_ref) else {
                return false;
            };
            if ticket.binding.fingerprint() != binding {
                return false;
            }
            let TicketState::Claimed(work) = &mut ticket.state else {
                return false;
            };
            if work.post != Some(true)
                || !work
                    .frame
                    .as_ref()
                    .is_some_and(|frame| frame.children.settled())
            {
                return false;
            }
            work.lease = false;
            true
        };
        if released {
            self.leases.release(detail_ref);
            self.tickets.remove(detail_ref);
        }
        released
    }

    /// Retires a delivered pre-claim Edit terminal result, freeing its ticket-capacity entry.
    ///
    /// Only `ExpiredNoEffect` has proof that no helper was claimed and therefore no physical work
    /// needs later correlation. Claimed uncertainty remains retained for cleanup-only evidence.
    pub fn retire_no_effect_edit(&mut self, detail_ref: &str, binding: [u8; 32]) -> bool {
        let removable = self.tickets.get(detail_ref).is_some_and(|ticket| {
            ticket.binding.fingerprint() == binding
                && ticket.job.operation == HelperOperation::Edit
                && ticket.state == TicketState::ExpiredNoEffect
        });
        removable
            .then(|| self.tickets.remove(detail_ref))
            .flatten()
            .is_some()
    }

    /// Records the matching `Bash` post-hook for one claimed ticket.
    ///
    /// This post is special: it settles the helper's own operation and must not be treated as a
    /// generic native edit that invalidates the result the helper just produced. Only later
    /// unrelated native posts advance the native epoch.
    /// Quarantined work still accepts its own late post for the same cleanup-only reason as
    /// [`Self::settle_frame`]; the post never restores authority.
    pub fn settle_post(&mut self, tool_use_id: &str, success: bool) -> Result<(), FailureCode> {
        let Some((reference, ticket)) = self
            .tickets
            .iter_mut()
            .find(|(_, ticket)| ticket.correlates(tool_use_id))
        else {
            return Err(FailureCode::InvalidDetail);
        };
        let reference = reference.clone();
        let cleanup_only = matches!(ticket.state, TicketState::Uncertain(_));
        let (TicketState::Claimed(work) | TicketState::Uncertain(work)) = &mut ticket.state else {
            return Err(FailureCode::InvalidDetail);
        };
        if work.post.is_some() {
            return Err(FailureCode::Conflict);
        }
        work.post = Some(success);
        if cleanup_only {
            Self::release_settled_lease(work, &reference, &mut self.leases);
        }
        Ok(())
    }

    /// Returns whether a completed post belongs to a helper this ledger launched.
    ///
    /// The hook path uses this to keep a helper's own post from invalidating its own result.
    pub fn owns_post(&self, tool_use_id: &str) -> bool {
        self.tickets
            .values()
            .any(|ticket| ticket.correlates(tool_use_id))
    }

    /// Returns the daemon-private settled token for one positively settled Claude operation.
    ///
    /// Returns `None` unless every requirement holds at once: the handle exists and is owned by the
    /// supplied binding generation; the ticket is still `Claimed` rather than quarantined; the
    /// retained frame names this exact handle; the outcome is `Complete` with exactly settled child
    /// accounting; the matching post arrived and succeeded; the helper and child executable
    /// identities are present and well formed. The lease may already have been released by an
    /// absolute-deadline sweep, which preserves known evidence for later retrieval while avoiding
    /// permanent admission loss. Because the token has no public constructor and no `Deserialize`,
    /// this is the only way one exists.
    ///
    /// Reads only ledger state: no Git, source, provider or process effect.
    pub fn settled(&self, detail_ref: &str, binding: [u8; 32]) -> Option<SettledClaudeOperation> {
        let ticket = self.tickets.get(detail_ref)?;
        if ticket.binding.fingerprint() != binding {
            return None;
        }
        let TicketState::Claimed(work) = &ticket.state else {
            return None;
        };
        if work.post != Some(true) {
            return None;
        }
        let frame = work.frame.as_ref()?;
        if frame.detail_ref != detail_ref
            || !matches!(frame.outcome, HelperOutcome::Complete { .. })
            || !frame.children.settled()
        {
            return None;
        }
        Some(SettledClaudeOperation {
            job: ticket.job.clone(),
            result: frame.clone(),
            binding: ticket.binding.clone(),
            helper: ticket.helper.clone(),
            children: ticket.children.clone(),
        })
    }

    /// Reports whether a claimed operation may become model-visible yet.
    ///
    /// Visibility requires both the final helper frame and the exact successful post-hook, in
    /// either order. A failed post, a missing half, or an uncertain ticket produces an honest
    /// finite outcome rather than a silent success.
    pub fn delivery(&self, detail_ref: &str) -> Delivery {
        let Some(ticket) = self.tickets.get(detail_ref) else {
            return Delivery::Failed(FailureCode::InvalidDetail);
        };
        match &ticket.state {
            // Quarantine is unconditional and permanent: a late cleanup frame or post may still be
            // recorded against retained identity, but it can never produce a visible result.
            TicketState::Uncertain(_) if ticket.job.operation == HelperOperation::Edit => {
                Delivery::OutcomeUnknown(Box::new(ticket.job.clone()))
            }
            TicketState::Uncertain(_) => Delivery::Failed(FailureCode::Deadline),
            // Delivery re-checks settlement rather than trusting that the frame was validated on
            // the way in: a completed result requires exact settled child accounting *and* the
            // final frame *and* the matching successful post, in either order.
            TicketState::Claimed(ClaimedWork {
                frame: Some(frame),
                post: Some(true),
                ..
            }) if frame.children.settled() => Delivery::Ready(Box::new(frame.clone())),
            TicketState::Claimed(ClaimedWork {
                frame: Some(_),
                post: Some(true),
                ..
            }) => Delivery::Failed(FailureCode::Deadline),
            TicketState::Claimed(ClaimedWork {
                post: Some(false), ..
            }) if ticket.job.operation == HelperOperation::Edit => {
                Delivery::OutcomeUnknown(Box::new(ticket.job.clone()))
            }
            TicketState::Claimed(ClaimedWork {
                post: Some(false), ..
            }) => Delivery::Failed(FailureCode::Cancelled),
            TicketState::Claimed(_) => Delivery::Waiting,
            TicketState::Minted | TicketState::Launched { .. } => Delivery::Waiting,
            TicketState::ExpiredNoEffect => {
                Delivery::DeadlineNoEffect(Box::new(ticket.job.clone()))
            }
        }
    }

    /// Expires overdue tickets, distinguishing a launch that never happened from claimed work.
    ///
    /// An unclaimed non-Edit ticket is dropped: nothing ran. An unclaimed Edit is retained as
    /// `ExpiredNoEffect` so retrieval durably settles `deadline_no_effect`. A claimed ticket that
    /// never settled becomes `TicketState::Uncertain`; claimed Edit retrieval reports
    /// `outcome_unknown`, and its admission stays quarantined instead of being silently reused. A
    /// claimed ticket whose final frame and matching successful post already arrived before the
    /// sweep retains its proven result but releases its bounded lease, so an uninspected success
    /// cannot occupy shared execution capacity forever.
    pub fn expire(&mut self, now_ms: u64) {
        let leases = &mut self.leases;
        self.tickets.retain(|reference, ticket| {
            if now_ms < ticket.deadline_ms {
                return true;
            }
            if ticket.positively_settled() {
                if let TicketState::Claimed(work) = &mut ticket.state {
                    LaunchLedger::release_settled_lease(work, reference, leases);
                }
                return true;
            }
            ticket.quarantine(reference, leases)
        });
    }

    /// Retires unclaimed work and quarantines claimed work for one revoked binding generation.
    ///
    /// Stop closes binding admission before calling this method; a ticket for that generation can
    /// no longer be claimed, so a late helper is rejected rather than being allowed to run against
    /// stale authority. The caller first extracts ready Edits for known receipt settlement. Only
    /// remaining work that provably never ran is retired: a `Minted` or `Launched` ticket had no
    /// claim, so nothing physical exists to settle and dropping it has no effect.
    ///
    /// Claimed work is *not* deleted, and its claimed identity is *not* discarded. A claimed,
    /// disconnected, expired or unsettled ticket keeps its `tool_use_id`, frame slot and lease and
    /// becomes `TicketState::Uncertain`, so its admission stays quarantined and it can still
    /// accept cleanup-only settlement afterwards. Replacing that identity with a stateless marker
    /// was the previous defect: it made the documented late cleanup impossible to correlate and
    /// left the bounded lease unreleasable. Authority never returns, because [`Self::claim`] admits
    /// `Launched` alone and both [`Self::delivery`] and [`Self::settled`] refuse quarantined work.
    /// Returned Edit terminals let the Worker settle already-prepared receipts as no-effect or
    /// unknown even after the binding itself has been stopped.
    pub fn revoke(
        &mut self,
        binding: [u8; 32],
    ) -> Vec<(String, HelperJob, crate::changes::edit::EditOutcome)> {
        let leases = &mut self.leases;
        let mut terminals = Vec::new();
        self.tickets.retain(|reference, ticket| {
            if ticket.binding.fingerprint() != binding {
                return true;
            }
            let terminal = match ticket.state {
                TicketState::Minted
                | TicketState::Launched { .. }
                | TicketState::ExpiredNoEffect
                    if ticket.job.operation == HelperOperation::Edit =>
                {
                    Some(crate::changes::edit::EditOutcome::DeadlineNoEffect)
                }
                TicketState::Claimed(_) | TicketState::Uncertain(_)
                    if ticket.job.operation == HelperOperation::Edit =>
                {
                    Some(crate::changes::edit::EditOutcome::OutcomeUnknown)
                }
                _ => None,
            };
            if let Some(outcome) = terminal {
                terminals.push((reference.clone(), ticket.job.clone(), outcome));
            }
            ticket.quarantine(reference, leases)
        });
        terminals
    }

    /// Returns the exact helper command of a ticket the native pre-hook never recognized.
    ///
    /// A `Minted` ticket means the model has not run the command, or ran a modified form that was
    /// refused; retrieval repeats the command so the model can run it exactly instead of polling.
    pub fn unrun_command(&self, detail_ref: &str) -> Option<&str> {
        self.tickets
            .get(detail_ref)
            .filter(|ticket| ticket.state == TicketState::Minted)
            .map(LaunchTicket::command)
    }

    /// Returns whether one handle exists and belongs to the supplied binding generation.
    ///
    /// Retrieval uses this before reading any outcome, so a handle copied into another actor's
    /// or another generation's call can never surface a result it does not own.
    pub fn owned_by(&self, detail_ref: &str, binding: [u8; 32]) -> bool {
        self.tickets
            .get(detail_ref)
            .is_some_and(|ticket| ticket.binding.fingerprint() == binding)
    }

    /// Returns ready Edit handles still owned by `binding` for stop-time receipt settlement.
    ///
    /// The caller has already closed binding admission, removing every claim path. These handles
    /// are already claimed and positively proven, so completing their prepared receipts is cleanup
    /// only and cannot start new helper work. The caller extracts them and revokes all remaining
    /// tickets under the same ledger lock, preventing a late settlement from falling between steps.
    pub fn ready_edit_references(&self, binding: [u8; 32]) -> Vec<String> {
        self.tickets
            .iter()
            .filter(|(_, ticket)| {
                ticket.binding.fingerprint() == binding
                    && ticket.job.operation == HelperOperation::Edit
                    && ticket.positively_settled()
            })
            .map(|(reference, _)| reference.clone())
            .collect()
    }

    /// Returns the number of outstanding tickets for bounded-capacity assertions.
    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    /// Returns whether no ticket is outstanding.
    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the fixed binding generation every ledger fixture mints under.
    fn binding_fixture() -> BindingRef {
        BindingRef::fixture("agent", "channel", 1)
    }

    /// Reports the fixture generation as currently live.
    fn live(binding: &BindingRef) -> bool {
        binding == &binding_fixture()
    }

    /// Reports every generation as no longer live, as a revoked or replaced one would be.
    fn stale(_: &BindingRef) -> bool {
        false
    }

    /// Returns one accepted executable identity fixture with a well-formed digest.
    fn identity(path: &str) -> AcceptedIdentity {
        AcceptedIdentity::new(PathBuf::from(path), "fixture", &"ab".repeat(32))
            .expect("fixture identity is well formed")
    }

    /// Builds one valid Rust-profile job with entirely daemon-selected values.
    fn job() -> HelperJob {
        HelperJob {
            protocol: HELPER_PROTOCOL,
            operation: HelperOperation::Context,
            candidate: PathBuf::from("/private/tmp/work"),
            git: PathBuf::from("/usr/bin/git"),
            canonical_root: Some(PathBuf::from("/private/tmp/work")),
            scope: Some(HelperScope {
                worktree_id: "worktree".into(),
                incarnation: 1,
                root: PathBuf::from("/private/tmp/work"),
                repository_root: PathBuf::from("/private/tmp/work"),
                git_common_dir: PathBuf::from(".git"),
                native_root_identity: [1; 32],
                authority_epoch: 1,
            }),
            baseline: None,
            provider: Some(HelperProvider {
                executable: PathBuf::from("/Users/pluto/.local/bin/rust-analyzer"),
                version: "rust-analyzer fixture".into(),
                language: HelperLanguage::Rust,
                rust_settings: Some(RustEffectiveSettings {
                    cache_priming: false,
                    proc_macro: false,
                }),
                pyright: None,
                typescript: None,
                toolchain: "fixture".into(),
                cargo: Some(PathBuf::from("/usr/bin/true")),
                cargo_version: Some("cargo fixture".into()),
                rustc: Some(PathBuf::from("/usr/bin/true")),
                rustc_version: Some("rustc fixture".into()),
                trust: "fixture".into(),
                cache_namespace: "/private/tmp/ns-1".into(),
            }),
            edit_source: None,
            parameters: serde_json::json!({"query": "x"}),
            budgets: HelperBudgets {
                output_bytes: 4096,
                processes: 2,
                deadline_ms: 30_000,
            },
        }
    }

    /// Builds one valid daemon-selected Edit job over the fixture scope and expected source bytes.
    fn edit_job() -> HelperJob {
        let mut job = job();
        job.operation = HelperOperation::Edit;
        job.provider = None;
        job.edit_source = Some(HelperEditSource {
            path: "main.rs".into(),
            present: true,
            digest: Some(*blake3::hash(b"old").as_bytes()),
            length: 3,
            sequence: 1,
            source_revision: "revision".into(),
            observation_ref: "context".into(),
        });
        job.parameters = serde_json::json!({
            "operation_id":"edit-operation",
            "path":"main.rs",
            "source_ref":"context",
            "content":"new"
        });
        job
    }

    /// Mints an Edit ticket in the same closed ledger used by lifecycle settlement tests.
    fn edit_ledger() -> (LaunchLedger, String, HelperActor) {
        let (mut ledger, reference, actor) = ledger();
        ledger.tickets.get_mut(&reference).unwrap().job = edit_job();
        (ledger, reference, actor)
    }

    /// Mints one ticket on a fixed binding/actor/channel with the fixed helper command.
    fn ledger() -> (LaunchLedger, String, HelperActor) {
        let mut ledger = LaunchLedger::new(Arc::new(Mutex::new(
            crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
                total_running: CLAIM_CEILING,
                per_owner_running: CLAIM_CEILING,
                per_owner_queued: 1,
                total_queued: 2,
                interactive_burst: 8,
            })
            .expect("fixture limits"),
        )));
        let actor = HelperActor::new("agent", Some("session")).unwrap();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger
            .mint(
                "detail-1",
                binding_fixture(),
                actor.clone(),
                "channel",
                command,
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            )
            .unwrap();
        (ledger, "detail-1".into(), actor)
    }

    /// Mints a ticket on the same fixed binding/actor/channel, but with a caller-chosen expected
    /// command, so recognition-matching tests can exercise commands `helper_command` itself would
    /// not currently emit (for example, an old-style always-quoted command).
    fn ledger_with_command(command: &str) -> (LaunchLedger, String, HelperActor) {
        let mut ledger = LaunchLedger::new(Arc::new(Mutex::new(
            crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
                total_running: CLAIM_CEILING,
                per_owner_running: CLAIM_CEILING,
                per_owner_queued: 1,
                total_queued: 2,
                interactive_burst: 8,
            })
            .expect("fixture limits"),
        )));
        let actor = HelperActor::new("agent", Some("session")).unwrap();
        ledger
            .mint(
                "detail-1",
                binding_fixture(),
                actor.clone(),
                "channel",
                command.to_owned(),
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            )
            .unwrap();
        (ledger, "detail-1".into(), actor)
    }

    /// Ordinary daemon work and a Claude helper claim contend for exactly one configured budget.
    ///
    /// This is the distinguishing test for the shared-admission contract. With a ledger-private
    /// counter both directions pass trivially: ordinary work never sees the helper's slot and the
    /// helper never sees ordinary work's. Here the budget is exhausted by ordinary work first, so
    /// the claim must be refused for capacity; and once the helper holds a claim, ordinary work
    /// must be unable to take that same slot back.
    #[test]
    fn ordinary_work_and_a_claude_helper_contend_for_one_configured_budget() {
        use crate::execution::{Admission, AdmissionClass, OwnerId};
        let admission = Arc::new(Mutex::new(
            crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
                total_running: 2,
                per_owner_running: 2,
                per_owner_queued: 1,
                total_queued: 2,
                interactive_burst: 8,
            })
            .expect("fixture limits"),
        ));
        // One ordinary daemon operation, submitted exactly as the worker submits discovery,
        // snapshot and provider work.
        let ordinary = |admission: &Arc<Mutex<crate::execution::AdmissionController>>| {
            admission.lock().unwrap().submit(
                OwnerId::new(String::from("ordinary-owner")).unwrap(),
                AdmissionClass::Interactive,
            )
        };
        let Admission::Granted(first) = ordinary(&admission) else {
            panic!("the first ordinary operation must be admitted")
        };
        let Admission::Granted(second) = ordinary(&admission) else {
            panic!("the second ordinary operation must be admitted")
        };

        let mut ledger = LaunchLedger::new(admission.clone());
        let actor = HelperActor::new("agent", Some("session")).unwrap();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger
            .mint(
                "detail-1",
                binding_fixture(),
                actor.clone(),
                "channel",
                command,
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            )
            .expect("ticket mints");
        assert_eq!(
            ledger.recognize(
                "channel",
                &LaunchLedger::helper_command(
                    std::path::Path::new("/usr/local/bin/agent-ide"),
                    std::path::Path::new("/private/tmp/rt"),
                    "attach",
                    "detail-1",
                ),
                false,
                "call",
                &actor,
                0,
            ),
            LaunchRecognition::Recognized
        );

        // Ordinary work holds the whole budget, so no helper child may be released.
        assert_eq!(
            ledger.claim("detail-1", "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::Capacity)
        );
        assert_eq!(ledger.active_claims(), 0);

        // Freeing one ordinary slot is what makes the helper claimable; nothing else changed.
        admission.lock().unwrap().release(first).expect("release");
        assert!(matches!(
            ledger.claim("detail-1", "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(ledger.active_claims(), 1);

        // The helper now genuinely occupies that shared slot: ordinary work cannot retake it.
        assert!(
            !matches!(ordinary(&admission), Admission::Granted(_)),
            "a claimed Claude helper must consume the same global budget as ordinary work"
        );
        admission.lock().unwrap().release(second).expect("release");
    }

    /// Only strict macOS operator evidence enables the Claude path; Linux stays unavailable.
    #[test]
    fn only_strict_macos_operator_profile_is_accepted() {
        let strict = ClaudeOperatorProfile {
            enabled: true,
            fail_if_unavailable: true,
            allow_unsandboxed_commands: false,
            no_matching_excluded_commands: true,
            scope_declared: true,
            read_roots: None,
            read_denies: Vec::new(),
            platform: HelperPlatform::MacOs,
        };
        assert_eq!(strict.validate(), Ok(()));
        for weakened in [
            ClaudeOperatorProfile {
                enabled: false,
                ..strict.clone()
            },
            ClaudeOperatorProfile {
                allow_unsandboxed_commands: true,
                ..strict.clone()
            },
            ClaudeOperatorProfile {
                fail_if_unavailable: false,
                ..strict.clone()
            },
            ClaudeOperatorProfile {
                platform: HelperPlatform::Linux,
                ..strict.clone()
            },
        ] {
            assert_eq!(weakened.validate(), Err(FailureCode::ExecutionProfile));
        }
    }

    /// Whole-tree check authority follows the accepted Claude read grants and exclusions.
    #[test]
    fn claude_profile_limits_project_checks_to_whole_tree_read() {
        let profile = ClaudeOperatorProfile {
            enabled: true,
            fail_if_unavailable: true,
            allow_unsandboxed_commands: false,
            no_matching_excluded_commands: true,
            scope_declared: true,
            read_roots: None,
            read_denies: Vec::new(),
            platform: HelperPlatform::MacOs,
        };
        let root = Path::new("/private/tmp/project");
        assert!(profile.declares_whole_tree_read(root));
        let mut limited = profile.clone();
        limited.read_roots = Some(vec![PathBuf::from("/private/tmp/other")]);
        assert!(!limited.declares_whole_tree_read(root));
        limited.read_roots = Some(vec![PathBuf::from("/private/tmp")]);
        limited.read_denies.push("/private/tmp/other/*.key".into());
        assert!(limited.declares_whole_tree_read(root));
        limited
            .read_denies
            .push("/private/tmp/project/secret/*.key".into());
        assert!(!limited.declares_whole_tree_read(root));
    }

    /// The accepted Rust helper payload disables both switches and adds nothing else.
    #[test]
    fn rust_helper_configuration_disables_both_switches() {
        let settings = RustEffectiveSettings {
            cache_priming: false,
            proc_macro: false,
        };
        assert_eq!(
            settings.configuration(),
            serde_json::json!({
                "cachePriming": {"enable": false},
                "procMacro": {"enable": false},
            })
        );
    }

    /// Rust helper settings may never re-enable priming or proc-macro expansion.
    #[test]
    fn rust_profile_refuses_cache_priming_and_proc_macro() {
        let mut job = job();
        assert_eq!(job.validate(), Ok(()));
        job.provider.as_mut().unwrap().rust_settings = Some(RustEffectiveSettings {
            cache_priming: false,
            proc_macro: true,
        });
        assert_eq!(job.validate(), Err(FailureCode::ExecutionProfile));
        job.provider.as_mut().unwrap().language = HelperLanguage::Go;
        job.provider.as_mut().unwrap().rust_settings = None;
        job.provider.as_mut().unwrap().toolchain = "/usr/bin/go".into();
        job.provider.as_mut().unwrap().cargo = None;
        job.provider.as_mut().unwrap().cargo_version = None;
        job.provider.as_mut().unwrap().rustc = None;
        job.provider.as_mut().unwrap().rustc_version = None;
        assert_eq!(job.validate(), Ok(()));
    }

    /// Python helper frames require matching launcher-selected script and Node identities.
    #[test]
    fn python_profile_requires_complete_agreeing_launcher_identities() {
        let mut job = job();
        let provider = job.provider.as_mut().unwrap();
        provider.executable = PathBuf::from("/private/tmp/pyright-langserver");
        provider.version = "pyright fixture".into();
        provider.language = HelperLanguage::Python;
        provider.rust_settings = None;
        provider.toolchain = "node fixture".into();
        provider.cargo = None;
        provider.cargo_version = None;
        provider.rustc = None;
        provider.rustc_version = None;
        provider.pyright = Some(HelperPyrightProfile {
            script: provider.executable.clone(),
            script_identity: provider.version.clone(),
            script_blake3: "ab".repeat(32),
            node: PathBuf::from("/private/tmp/node"),
            node_identity: provider.toolchain.clone(),
            node_blake3: "cd".repeat(32),
        });
        assert_eq!(job.validate(), Ok(()));
        job.provider
            .as_mut()
            .unwrap()
            .pyright
            .as_mut()
            .unwrap()
            .node = PathBuf::from("node");
        assert_eq!(job.validate(), Err(FailureCode::ExecutionProfile));
    }

    /// A bare copied handle without a recognized native launch never releases a job.
    #[test]
    fn copied_reference_without_native_launch_is_rejected() {
        let (mut ledger, reference, _actor) = ledger();
        assert_eq!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// Only byte-exact foreground commands are recognized; nothing else is retained.
    #[test]
    fn recognition_requires_exact_bytes_and_foreground() {
        let (mut ledger, _, actor) = ledger();
        let exact = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize(
                "channel",
                &format!("{exact} ; rm -rf /"),
                false,
                "call",
                &actor,
                0
            ),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("channel", "cargo test", false, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("channel", &exact, true, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("channel", &exact, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert_eq!(
            ledger.recognize("channel", &exact, false, "call-2", &actor, 0),
            LaunchRecognition::Ignored
        );
    }

    /// An old-style always-quoted expected command still recognizes a fully unquoted reproduction
    /// of the same argv.
    #[test]
    fn unquoted_reproduction_of_a_quoted_command_is_recognized() {
        let quoted = "'/usr/local/bin/agent-ide' claude-worker --runtime-dir '/private/tmp/rt' \
                       --attachment 'attach' --detail-ref 'detail-1'";
        let (mut ledger, _, actor) = ledger_with_command(quoted);
        let unquoted = "/usr/local/bin/agent-ide claude-worker --runtime-dir /private/tmp/rt --attachment attach --detail-ref detail-1";
        assert_eq!(
            ledger.recognize("channel", unquoted, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
    }

    /// An old-style always-quoted expected command still recognizes a reproduction where every
    /// word is double-quoted instead of single-quoted.
    #[test]
    fn double_quoted_reproduction_of_every_word_is_recognized() {
        let quoted = "'/usr/local/bin/agent-ide' claude-worker --runtime-dir '/private/tmp/rt' \
                       --attachment 'attach' --detail-ref 'detail-1'";
        let (mut ledger, _, actor) = ledger_with_command(quoted);
        let double_quoted = "\"/usr/local/bin/agent-ide\" \"claude-worker\" --runtime-dir \"/private/tmp/rt\" --attachment \"attach\" --detail-ref \"detail-1\"";
        assert_eq!(
            ledger.recognize("channel", double_quoted, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
    }

    /// `helper_command` single-quotes a value containing a space, and both the emitted quoted
    /// form and an equivalent double-quoted reproduction are recognized.
    #[test]
    fn a_space_containing_value_is_single_quoted_and_both_quotings_are_recognized() {
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt with space"),
            "attach",
            "detail-1",
        );
        assert!(
            command.contains("'/private/tmp/rt with space'"),
            "a space forces single-quoting: {command}"
        );

        let (mut first_ledger, _, first_actor) = ledger_with_command(&command);
        assert_eq!(
            first_ledger.recognize("channel", &command, false, "call", &first_actor, 0),
            LaunchRecognition::Recognized
        );

        let double_quoted = command.replace(
            "'/private/tmp/rt with space'",
            "\"/private/tmp/rt with space\"",
        );
        let (mut other_ledger, _, other_actor) = ledger_with_command(&command);
        assert_eq!(
            other_ledger.recognize("channel", &double_quoted, false, "call", &other_actor, 0),
            LaunchRecognition::Recognized
        );
    }

    /// A compound command, an added or removed argument, and a shell expansion are all ignored
    /// even though they share a prefix or most words with the exact expected command.
    #[test]
    fn shell_equivalent_variations_that_change_the_argv_are_ignored() {
        let (mut ledger, _, actor) = ledger();
        let exact = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize(
                "channel",
                &format!("{exact}; echo x"),
                false,
                "call",
                &actor,
                0
            ),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize(
                "channel",
                &format!("{exact} --extra"),
                false,
                "call",
                &actor,
                0
            ),
            LaunchRecognition::Ignored
        );
        let missing_argument = exact.rsplit_once(' ').expect("multi-word command").0;
        assert_eq!(
            ledger.recognize("channel", missing_argument, false, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
        let expanded = exact.replace("detail-1", "$HOME");
        assert_eq!(
            ledger.recognize("channel", &expanded, false, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("channel", &exact, true, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
    }

    /// A plain-path helper command contains no shell quote characters.
    #[test]
    fn helper_command_for_plain_paths_is_unquoted() {
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert!(
            !command.contains('\'') && !command.contains('"'),
            "plain paths must not be quoted: {command}"
        );
    }

    /// Wrong actor, wrong channel, stale generation and replayed claims are all refused.
    #[test]
    fn wrong_actor_channel_generation_and_replay_are_rejected() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("other-channel", &command, false, "wrong-channel", &actor, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );

        assert_eq!(
            ledger.claim(&reference, "other-channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
        assert_eq!(
            ledger.claim(&reference, "channel", 0, stale),
            ClaimOutcome::Rejected(FailureCode::WorkspaceAuthority)
        );

        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// A parent session running a subagent's exact command never arms that subagent's ticket.
    #[test]
    fn parent_session_launch_cannot_arm_a_subagent_ticket() {
        let (mut ledger, reference, _actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        let parent = HelperActor::new("session", None).unwrap();
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &parent, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// Only a ticket the pre-hook never recognized reports its command for a repeated instruction.
    #[test]
    fn unrun_command_is_reported_only_while_minted() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(ledger.unrun_command(&reference), Some(command.as_str()));
        assert_eq!(ledger.unrun_command("missing"), None);
        // A wrapped command is not recognized, so the ticket keeps asking for the exact one.
        let wrapped = format!("date '+%H:%M:%S'; {command}");
        assert_eq!(
            ledger.recognize("channel", &wrapped, false, "call", &actor, 0),
            LaunchRecognition::Ignored
        );
        assert_eq!(ledger.unrun_command(&reference), Some(command.as_str()));
        ledger.recognize("channel", &command, false, "call", &actor, 0);
        assert_eq!(ledger.unrun_command(&reference), None);
    }

    /// An unclaimed expiry disappears with no effect; a claimed one stays quarantined.
    #[test]
    fn unclaimed_expiry_vanishes_and_claimed_expiry_stays_uncertain() {
        {
            let (mut ledger, reference, _actor) = ledger();
            ledger.expire(1000);
            assert!(ledger.is_empty());
            assert_eq!(
                ledger.delivery(&reference),
                Delivery::Failed(FailureCode::InvalidDetail)
            );
        }

        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize("channel", &command, false, "call", &actor, 0);
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        ledger.expire(1000);
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Deadline)
        );
    }

    /// Edit expiry is typed: unclaimed proves no effect, while claimed can only be unknown.
    #[test]
    fn edit_expiry_distinguishes_unclaimed_no_effect_from_claimed_unknown() {
        let (mut ledger, reference, _) = edit_ledger();
        ledger.expire(1000);
        assert!(matches!(
            ledger.delivery(&reference),
            Delivery::DeadlineNoEffect(job) if job.operation == HelperOperation::Edit
        ));
        assert!(ledger.retire_no_effect_edit(&reference, binding_fixture().fingerprint()));
        assert!(
            ledger.is_empty(),
            "delivered no-effect edits free ticket capacity"
        );

        let (mut ledger, reference, actor) = edit_ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "edit-call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        ledger.expire(1000);
        assert!(matches!(
            ledger.delivery(&reference),
            Delivery::OutcomeUnknown(job) if job.operation == HelperOperation::Edit
        ));
    }

    /// A claim clamps the granted job's budget to the ticket's own remaining time.
    ///
    /// The fixture ticket carries a 1000ms absolute deadline and a 30_000ms configured budget. A
    /// delayed foreground launch that only claims at 700ms leaves 300ms actually remaining; the
    /// helper must receive that shrunk figure, never the full configured window, or its own
    /// diagnostic wait could outlive the ticket the daemon will expire.
    #[test]
    fn delayed_claim_clamps_the_granted_budget_to_the_tickets_remaining_time() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &actor, 700),
            LaunchRecognition::Recognized
        );
        let ClaimOutcome::Granted(granted) = ledger.claim(&reference, "channel", 700, live) else {
            panic!("a live claim before the ticket's own deadline must be granted");
        };
        assert_eq!(
            job().budgets.deadline_ms,
            30_000,
            "fixture budget is unclamped"
        );
        assert_eq!(granted.budgets.deadline_ms, 300);
    }

    /// A ready frame that arrives before the deadline survives the same sweep that expires it.
    ///
    /// Both settlement halves and exact child accounting already prove the operation's outcome;
    /// nothing about crossing the deadline afterward makes that proof less true.
    #[test]
    fn ready_frame_crossing_its_deadline_survives_the_expiry_sweep() {
        let (mut ledger, reference) = claimed();
        assert_eq!(ledger.settle_frame(completed(1, 1)), Ok(()));
        assert_eq!(ledger.settle_post("call", true), Ok(()));
        ledger.expire(1000);
        assert!(matches!(
            ledger.delivery(&reference),
            Delivery::Ready(frame) if frame.children.settled()
        ));
        assert_eq!(
            ledger.len(),
            1,
            "a proven result remains retrievable after its lease is released"
        );
        assert_eq!(
            ledger.active_claims(),
            0,
            "known work cannot pin capacity forever"
        );
        assert!(
            ledger
                .settled(&reference, binding_fixture().fingerprint())
                .is_some(),
            "lease release preserves the known result for bounded later consumption"
        );
    }

    /// A ready Edit frame crossing its deadline reports its known outcome, not `outcome_unknown`.
    ///
    /// This is the Edit-specific case the daemon must get right: the same expiry sweep that types
    /// unsettled claimed Edit work as `outcome_unknown` must not apply that same downgrade to Edit
    /// work whose frame and post already proved a concrete outcome.
    #[test]
    fn ready_edit_frame_crossing_its_deadline_reports_the_known_outcome() {
        let (mut ledger, reference, actor) = edit_ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "edit-call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(
            ledger.settle_frame(HelperResult {
                protocol: HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                outcome: HelperOutcome::Complete {
                    text: "replaced".into(),
                },
                children: ChildSettlement {
                    spawned: 1,
                    reaped: 1,
                },
                discovery: Vec::new(),
                payload: Some(HelperPayload::Edit {
                    outcome: crate::changes::edit::EditOutcome::Replaced,
                    source: Some(HelperSource {
                        path: "main.rs".into(),
                        present: true,
                        digest: Some(*blake3::hash(b"new").as_bytes()),
                        length: 3,
                    }),
                    diagnostics: crate::assistance::reply::EditDiagnostics::Unknown {},
                }),
            }),
            Ok(())
        );
        assert_eq!(ledger.settle_post("edit-call", true), Ok(()));
        ledger.expire(1000);
        assert!(matches!(
            ledger.delivery(&reference),
            Delivery::Ready(frame)
                if frame.children.settled()
                    && matches!(frame.outcome, HelperOutcome::Complete { .. })
        ));
    }

    /// Stop extracts a fully proven Edit before revocation and releases its ticket and lease.
    #[test]
    fn stop_drains_ready_edit_without_ticket_or_lease_capacity_loss() {
        let (mut ledger, reference, actor) = edit_ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "edit-call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        assert_eq!(ledger.settle_frame(completed_edit()), Ok(()));
        assert_eq!(ledger.settle_post("edit-call", true), Ok(()));
        assert_eq!(
            ledger.ready_edit_references(binding_fixture().fingerprint()),
            vec![reference.clone()]
        );
        assert!(matches!(ledger.delivery(&reference), Delivery::Ready(_)));
        let settled = ledger
            .settled(&reference, binding_fixture().fingerprint())
            .expect("Stop captures the known result before revocation");
        assert!(ledger.release_settled(&reference, binding_fixture().fingerprint()));
        assert_eq!(
            settled.result().payload.as_ref().unwrap().operation(),
            HelperOperation::Edit
        );
        assert!(ledger.revoke(binding_fixture().fingerprint()).is_empty());
        assert_eq!(ledger.active_claims(), 0);
        assert!(ledger.is_empty());
    }

    /// Proves a daemon-consumed successful helper releases both its shared lease and ticket slot.
    #[test]
    fn consumed_settlement_releases_ticket_capacity() {
        let (mut ledger, reference) = claimed();
        assert_eq!(ledger.settle_frame(completed(1, 1)), Ok(()));
        assert_eq!(ledger.settle_post("call", true), Ok(()));
        assert!(ledger.release_settled(&reference, binding_fixture().fingerprint()));
        assert_eq!(ledger.active_claims(), 0);
        assert!(ledger.is_empty());
    }

    /// Both settlement halves are required, and either arrival order reaches the same result.
    #[test]
    fn result_is_visible_only_after_frame_and_successful_post_in_either_order() {
        for frame_first in [true, false] {
            let (mut ledger, reference, actor) = ledger();
            let command = LaunchLedger::helper_command(
                std::path::Path::new("/usr/local/bin/agent-ide"),
                std::path::Path::new("/private/tmp/rt"),
                "attach",
                "detail-1",
            );
            ledger.recognize("channel", &command, false, "call", &actor, 0);
            ledger.claim(&reference, "channel", 0, live);
            assert_eq!(ledger.delivery(&reference), Delivery::Waiting);

            let result = HelperResult {
                protocol: HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                outcome: HelperOutcome::Complete { text: "ok".into() },
                children: ChildSettlement {
                    spawned: 2,
                    reaped: 2,
                },
                discovery: Vec::new(),
                payload: Some(context_payload()),
            };
            if frame_first {
                ledger.settle_frame(result).unwrap();
                assert_eq!(ledger.delivery(&reference), Delivery::Waiting);
                ledger.settle_post("call", true).unwrap();
            } else {
                ledger.settle_post("call", true).unwrap();
                assert_eq!(ledger.delivery(&reference), Delivery::Waiting);
                ledger.settle_frame(result).unwrap();
            }
            let Delivery::Ready(delivered) = ledger.delivery(&reference) else {
                panic!("both halves settled");
            };
            assert!(delivered.children.settled());
        }
    }

    /// A failed post produces an honest finite failure instead of a silent success.
    #[test]
    fn failed_post_never_delivers_a_result() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize("channel", &command, false, "call", &actor, 0);
        ledger.claim(&reference, "channel", 0, live);
        ledger
            .settle_frame(HelperResult {
                protocol: HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                outcome: HelperOutcome::Complete { text: "ok".into() },
                children: ChildSettlement {
                    spawned: 1,
                    reaped: 1,
                },
                discovery: Vec::new(),
                payload: Some(context_payload()),
            })
            .unwrap();
        ledger.settle_post("call", false).unwrap();
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Cancelled)
        );
    }

    /// A helper's own post is recognized as its own, so it never invalidates its own result.
    #[test]
    fn helper_post_is_owned_and_unrelated_posts_are_not() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize("channel", &command, false, "call", &actor, 0);
        ledger.claim(&reference, "channel", 0, live);
        assert!(ledger.owns_post("call"));
        assert!(!ledger.owns_post("some-native-edit"));
    }

    /// Unreaped children are visible in the frame so a stop cannot be reported as settled.
    #[test]
    fn unsettled_children_are_reported_rather_than_assumed() {
        assert!(
            !ChildSettlement {
                spawned: 3,
                reaped: 1
            }
            .settled()
        );
        let forged = HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: "detail-1".into(),
            outcome: HelperOutcome::Failed {
                code: FailureCode::Deadline,
            },
            children: ChildSettlement {
                spawned: 1,
                reaped: 4,
            },
            discovery: Vec::new(),
            payload: None,
        };
        assert_eq!(forged.validate(), Err(FailureCode::ExecutionProfile));
    }

    /// Builds one claimed ticket, returning the ledger and its handle, for settlement assertions.
    ///
    /// The sequence is the real one: mint, recognize the exact native launch, then claim once.
    fn claimed() -> (LaunchLedger, String) {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
        (ledger, reference)
    }

    /// Renders one complete helper frame with the exact child accounting under test.
    fn completed(spawned: u32, reaped: u32) -> HelperResult {
        HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: "detail-1".into(),
            outcome: HelperOutcome::Complete {
                text: "discovery observed".into(),
            },
            children: ChildSettlement { spawned, reaped },
            discovery: Vec::new(),
            payload: Some(context_payload()),
        }
    }

    /// Returns one valid successful Edit frame for a ticket named `detail-1`.
    fn completed_edit() -> HelperResult {
        HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: "detail-1".into(),
            outcome: HelperOutcome::Complete {
                text: "replaced".into(),
            },
            children: ChildSettlement {
                spawned: 1,
                reaped: 1,
            },
            discovery: Vec::new(),
            payload: Some(HelperPayload::Edit {
                outcome: crate::changes::edit::EditOutcome::Replaced,
                source: Some(HelperSource {
                    path: "main.rs".into(),
                    present: true,
                    digest: Some(*blake3::hash(b"new").as_bytes()),
                    length: 3,
                }),
                diagnostics: crate::assistance::reply::EditDiagnostics::Unknown {},
            }),
        }
    }

    /// Returns the minimal valid Context payload for ledger-only settlement tests.
    fn context_payload() -> HelperPayload {
        HelperPayload::Context {
            source: HelperSource {
                path: "main.rs".into(),
                present: false,
                digest: None,
                length: 0,
            },
            feedback: None,
            diagnostic_fingerprint: None,
            truncated: false,
        }
    }

    /// A completed result requires exact settled child accounting, not a self-reported success.
    ///
    /// The previously accepted `spawned=3/reaped=0` frame is refused at validation, so it never
    /// reaches daemon state, and delivery independently refuses to publish an unsettled frame even
    /// when the matching successful post has arrived.
    #[test]
    fn completed_result_requires_exact_positive_child_settlement() {
        assert_eq!(completed(3, 0).validate(), Err(FailureCode::Deadline));
        assert_eq!(completed(3, 2).validate(), Err(FailureCode::Deadline));
        assert_eq!(completed(3, 3).validate(), Ok(()));

        let (mut ledger, reference) = claimed();
        assert_eq!(
            ledger.settle_frame(completed(3, 0)),
            Err(FailureCode::Deadline)
        );
        assert_eq!(ledger.settle_post("call", true), Ok(()));
        // The refused frame changed nothing, so the operation is still awaiting its evidence.
        assert_eq!(ledger.delivery(&reference), Delivery::Waiting);
        assert_eq!(ledger.settle_frame(completed(2, 2)), Ok(()));
        assert!(matches!(
            ledger.delivery(&reference),
            Delivery::Ready(frame) if frame.children.settled()
        ));
    }

    /// Revocation retires only work that provably never ran and quarantines claimed work.
    ///
    /// A claimed ticket must survive revocation as uncertain: deleting it would let its
    /// disappearance be read as successful cleanup, and it must never regain authority afterwards.
    #[test]
    fn revocation_quarantines_claimed_work_instead_of_deleting_it() {
        let (mut ledger, reference) = claimed();
        ledger.revoke(binding_fixture().fingerprint());
        assert!(
            !ledger.is_empty(),
            "claimed work must be retained for cleanup settlement after revocation"
        );
        // Retained, but never successful and never reclaimable.
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Deadline)
        );
        assert_eq!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
        // The lease taken at claim is still held: revocation is not proof that the work stopped.
        assert_eq!(ledger.active_claims(), 1);
        // Positive late cleanup is *accepted* against the retained claimed identity, which is what
        // the contract promises and what releases the bounded lease. Rejecting it would strand the
        // lease permanently and make the documented cleanup path unreachable.
        assert_eq!(ledger.settle_frame(completed(1, 1)), Ok(()));
        assert_eq!(ledger.settle_post("call", true), Ok(()));
        assert_eq!(ledger.active_claims(), 0);
        // Cleanup releases capacity; it never revives authority.
        assert_eq!(
            ledger.delivery(&reference),
            Delivery::Failed(FailureCode::Deadline)
        );
        assert!(
            ledger
                .settled(&reference, binding_fixture().fingerprint())
                .is_none(),
            "revoked work can never mint a settled token"
        );
    }

    /// Only a positively settled, correlated, owned and leased operation mints a settled token.
    ///
    /// This is the constructor gate for [`SettledClaudeOperation`]: raw helper JSON, a wrong
    /// binding, a missing post and a frame naming another handle each mint nothing.
    #[test]
    fn settled_token_requires_exact_correlated_positive_settlement() {
        let (mut ledger, reference) = claimed();
        let owner = binding_fixture().fingerprint();

        // Frame only: the matching successful post has not arrived.
        assert_eq!(ledger.settle_frame(completed(2, 2)), Ok(()));
        assert!(ledger.settled(&reference, owner).is_none());

        // A frame naming a different handle never reaches this ticket at all.
        let mut foreign = completed(1, 1);
        foreign.detail_ref = "detail-elsewhere".into();
        assert_eq!(
            ledger.settle_frame(foreign),
            Err(FailureCode::InvalidDetail)
        );

        assert_eq!(ledger.settle_post("call", true), Ok(()));
        // Another generation owns nothing here, even with the correct handle.
        assert!(ledger.settled(&reference, [9; 32]).is_none());

        let settled = ledger
            .settled(&reference, owner)
            .expect("positively settled work mints its token");
        assert_eq!(settled.operation(), HelperOperation::Context);
        assert_eq!(settled.binding().fingerprint(), owner);
        assert!(settled.result().children.settled());
        assert_eq!(settled.helper().blake3().len(), 64);
        assert!(!settled.children().is_empty());
    }

    /// Total running slots the fixture's shared admission controller grants; the ledger owns none.
    const CLAIM_CEILING: usize = 3;

    /// Concurrent claimed operations are bounded by the shared Execution budget, not by reported
    /// child counts and not by any ceiling this ledger holds itself.
    #[test]
    fn concurrent_claims_are_bounded_by_a_daemon_reserved_lease() {
        let (mut ledger, _, actor) = ledger();
        for index in 1..=CLAIM_CEILING {
            let reference = format!("lease-{index}");
            let command = format!("cmd-lease-{index}");
            ledger
                .mint(
                    &reference,
                    binding_fixture(),
                    actor.clone(),
                    "channel",
                    command.clone(),
                    job(),
                    1000,
                    identity("/usr/local/bin/agent-ide"),
                    vec![identity("/usr/bin/git")],
                )
                .expect("ticket mints");
            assert_eq!(
                ledger.recognize(
                    "channel",
                    &command,
                    false,
                    &format!("call-{index}"),
                    &actor,
                    0
                ),
                LaunchRecognition::Recognized
            );
            assert!(matches!(
                ledger.claim(&reference, "channel", 0, live),
                ClaimOutcome::Granted(_)
            ));
        }
        assert_eq!(ledger.active_claims(), CLAIM_CEILING);

        // The original fixture ticket is recognized and in time, yet admission is full.
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert_eq!(
            ledger.claim("detail-1", "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::Capacity)
        );
    }

    /// A claim is refused when the ticket's own generation is no longer live.
    ///
    /// The probe is asked about the generation stored in the ticket, so this cannot be satisfied by
    /// echoing a fingerprint read out of that same ticket.
    #[test]
    fn claim_requires_current_liveness_of_the_tickets_own_generation() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        assert_eq!(
            ledger.recognize("channel", &command, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
        assert_eq!(
            ledger.claim(&reference, "channel", 0, stale),
            ClaimOutcome::Rejected(FailureCode::WorkspaceAuthority)
        );
        // A refused claim reserves nothing and leaves the ticket claimable by a live generation.
        assert_eq!(ledger.active_claims(), 0);
        assert!(matches!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Granted(_)
        ));
    }

    /// Both wire directions reject foreign revisions, unknown fields and over-budget frames.
    #[test]
    fn wire_frames_are_closed_and_bounded_in_both_directions() {
        let encoded = job().encode().unwrap();
        assert_eq!(HelperJob::decode(&encoded).unwrap(), job());
        for raw in [
            r#"{"protocol":3,"operation":"context","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":2,"operation":"context","candidate":"relative","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":2,"operation":"inspect","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1}}"#,
            r#"{"protocol":2,"operation":"context","candidate":"/a","git":"/b","canonical_root":null,"provider":null,"parameters":{},"budgets":{"output_bytes":1,"processes":1,"deadline_ms":1},"extra":1}"#,
        ] {
            assert!(HelperJob::decode(raw).is_err());
        }
        for raw in [
            r#"{"protocol":3,"detail_ref":"d","outcome":"failed","code":"internal","children":{"spawned":0,"reaped":0}}"#,
            r#"{"protocol":2,"detail_ref":"","outcome":"failed","code":"internal","children":{"spawned":0,"reaped":0}}"#,
            r#"{"protocol":2,"detail_ref":"d","outcome":"unknown","children":{"spawned":0,"reaped":0}}"#,
        ] {
            assert!(HelperResult::decode(raw).is_err());
        }
        assert!(HelperJob::decode(&"x".repeat(MAX_HELPER_FRAME_BYTES + 1)).is_err());
    }

    /// Discovery exit status accepts one Unix byte and rejects values that would fold to success.
    #[test]
    fn discovery_exit_status_rejects_negative_overflow_and_wrapping_values() {
        let frame = |exit_code| DiscoveryFrame {
            query: HelperQuery::ShowTopLevel,
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code,
            truncated: false,
        };
        for valid in [None, Some(0), Some(255)] {
            assert_eq!(frame(valid).validate(), Ok(()));
        }
        for invalid in [Some(-1), Some(256), Some(8_388_608), Some(16_777_216)] {
            assert_eq!(
                frame(invalid).validate(),
                Err(FailureCode::ExecutionProfile)
            );
        }
    }

    /// A worktree list from a repository with many worktrees (~10 KiB, seen live with ~50) is a
    /// valid discovery frame, and a full three-frame result with it still encodes (T39G).
    #[test]
    fn many_worktree_discovery_frames_fit_the_helper_result() {
        let frame = |stdout: Vec<u8>| DiscoveryFrame {
            query: HelperQuery::WorktreeListPorcelainZ,
            stdout,
            stderr: Vec::new(),
            exit_code: Some(0),
            truncated: false,
        };
        let live = frame(vec![b'w'; 10_169]);
        assert_eq!(live.validate(), Ok(()));
        let result = HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: "d".into(),
            outcome: HelperOutcome::Failed {
                code: FailureCode::Capacity,
            },
            children: ChildSettlement {
                spawned: 3,
                reaped: 3,
            },
            discovery: vec![frame(vec![0xff; MAX_DISCOVERY_STDOUT_BYTES]); 3],
            payload: None,
        };
        assert!(result.encode().is_ok());
        assert_eq!(
            frame(vec![b'w'; MAX_DISCOVERY_STDOUT_BYTES + 1]).validate(),
            Err(FailureCode::Capacity)
        );
    }

    /// Stop revokes first: a ticket on a revoked generation can no longer be claimed.
    #[test]
    fn revocation_removes_tickets_before_a_late_helper_can_claim() {
        let (mut ledger, reference, actor) = ledger();
        let command = LaunchLedger::helper_command(
            std::path::Path::new("/usr/local/bin/agent-ide"),
            std::path::Path::new("/private/tmp/rt"),
            "attach",
            "detail-1",
        );
        ledger.recognize("channel", &command, false, "call", &actor, 0);
        ledger.revoke(binding_fixture().fingerprint());
        assert!(ledger.is_empty());
        assert_eq!(
            ledger.claim(&reference, "channel", 0, live),
            ClaimOutcome::Rejected(FailureCode::InvalidDetail)
        );
    }

    /// Proves stop emits a durable no-effect terminal for an unclaimed prepared Edit ticket.
    #[test]
    fn revocation_emits_and_releases_unclaimed_edit_terminal() {
        let (mut ledger, reference, _) = edit_ledger();
        let terminals = ledger.revoke(binding_fixture().fingerprint());
        assert!(matches!(
            terminals.as_slice(),
            [(_, job, crate::changes::edit::EditOutcome::DeadlineNoEffect)]
                if job.operation == HelperOperation::Edit
        ));
        assert!(ledger.retire_no_effect_edit(&reference, binding_fixture().fingerprint()));
        assert!(ledger.is_empty());
    }

    /// Ticket admission is bounded and duplicate handles are refused rather than overwritten.
    #[test]
    fn ticket_admission_is_bounded_and_refuses_duplicates() {
        let (mut ledger, _, actor) = ledger();
        assert_eq!(
            ledger.mint(
                "detail-1",
                binding_fixture(),
                actor.clone(),
                "channel",
                "cmd".into(),
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            ),
            Err(FailureCode::Conflict)
        );
        let retained = ledger.len();
        let quoted_runtime = format!("/{}", "'".repeat(1024));
        let expanded = LaunchLedger::helper_command(
            std::path::Path::new("/agent ide"),
            std::path::Path::new(&quoted_runtime),
            "channel with space",
            "malformed-command",
        );
        assert!(expanded.len() > MAX_COMMAND_BYTES && expanded.contains("'\\''"));
        assert_eq!(
            ledger.mint(
                "malformed-command",
                binding_fixture(),
                actor.clone(),
                "channel",
                expanded,
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            ),
            Err(FailureCode::ExecutionProfile)
        );
        assert_eq!(ledger.len(), retained, "malformed input mints no ticket");
        for index in 1..MAX_TICKETS {
            let reference = format!("detail-{}", index + 1);
            ledger
                .mint(
                    &reference,
                    binding_fixture(),
                    actor.clone(),
                    "channel",
                    format!("cmd-{index}"),
                    job(),
                    1000,
                    identity("/usr/local/bin/agent-ide"),
                    vec![identity("/usr/bin/git")],
                )
                .unwrap();
        }
        assert_eq!(ledger.len(), MAX_TICKETS);
        assert_eq!(
            ledger.mint(
                "overflow",
                binding_fixture(),
                actor,
                "channel",
                "cmd-overflow".into(),
                job(),
                1000,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            ),
            Err(FailureCode::Capacity)
        );
    }
}
