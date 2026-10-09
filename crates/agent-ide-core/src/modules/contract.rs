//! `bundled-module/0` control vocabulary: identity, `hello`, request and response envelopes with
//! their fences, the capability set, readiness and coverage, and the typed failures.
//!
//! Every control object is one [`Control`] value tagged by `type`. Unknown fields and variants
//! are refused (`deny_unknown_fields`), so an incompatible peer fails at `hello` or at its first
//! malformed message instead of being half understood.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire::{AttachmentDecl, MAX_CHUNK, MAX_CONTROL, MAX_MESSAGE_ATTACHMENTS};

/// Protocol name offered and echoed in `hello`.
pub const PROTOCOL: &str = "bundled-module";
/// The one protocol version this build speaks.
pub const VERSION: u32 = 0;
/// Default startup budget (spawn to `hello` reply).
pub const STARTUP_BUDGET_MS: u64 = 30_000;
/// Default ordinary request budget.
pub const REQUEST_BUDGET_MS: u64 = 20_000;
/// Queued requests per channel beyond the one in flight; a full queue answers `busy`.
pub const QUEUE_DEPTH: u32 = 8;

/// A bundled module's identity: `bundled.<language id>`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModuleId(String);

impl ModuleId {
    /// The bundled module of the registered language `language` (its lowercase id).
    pub fn bundled(language: &str) -> Self {
        Self(format!("bundled.{language}"))
    }

    /// The language id this module serves.
    pub fn language(&self) -> &str {
        &self.0["bundled.".len()..]
    }

    /// The full id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModuleId {
    type Error = String;

    /// Accepts `bundled.` plus a nonempty lowercase ASCII identifier.
    fn try_from(value: String) -> Result<Self, String> {
        let valid = value.strip_prefix("bundled.").is_some_and(|language| {
            !language.is_empty()
                && language.len() <= 32
                && language
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
        if valid {
            Ok(Self(value))
        } else {
            Err(format!("invalid module id `{value}`"))
        }
    }
}

impl From<ModuleId> for String {
    /// The full id.
    fn from(id: ModuleId) -> Self {
        id.0
    }
}

impl fmt::Display for ModuleId {
    /// Writes the full id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Process role of one module instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Interactive language operations, and the provider when the language has one. A language
    /// without a provider (markup, style sheets) runs only this role, provider-free: it is the
    /// pure-language lane and never starts a language server.
    Analyzer,
    /// Long project checks, isolated from interactive requests.
    Checker,
}

impl Role {
    /// The lowercase name used on the command line and in fault records.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Analyzer => "analyzer",
            Self::Checker => "checker",
        }
    }

    /// Parses a command-line role.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Analyzer, Self::Checker]
            .into_iter()
            .find(|role| role.name() == name)
    }
}

/// Capability families of version 0 (§2.4 of the design).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    /// Project detection, commands, entry points and environment facts.
    #[serde(rename = "project")]
    Project,
    /// The batched per-file computation (outline, doc, syntax, tests, anchors).
    #[serde(rename = "analyze_source")]
    AnalyzeSource,
    /// Provider document symbols normalized into an outline.
    #[serde(rename = "outline")]
    Outline,
    /// File documentation line.
    #[serde(rename = "file_doc")]
    FileDoc,
    /// Insertion geometry.
    #[serde(rename = "insert_site")]
    InsertSite,
    /// Structural verdict or stdin probe plan.
    #[serde(rename = "syntax")]
    Syntax,
    /// Context, hover, definitions, references, workspace symbols and diagnostics.
    #[serde(rename = "semantic")]
    Semantic,
    /// Call hierarchy preparation, incoming and outgoing calls.
    #[serde(rename = "calls")]
    Calls,
    /// Rename as an edit proposal.
    #[serde(rename = "edit_plan.rename")]
    Rename,
    /// Formatter choice for a candidate text.
    #[serde(rename = "format_plan")]
    FormatPlan,
    /// Test selection and ids.
    #[serde(rename = "test_plan")]
    TestPlan,
    /// Test runner output parsing.
    #[serde(rename = "test_parse")]
    TestParse,
    /// Project check through core-run effects.
    #[serde(rename = "check_plan")]
    CheckPlan,
    /// Raw check output interpretation.
    #[serde(rename = "check_parse")]
    CheckParse,
    /// Whether the project check analyses a path.
    #[serde(rename = "analysis_scope")]
    AnalysisScope,
    /// `linkage/0`: anchors and core-routed resolve.
    #[serde(rename = "linkage")]
    Linkage,
    /// Interpretation of the language's launcher configuration and presence before or outside
    /// a session (launcher parse, doctor, check scheduling).
    #[serde(rename = "describe")]
    Describe,
}

impl Capability {
    /// Every capability, in declaration order.
    pub const ALL: [Self; 17] = [
        Self::Project,
        Self::AnalyzeSource,
        Self::Outline,
        Self::FileDoc,
        Self::InsertSite,
        Self::Syntax,
        Self::Semantic,
        Self::Calls,
        Self::Rename,
        Self::FormatPlan,
        Self::TestPlan,
        Self::TestParse,
        Self::CheckPlan,
        Self::CheckParse,
        Self::AnalysisScope,
        Self::Linkage,
        Self::Describe,
    ];
}

/// Whether a module implements a declared capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Implemented; a call may still answer `warming` or `unsupported` for one input.
    Supported,
    /// Declared and refused, as the in-process language does today.
    Unsupported,
}

/// One declared capability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDecl {
    /// The family.
    pub capability: Capability,
    /// Its version; 0 everywhere in this protocol version.
    pub version: u32,
    /// Implemented or refused.
    pub support: Support,
}

impl CapabilityDecl {
    /// Version 0 of `capability` with `support`.
    pub const fn v0(capability: Capability, support: Support) -> Self {
        Self {
            capability,
            version: 0,
            support,
        }
    }
}

/// The transport ceilings the core grants an instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Largest control body.
    pub max_control: u64,
    /// Largest chunk data.
    pub max_chunk: u64,
    /// Largest attachment total of one message.
    pub max_attachments: u64,
    /// Queued requests beyond the one in flight.
    pub queue: u32,
}

impl Default for Limits {
    /// The protocol defaults.
    fn default() -> Self {
        Self {
            max_control: MAX_CONTROL as u64,
            max_chunk: MAX_CHUNK as u64,
            max_attachments: MAX_MESSAGE_ATTACHMENTS,
            queue: QUEUE_DEPTH,
        }
    }
}

/// The core's opening offer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloOffer {
    /// Always [`PROTOCOL`].
    pub protocol: String,
    /// Versions the core accepts.
    pub versions: Vec<u32>,
    /// The module the core started.
    pub module_id: ModuleId,
    /// The core's own package version; a bundled module must match it exactly.
    pub package_version: String,
    /// Hex digest of the executable the core verified before spawning.
    pub executable_digest: String,
    /// Core-minted instance number, echoed in every envelope.
    pub instance: u64,
    /// The role this instance plays.
    pub role: Role,
    /// Granted transport ceilings.
    pub limits: Limits,
    /// Capabilities the core needs declared (supported or not).
    pub requested_caps: Vec<Capability>,
    /// The language's admitted configuration for this instance. Never a grant or a credential.
    pub config: ModuleConfig,
}

/// What a module instance learns about its configuration in `hello`; its process environment is
/// otherwise cleared (only the same `env` entries are set).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleConfig {
    /// The language's accepted provider declaration (executable, toolchain, trust, cache
    /// namespace and its raw option fields), when one is configured.
    pub provider: Option<Value>,
    /// The language's `project_checks` launcher section, when configured.
    pub checks: Option<Value>,
    /// The descriptor's module environment names with the daemon's values.
    pub env: std::collections::BTreeMap<String, String>,
    /// The real user home the daemon resolved.
    pub home: Option<std::path::PathBuf>,
}

/// The module's answer to [`HelloOffer`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    /// Always [`PROTOCOL`].
    pub protocol: String,
    /// The selected version.
    pub version: u32,
    /// The module's own id.
    pub module_id: ModuleId,
    /// The module's own package version.
    pub package_version: String,
    /// The digest the core offered, echoed.
    pub executable_digest: String,
    /// The instance the core offered, echoed.
    pub instance: u64,
    /// Every capability the module declares.
    pub capabilities: Vec<CapabilityDecl>,
    /// Linkage namespaces the module emits with their roles; empty when linkage is unsupported.
    pub linkage_kinds: Vec<super::payload::LinkageCoverage>,
}

/// What a module declares about itself, independent of any one offer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    /// The module id.
    pub module_id: ModuleId,
    /// The module's package version.
    pub package_version: String,
    /// Declared capabilities.
    pub capabilities: Vec<CapabilityDecl>,
    /// Emitted linkage namespaces with their roles.
    pub linkage_kinds: Vec<super::payload::LinkageCoverage>,
}

impl Declaration {
    /// The declared support of `capability`, if declared.
    pub fn support(&self, capability: Capability) -> Option<Support> {
        self.capabilities
            .iter()
            .find(|decl| decl.capability == capability)
            .map(|decl| decl.support)
    }

    /// The module's reply to `offer`, or the incompatibility that refuses it: another protocol,
    /// no common version, another module or package version, or a requested capability left
    /// undeclared.
    ///
    /// The executable digest is the core's check: the host runtime pins the sealed executable
    /// and re-hashes it immediately before spawning, so the module only echoes the digest as part
    /// of the instance identity and never re-hashes its own image.
    pub fn answer(&self, offer: &HelloOffer) -> Result<HelloReply, String> {
        if offer.protocol != PROTOCOL || !offer.versions.contains(&VERSION) {
            return Err("no common protocol version".to_owned());
        }
        if offer.module_id != self.module_id {
            return Err(format!(
                "this is {}, not {}",
                self.module_id, offer.module_id
            ));
        }
        if offer.package_version != self.package_version {
            return Err(format!(
                "package {} cannot serve core {}",
                self.package_version, offer.package_version
            ));
        }
        if let Some(missing) = offer
            .requested_caps
            .iter()
            .find(|capability| self.support(**capability).is_none())
        {
            return Err(format!("capability {missing:?} undeclared"));
        }
        Ok(HelloReply {
            protocol: PROTOCOL.to_owned(),
            version: VERSION,
            module_id: self.module_id.clone(),
            package_version: self.package_version.clone(),
            executable_digest: offer.executable_digest.clone(),
            instance: offer.instance,
            capabilities: self.capabilities.clone(),
            linkage_kinds: self.linkage_kinds.clone(),
        })
    }
}

impl HelloOffer {
    /// Checks the module's reply against this offer: same protocol and version, module, package
    /// version, digest and instance, and every requested capability declared.
    pub fn accept(&self, reply: &HelloReply) -> Result<(), String> {
        let same = reply.protocol == PROTOCOL
            && reply.version == VERSION
            && self.versions.contains(&reply.version)
            && reply.module_id == self.module_id
            && reply.package_version == self.package_version
            && reply.executable_digest == self.executable_digest
            && reply.instance == self.instance;
        if !same {
            return Err("hello identity mismatch".to_owned());
        }
        match self.requested_caps.iter().find(|capability| {
            !reply
                .capabilities
                .iter()
                .any(|decl| decl.capability == **capability && decl.version == 0)
        }) {
            Some(missing) => Err(format!("capability {missing:?} undeclared")),
            None => Ok(()),
        }
    }
}

/// The fence every envelope carries; a reply must echo its request's fence exactly.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fence {
    /// Instance from `hello`.
    pub instance: u64,
    /// Monotonic per instance, never reused.
    pub request_id: u64,
    /// Opaque core-minted scope key (worktree, environment, settings view).
    pub scope_key: String,
    /// Opaque core-minted revision key (source and configuration revisions).
    pub revision_key: String,
}

/// One request, core to module.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The fence the reply must echo.
    pub fence: Fence,
    /// The capability addressed.
    pub capability: Capability,
    /// Its version.
    pub capability_version: u32,
    /// Remaining allowance in milliseconds; the core's own deadline is authoritative.
    pub budget_ms: u64,
    /// The capability's typed payload ([`super::payload`]); `null` when it was spilled.
    pub payload: Value,
    /// When set, the payload is this `application/json` attachment (encoded above
    /// [`MAX_INLINE_BODY`]); the receiver decodes it before the capability sees it.
    pub body_attachment: Option<u32>,
    /// Attachments that follow this frame.
    pub attachments: Vec<AttachmentDecl>,
}

/// Largest encoded payload or result kept inline in a control frame; larger ones are sent as an
/// `application/json` attachment and named by `body_attachment`.
pub const MAX_INLINE_BODY: usize = 512 * 1024;

/// Moves `value` into a new `application/json` attachment when its encoding exceeds
/// [`MAX_INLINE_BODY`]; returns what the envelope carries inline and the attachment id.
pub(crate) fn spill(
    value: Value,
    attachments: &mut Vec<super::wire::Attachment>,
) -> (Value, Option<u32>) {
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    if bytes.len() <= MAX_INLINE_BODY {
        return (value, None);
    }
    let id = attachments.iter().map(|a| a.id).max().unwrap_or(0) + 1;
    attachments.push(super::wire::Attachment {
        id,
        content_type: "application/json".to_owned(),
        bytes,
    });
    (Value::Null, Some(id))
}

/// Reverses [`spill`]: takes the named attachment out of `attachments` and decodes it; an
/// absent or undecodable body, or an inline value beside it, is `Err`.
pub(crate) fn unspill(
    inline: Value,
    body: Option<u32>,
    attachments: &mut Vec<super::wire::Attachment>,
) -> Result<Value, ()> {
    let Some(id) = body else {
        return Ok(inline);
    };
    let position = attachments.iter().position(|a| a.id == id).ok_or(())?;
    let attachment = attachments.remove(position);
    if !inline.is_null() || attachment.content_type != "application/json" {
        return Err(());
    }
    serde_json::from_slice(&attachment.bytes).map_err(|_| ())
}

/// Readiness of the capability that answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// The provider is still starting or indexing.
    Warming,
    /// The provider reported readiness.
    Ready,
    /// The provider answers but reported a problem.
    Degraded,
    /// No provider answers for this scope.
    Unavailable,
}

/// How complete a result is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// Everything in scope was considered.
    Complete,
    /// Some of the scope was not considered (indexing, a cap, a failed root).
    Partial,
    /// The module cannot tell.
    Unknown,
}

/// A module's typed refusal of one request; the instance stays usable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleError {
    /// The closed refusal kind.
    pub code: ErrorCode,
    /// Sanitized detail for the core; never forwarded raw to an agent.
    pub message: String,
}

/// Closed refusal kinds of [`ModuleError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The module cannot take another request now.
    Busy,
    /// The capability or this input is not supported, as in process today.
    Unsupported,
    /// The provider is not ready to answer yet.
    Warming,
    /// The payload does not decode or violates its contract.
    InvalidRequest,
    /// A configured tool is missing.
    ToolMissing,
    /// The language computation failed for this input.
    Failed,
}

/// Exactly one of a result or a typed error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The capability's typed result.
    Result(Value),
    /// A typed refusal.
    Error(ModuleError),
}

/// One response, module to core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// The request's fence, echoed exactly.
    pub fence: Fence,
    /// Result or error; a spilled result is `null` here.
    pub outcome: Outcome,
    /// When set, the result is this `application/json` attachment (see [`MAX_INLINE_BODY`]).
    pub body_attachment: Option<u32>,
    /// Readiness of the answering capability.
    pub readiness: Readiness,
    /// Completeness of the result.
    pub coverage: Coverage,
    /// Attachments that follow this frame.
    pub attachments: Vec<AttachmentDecl>,
}

/// A module's request that the core run one effect recipe while `fence`'s request is active.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectCall {
    /// The active request's fence.
    pub fence: Fence,
    /// Module-local call number within the request; a repeat returns the earlier outcome.
    pub call: u32,
    /// The recipe and its typed parameters; `null` when spilled.
    pub effect: Value,
    /// When set, the effect is this `application/json` attachment (see [`MAX_INLINE_BODY`]):
    /// parameter lists such as a deep worktree's ancestor files are never cut to fit a frame.
    pub body_attachment: Option<u32>,
    /// Attachments that follow this frame.
    pub attachments: Vec<AttachmentDecl>,
}

/// The core's answer to an [`EffectCall`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectReply {
    /// The active request's fence.
    pub fence: Fence,
    /// The call answered.
    pub call: u32,
    /// The run's outcome or the core's refusal.
    pub outcome: super::payload::EffectOutcome,
    /// Stdout (id 1) and stderr (id 2) bytes, following this frame.
    pub attachments: Vec<AttachmentDecl>,
}

/// Every control object, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// Core opens the instance.
    Hello(HelloOffer),
    /// Module accepts it.
    HelloReply(HelloReply),
    /// Module refuses it (incompatible identity, version, role or configuration) and exits.
    HelloRefused {
        /// Sanitized reason.
        reason: String,
    },
    /// Core asks.
    Request(Request),
    /// Module answers.
    Response(Response),
    /// Module asks the core to run an effect for the active request.
    Effect(EffectCall),
    /// Core answers an effect.
    EffectReply(EffectReply),
    /// Core abandons the active request; the module stops working on it and sends nothing more
    /// for it. The core treats the instance as poisoned unless the module answers nothing.
    Cancel(Fence),
    /// Core asks the module to exit cleanly.
    Shutdown {
        /// The instance.
        instance: u64,
    },
}

/// Stage at which a module became unavailable (closed set).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Waiting for Execution admission.
    Admission,
    /// Starting the process.
    Spawn,
    /// The `hello` exchange.
    Hello,
    /// The module's provider.
    Provider,
    /// An ordinary request.
    Request,
    /// A core-run effect.
    Effect,
    /// Decoding a reply.
    Decode,
    /// Stopping and reaping.
    Drain,
}

/// Cause of a module's unavailability (closed set).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The process or its stream ended.
    Exited,
    /// A budget expired.
    Timeout,
    /// A frame or payload did not decode.
    Malformed,
    /// A frame or attachment exceeded its ceiling.
    Oversized,
    /// A reply named another instance, request, scope or revision.
    WrongFence,
    /// `hello` found no compatible identity, version or capability set.
    Incompatible,
    /// A configured tool is missing.
    ToolMissing,
    /// Core policy refused the module's proposal.
    PolicyRefused,
    /// The restart budget is spent until its window expires.
    RestartExhausted,
    /// A bound on count, bytes or queue was reached.
    ResourceLimit,
    /// Cleanup could not prove the process group gone.
    ReapUnverified,
}

/// Name of a serde-serialized unit variant.
fn variant_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl fmt::Display for Stage {
    /// Writes the snake-case name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&variant_name(self))
    }
}

impl fmt::Display for Cause {
    /// Writes the snake-case name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&variant_name(self))
    }
}

/// The typed failure every dependent tool path reports when a module cannot answer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleUnavailable {
    /// The module.
    pub module_id: ModuleId,
    /// Its package version.
    pub module_version: String,
    /// The instance's role.
    pub role: Role,
    /// Where it failed.
    pub stage: Stage,
    /// Why.
    pub cause: Cause,
    /// The failed instance, when one existed.
    pub instance: Option<u64>,
    /// When a new attempt is permitted, for a restart budget or backoff.
    pub retry_after_ms: Option<u64>,
}

impl fmt::Display for ModuleUnavailable {
    /// Writes `module_unavailable (<module>:<stage>:<cause>)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "module_unavailable ({}:{}:{})",
            self.module_id, self.stage, self.cause
        )
    }
}

impl std::error::Error for ModuleUnavailable {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An offer for `bundled.alpha` 1.0 requesting `caps`.
    pub(crate) fn offer(caps: Vec<Capability>) -> HelloOffer {
        HelloOffer {
            protocol: PROTOCOL.to_owned(),
            versions: vec![0],
            module_id: ModuleId::bundled("alpha"),
            package_version: "1.0".to_owned(),
            executable_digest: "ab".repeat(32),
            instance: 3,
            role: Role::Analyzer,
            limits: Limits::default(),
            requested_caps: caps,
            config: ModuleConfig::default(),
        }
    }

    /// Module ids validate; capabilities, stages and causes use their wire names.
    #[test]
    fn names_are_stable() {
        assert_eq!(ModuleId::bundled("alpha").language(), "alpha");
        for bad in ["alpha", "bundled.", "bundled.Alpha", "bundled.a b"] {
            assert!(
                serde_json::from_value::<ModuleId>(json!(bad)).is_err(),
                "{bad}"
            );
        }
        assert_eq!(json!(Capability::Rename), json!("edit_plan.rename"));
        assert_eq!(json!(Capability::AnalyzeSource), json!("analyze_source"));
        let failure = ModuleUnavailable {
            module_id: ModuleId::bundled("alpha"),
            module_version: "1.0".into(),
            role: Role::Checker,
            stage: Stage::Request,
            cause: Cause::WrongFence,
            instance: Some(2),
            retry_after_ms: None,
        };
        assert_eq!(
            failure.to_string(),
            "module_unavailable (bundled.alpha:request:wrong_fence)"
        );
        assert_eq!(Role::parse("checker"), Some(Role::Checker));
        assert_eq!(Role::parse("other"), None);
    }

    /// Every control variant round-trips; unknown fields and unknown types are refused.
    #[test]
    fn control_round_trips_and_refuses_unknowns() {
        let fence = Fence {
            instance: 3,
            request_id: 4,
            scope_key: "s".into(),
            revision_key: "r".into(),
        };
        let controls = [
            Control::Hello(offer(vec![Capability::Outline])),
            Control::Request(Request {
                fence: fence.clone(),
                capability: Capability::Outline,
                capability_version: 0,
                budget_ms: 10,
                payload: json!({"path": "a"}),
                body_attachment: None,
                attachments: vec![],
            }),
            Control::Response(Response {
                fence: fence.clone(),
                outcome: Outcome::Error(ModuleError {
                    code: ErrorCode::Warming,
                    message: "indexing".into(),
                }),
                body_attachment: Some(2),
                readiness: Readiness::Warming,
                coverage: Coverage::Unknown,
                attachments: vec![],
            }),
            Control::Cancel(fence.clone()),
            Control::Shutdown { instance: 3 },
        ];
        for control in controls {
            let value = serde_json::to_value(&control).unwrap();
            assert_eq!(serde_json::from_value::<Control>(value).unwrap(), control);
        }
        let mut extra = serde_json::to_value(Control::Shutdown { instance: 1 }).unwrap();
        extra["surprise"] = json!(true);
        assert!(serde_json::from_value::<Control>(extra).is_err());
        assert!(serde_json::from_value::<Control>(json!({"type": "upgrade"})).is_err());
        let mut request = serde_json::to_value(Control::Cancel(fence)).unwrap();
        request["grant"] = json!("x");
        assert!(serde_json::from_value::<Control>(request).is_err());
    }

    /// `hello` accepts only the exact identity and a declaration of every requested capability.
    #[test]
    fn hello_negotiation_is_exact() {
        let declaration = Declaration {
            module_id: ModuleId::bundled("alpha"),
            package_version: "1.0".into(),
            capabilities: vec![CapabilityDecl::v0(Capability::Outline, Support::Supported)],
            linkage_kinds: vec![],
        };
        let offer = offer(vec![Capability::Outline]);
        let reply = declaration.answer(&offer).unwrap();
        assert_eq!(offer.accept(&reply), Ok(()));
        let mut wrong = reply.clone();
        wrong.instance = 9;
        assert!(offer.accept(&wrong).is_err());
        let mut digest = reply.clone();
        digest.executable_digest = "00".into();
        assert!(offer.accept(&digest).is_err());
        let mut older = offer.clone();
        older.package_version = "0.9".into();
        assert!(declaration.answer(&older).is_err());
        let mut future = offer.clone();
        future.versions = vec![1];
        assert!(declaration.answer(&future).is_err());
        let needs_calls = super::tests::offer(vec![Capability::Calls]);
        assert!(declaration.answer(&needs_calls).is_err());
        assert!(needs_calls.accept(&reply).is_err());
    }
}
