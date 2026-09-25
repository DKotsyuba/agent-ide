//! Execution-private v2 shape proofs and opt-in v3 visualization-leaf admission.
//!
//! This module replaces the opaque v1 digest *comparison* for admitted managed Codex sandbox
//! states with a closed, versioned semantic shape. It never rewrites replay JSON: every function
//! here only reads the already-validated [`HostSandboxState`]. V3 additionally checks its one
//! existing visualization directory for symlink redirection. The design is deliberately
//! incomplete subtyping: `prove_narrower` implements exactly seven sufficient conditions and
//! refuses everything else, so an unproven change is never admitted by a looser fallback.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::{HostSandboxState, ProfileClass, canonical_json};

/// Domain separator prefixing every profile-shape v2 digest input (T35B).
const SHAPE_DIGEST_DOMAIN: &[u8] = b"agent-ide/profile-shape/v2\0";
/// Separate identity for the opt-in visualization-leaf family.
const VISUALIZATION_DIGEST_DOMAIN: &[u8] = b"agent-ide/profile-shape/v3\0";

/// Reports why one managed sandbox state has no derivable v2 or v3 shape.
///
/// The reason is one closed static description for operator diagnostics, never host-supplied
/// text. An unsupported v2 shape can only use a v1 exact digest; an unsupported v3 shape cannot
/// use the v3 family. Either way, the live JSON keeps replaying byte-for-byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnsupportedShape(pub(crate) &'static str);

/// One recognized network policy value of a managed restricted profile (T35B).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NetworkMode {
    /// Loopback-only or otherwise restricted network as declared by the host.
    Restricted,
    /// Unrestricted network as declared by the host.
    Enabled,
}

impl NetworkMode {
    /// Returns the closed JSON spelling used by the canonical serialization.
    fn tag(self) -> &'static str {
        match self {
            NetworkMode::Restricted => "restricted",
            NetworkMode::Enabled => "enabled",
        }
    }

    /// Parses the closed host spelling; any other value is unsupported, never defaulted.
    fn parse(value: &Value) -> Result<Self, UnsupportedShape> {
        match value.as_str() {
            Some("restricted") => Ok(NetworkMode::Restricted),
            Some("enabled") => Ok(NetworkMode::Enabled),
            _ => Err(UnsupportedShape("network")),
        }
    }
}

/// One recognized non-deny access value at a selector (T35B).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    /// The entry grants read access.
    Read,
    /// The entry grants write access (which the host may also read through).
    Write,
}

impl Access {
    /// Returns the closed JSON spelling used by the canonical serialization.
    fn tag(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
        }
    }
}

/// The preserved absent-versus-`skip` missing-path behavior of one entry (T35B).
///
/// A skip-read rule is not automatically noise: removing one can widen access, so the two values
/// are distinct rule identities and any other host value refuses the whole derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissingPath {
    /// The entry declared no `missing_path_behavior`.
    Absent,
    /// The entry declared `missing_path_behavior: "skip"`.
    Skip,
}

/// One recognized selector naming the object a filesystem entry grants or denies (T35B).
///
/// Selectors are tagged and component-wise; a literal placeholder string is never used, so no
/// path can collide with a marker. Paths retain their case and Unicode bytes: no home expansion,
/// Unicode normalization, case folding, or `Path` re-normalization is applied after the strict
/// raw-component grammar has accepted them.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Selector {
    /// The state's own validated sandbox cwd (empty component list) or a component descendant
    /// of it. This is the portable form the v1 `<workspace-cwd>` placeholder approximated;
    /// derivation binds it to the trusted candidate/Workspace worktree supplied to
    /// [`ProfileShapeV2::derive`], refusing any state whose `sandboxCwd` is a different
    /// directory or whose cwd subtree overlaps an absolute restriction unprovably (T35B-r).
    WorkspaceRelative(Vec<String>),
    /// An absolute local path outside the trusted cwd, one component per element; an empty
    /// component list is the filesystem root itself.
    Absolute(Vec<String>),
    /// The `root` special path; deliberately distinct from [`Selector::Absolute`].
    Root,
    /// The `slash_tmp` special path; deliberately distinct from [`Selector::Tmpdir`].
    SlashTmp,
    /// The `tmpdir` special path; deliberately distinct from [`Selector::SlashTmp`].
    Tmpdir,
}

impl Selector {
    /// Returns the tagged canonical JSON value for this selector.
    fn to_canonical(&self) -> Value {
        match self {
            Selector::WorkspaceRelative(components) => serde_json::json!({
                "type": "workspace_relative",
                "components": components,
            }),
            Selector::Absolute(components) => serde_json::json!({
                "type": "absolute",
                "components": components,
            }),
            Selector::Root => serde_json::json!({"type": "special", "kind": "root"}),
            Selector::SlashTmp => serde_json::json!({"type": "special", "kind": "slash_tmp"}),
            Selector::Tmpdir => serde_json::json!({"type": "special", "kind": "tmpdir"}),
        }
    }
}

/// One recognized deny rule: a required exclusion the accepted capture proved (T35B).
///
/// Only path denies and exact `glob_pattern` denies are recognized, and they stay distinct rule
/// identities: a wildcard-free glob is never collapsed into a path deny, because subtree
/// containment and exact glob matching are different semantics and a backslash has literal path
/// meaning but glob-escape meaning. Any other deny selector type refuses the whole derivation,
/// because an unknown deny vocabulary can hide a permission this build cannot reason about.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DenyRule {
    /// A `{"type":"path"}` deny naming one recognized selector.
    Path(Selector),
    /// A `{"type":"glob_pattern"}` deny. `base` is the cwd-normalized literal selector the
    /// pattern starts from and `pattern` is the host pattern's remainder starting at that base
    /// boundary, byte-for-byte (empty for a wildcard-free glob). The canonical form always
    /// carries the glob tag, the base, and this exact pattern text, so a path deny and a
    /// same-text glob deny are never one identity, a backslash is never reinterpreted, and a
    /// weakened pattern is simply a different required rule. No glob containment is ever
    /// computed; for an absolute base, base and pattern concatenate back to the host pattern
    /// byte-for-byte, and a workspace-relative base is the cwd-relative spelling the portable
    /// shape exists to keep.
    Glob { base: Selector, pattern: String },
}

impl DenyRule {
    /// Returns the tagged canonical JSON value for this deny rule.
    fn to_canonical(&self) -> Value {
        match self {
            DenyRule::Path(selector) => serde_json::json!({
                "type": "path",
                "selector": selector.to_canonical(),
            }),
            DenyRule::Glob { base, pattern } => serde_json::json!({
                "type": "glob_pattern",
                "base": base.to_canonical(),
                "pattern": pattern,
            }),
        }
    }
}

/// The conservative semantic essence of one managed restricted Codex sandbox state (T35B).
///
/// Every field is either essential authority (mechanism, network, selectors, denies) or a
/// preserved expansion setting that can change which restrictions take effect. The shape is
/// derived only by the closed parser [`ProfileShapeV2::derive`]; unknown top-level, filesystem,
/// entry, or selector fields produce [`UnsupportedShape`] and never a partial shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileShapeV2 {
    /// Exact `useLegacyLandlock` boolean: part of the replayed sandbox mechanism. A non-null
    /// `codexLinuxSandboxExe` refuses derivation entirely (no executable identity pin exists in
    /// this build), so no helper field is part of the shape.
    pub(crate) use_legacy_landlock: bool,
    /// Declared network mode; only `restricted` and `enabled` are recognized.
    pub(crate) network: NetworkMode,
    /// Preserved `glob_scan_max_depth`; changing expansion depth can change which restrictions
    /// take effect, so absent-versus-present is significant.
    pub(crate) glob_scan_max_depth: Option<u64>,
    /// Non-deny rules keyed by their selector. A selector with two conflicting accesses never
    /// derives, so each selector holds exactly one access and one missing-path behavior.
    pub(crate) rules: BTreeMap<Selector, (Access, MissingPath)>,
    /// Deduplicated required deny rules.
    pub(crate) denies: BTreeSet<DenyRule>,
}

impl ProfileShapeV2 {
    /// Derives the closed shape from one already-validated managed sandbox state (T35B).
    ///
    /// `trusted_cwd` is the trusted candidate or Workspace worktree the operation will actually
    /// touch; the portable cwd-derived authority is bound to it, so a state whose `sandboxCwd`
    /// is not component-wise this directory refuses derivation instead of relocating its grant
    /// (T35B-r). Every key set is closed: unknown top-level fields, unknown permission-profile or
    /// filesystem fields, unknown entry or selector fields, an unknown access, network, special
    /// kind, or missing-path value, a cwd of `/`, a non-null `codexLinuxSandboxExe` helper (no
    /// executable identity pin exists yet), any path outside the strict local grammar, and any
    /// absolute deny/restriction whose overlap with the cwd subtree changes precedence all
    /// return [`UnsupportedShape`] for the whole state. Identical duplicate rules deduplicate;
    /// the same selector with conflicting accesses refuses instead of guessing from array order
    /// or selecting the strongest access.
    pub(crate) fn derive(
        state: &HostSandboxState,
        trusted_cwd: &Path,
    ) -> Result<Self, UnsupportedShape> {
        if state.class != ProfileClass::Managed {
            // v2 initially supports managed restricted profiles only; `disabled` stays a
            // separate legacy v1 exact-digest concern.
            return Err(UnsupportedShape("profile class"));
        }
        let object = state.raw.as_object().ok_or(UnsupportedShape("envelope"))?;
        // The envelope itself is closed: an unknown top-level field may carry a permission this
        // build cannot interpret, so it refuses derivation instead of a partial shape.
        if object.len() != 4
            || !object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "permissionProfile"
                        | "codexLinuxSandboxExe"
                        | "sandboxCwd"
                        | "useLegacyLandlock"
                )
            })
        {
            return Err(UnsupportedShape("top-level fields"));
        }
        let use_legacy_landlock = object
            .get("useLegacyLandlock")
            .and_then(Value::as_bool)
            .ok_or(UnsupportedShape("useLegacyLandlock"))?;
        // A non-null helper refuses the whole derivation: the accepted string is evidence of a
        // path, not a pin on the executable's identity, so a replaced helper could change the
        // replayed mechanism without changing this shape. Until an executable identity is
        // pinned and revalidated, only the exact null helper derives.
        match object.get("codexLinuxSandboxExe") {
            Some(Value::Null) => {}
            _ => return Err(UnsupportedShape("codexLinuxSandboxExe helper")),
        }
        let profile = object
            .get("permissionProfile")
            .and_then(Value::as_object)
            .ok_or(UnsupportedShape("permissionProfile"))?;
        if profile.len() != 3
            || !profile
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "file_system" | "network"))
            || profile.get("type").and_then(Value::as_str) != Some("managed")
        {
            return Err(UnsupportedShape("permissionProfile fields"));
        }
        let network =
            NetworkMode::parse(profile.get("network").ok_or(UnsupportedShape("network"))?)?;
        let file_system = profile
            .get("file_system")
            .and_then(Value::as_object)
            .ok_or(UnsupportedShape("file_system"))?;
        if !file_system
            .keys()
            .all(|key| matches!(key.as_str(), "type" | "entries" | "glob_scan_max_depth"))
            || file_system.get("type").and_then(Value::as_str) != Some("restricted")
        {
            return Err(UnsupportedShape("file_system fields"));
        }
        let glob_scan_max_depth = match file_system.get("glob_scan_max_depth") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or(UnsupportedShape("glob_scan_max_depth"))?,
            ),
        };
        let entries = file_system
            .get("entries")
            .and_then(Value::as_array)
            .ok_or(UnsupportedShape("entries"))?;
        // The trusted cwd must be a real directory prefix, never `/`: a root cwd would collapse
        // every absolute path into the portable workspace-relative form and widen the template.
        let cwd_components = cwd_components(state, trusted_cwd)?;
        let mut rules = BTreeMap::new();
        let mut denies = BTreeSet::new();
        for entry in entries {
            derive_entry(entry, &cwd_components, &mut rules, &mut denies)?;
        }
        refuse_cwd_overlap(&rules, &denies, &cwd_components)?;
        Ok(Self {
            use_legacy_landlock,
            network,
            glob_scan_max_depth,
            rules,
            denies,
        })
    }

    /// Returns the domain-separated BLAKE3 shape digest over the canonical serialization.
    ///
    /// `BLAKE3("agent-ide/profile-shape/v2\0" || canonical)`; the domain separation keeps v2
    /// digests incomparable with v1 template digests and with raw-state digests.
    pub(crate) fn digest(&self) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(SHAPE_DIGEST_DOMAIN);
        hasher.update(self.canonical().as_bytes());
        hasher.finalize()
    }

    /// Serializes the closed canonical shape: sorted keys, rule arrays sorted by canonical
    /// encoded bytes and deduplicated, absent-versus-explicit values preserved.
    fn canonical(&self) -> String {
        let mut shape = serde_json::json!({
            "schema": "agent-ide/profile-shape/v2",
            "mechanism": {
                "file_system": "restricted",
                "use_legacy_landlock": self.use_legacy_landlock,
                "codex_linux_sandbox_exe": null,
            },
            "network": self.network.tag(),
            "rules": sorted_canonical_array(
                self.rules.iter().map(|(selector, (access, missing))| {
                    let mut rule = serde_json::json!({
                        "access": access.tag(),
                        "selector": selector.to_canonical(),
                    });
                    if let Some(behavior) = missing_behavior_tag(*missing) {
                        rule["missing_path_behavior"] = Value::String(behavior.to_owned());
                    }
                    rule
                }),
            ),
            "denies": sorted_canonical_array(self.denies.iter().map(DenyRule::to_canonical)),
        });
        if let Some(depth) = self.glob_scan_max_depth {
            shape["glob_scan_max_depth"] = Value::from(depth);
        }
        canonical_json(&shape)
    }

    /// Proves `self` (the live shape) is a sufficient subset of `accepted` (T35B).
    ///
    /// These are exactly the seven sufficient conditions of the design; anything not covered
    /// returns `false`, including safe-looking changes this deliberately incomplete proof does
    /// not model (removing a write selector, removing a nested read restriction, or any glob
    /// containment reasoning). A digest establishes equality; it can never establish subset
    /// containment, so narrowing is always proven structurally.
    pub(crate) fn prove_narrower(&self, accepted: &Self) -> bool {
        // 1. Same supported mechanism, selector vocabulary, and filesystem type. Both shapes
        //    come from the same closed parser, so the vocabulary and the `restricted` filesystem
        //    type are invariants, a non-null helper never derived at all, and the replayed
        //    mechanism must match exactly.
        if self.use_legacy_landlock != accepted.use_legacy_landlock {
            return false;
        }
        // 2. Same normalized set of non-deny selectors: an added positive selector can reopen a
        //    denied subtree, and a removed one can widen or narrow unpredictably, so any change
        //    refuses.
        if self.rules.keys().ne(accepted.rules.keys()) {
            return false;
        }
        // 3. At each selector the live access is equal or reduced from write to read, and the
        //    missing-path behavior is unchanged.
        for (selector, (live_access, live_missing)) in &self.rules {
            let (accepted_access, accepted_missing) = accepted
                .rules
                .get(selector)
                .expect("selector sets proved equal");
            if live_missing != accepted_missing
                || !(*live_access == *accepted_access
                    || (*live_access == Access::Read && *accepted_access == Access::Write))
            {
                return false;
            }
        }
        // 4. Every normalized accepted deny rule occurs in live: forgetting an accepted
        //    restriction is not monotonic and never ignored.
        if !accepted.denies.is_subset(&self.denies) {
            return false;
        }
        // 5. Additional live deny rules have recognized, verified narrowing semantics. The
        //    closed parser already refuses whole states carrying any deny type other than path
        //    or exact glob-pattern denies, so every rule in `self.denies` is by construction a
        //    recognized type; nothing further can be checked without a precedence-aware proof.
        // 6. Network is equal or reduced from enabled to restricted; the reverse direction
        //    refuses.
        if match (self.network, accepted.network) {
            (live, accepted) if live == accepted => false,
            (NetworkMode::Restricted, NetworkMode::Enabled) => false,
            _ => true,
        } {
            return false;
        }
        // 7. Glob expansion settings match exactly.
        self.glob_scan_max_depth == accepted.glob_scan_max_depth
    }
}

/// A v2 shape with exactly one reviewed visualization write leaf and its four denies removed.
///
/// The accepted record binds `namespace`; each live state must independently prove its current
/// directory, strict date/task path, and complete deny group before v2 narrowing compares the
/// remaining authority. The leaf itself is never added to daemon read proofs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileShapeV3 {
    /// All authority except the one checked visualization leaf and its four matching denies.
    base: ProfileShapeV2,
    /// Strict absolute namespace components accepted by the operator's v3 record.
    namespace: Vec<String>,
}

impl ProfileShapeV3 {
    /// Derives the narrow family from a live state bound to `trusted_cwd`.
    ///
    /// Exactly one writable `<accepted>/.codex/visualizations/YYYY/MM/DD/<uuid>` path
    /// must have no existing symlink component and carry all four credential glob denies.
    /// Filesystem failure, malformed or additional visualization grants, and extra denies inside
    /// the leaf refuse; this read-only check runs at permit and immediately before spawn.
    pub(crate) fn derive(
        state: &HostSandboxState,
        trusted_cwd: &Path,
    ) -> Result<Self, UnsupportedShape> {
        Self::derive_inner(state, trusted_cwd, true)
    }

    /// Reconstructs a stored accepted capture without consulting its former visualization path.
    ///
    /// The complete captured-state digest pins this old JSON; the remaining v2 cwd binding is
    /// unchanged. Only a live permit or spawn checks the current namespace and leaf filesystem.
    pub(crate) fn derive_stored(
        state: &HostSandboxState,
        trusted_cwd: &Path,
    ) -> Result<Self, UnsupportedShape> {
        Self::derive_inner(state, trusted_cwd, false)
    }

    /// Parses common v3 structure with `check_live_path` selecting live filesystem validation.
    ///
    /// Stored captures skip only visualization filesystem checks; strict v2 cwd binding and all
    /// structural grant/deny rejection conditions remain. Live derivation also rejects missing or
    /// redirected namespace paths, symlinked existing leaf components, and non-directory entries.
    fn derive_inner(
        state: &HostSandboxState,
        trusted_cwd: &Path,
        check_live_path: bool,
    ) -> Result<Self, UnsupportedShape> {
        let mut base = ProfileShapeV2::derive(state, trusted_cwd)?;
        let leaves: Vec<_> = base
            .rules
            .iter()
            .filter_map(|(selector, rule)| {
                let Selector::Absolute(parts) = selector else {
                    return None;
                };
                parts
                    .windows(2)
                    .any(|pair| pair == [".codex", "visualizations"])
                    .then_some((parts.clone(), *rule))
            })
            .collect();
        if leaves.len() != 1 {
            return Err(UnsupportedShape("visualization leaf count"));
        }
        let (leaf, rule) = &leaves[0];
        if leaf.len() < 7
            || leaf[leaf.len() - 6] != ".codex"
            || leaf[leaf.len() - 5] != "visualizations"
            || *rule != (Access::Write, MissingPath::Absent)
            || !valid_visualization_date(&leaf[leaf.len() - 4..leaf.len() - 1])
            || !valid_task_id(&leaf[leaf.len() - 1])
        {
            return Err(UnsupportedShape("visualization leaf"));
        }
        let namespace = leaf[..leaf.len() - 4].to_vec();
        let leaf_path = Path::new("/").join(leaf.join("/"));
        let namespace_path = Path::new("/").join(namespace.join("/"));
        if check_live_path {
            if std::fs::canonicalize(&namespace_path).ok().as_deref()
                != Some(namespace_path.as_path())
            {
                return Err(UnsupportedShape("visualization namespace"));
            }
            let mut component = leaf_path.as_path();
            while component != namespace_path {
                match std::fs::symlink_metadata(component) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(UnsupportedShape("visualization symlink"));
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(UnsupportedShape("visualization directory"));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(UnsupportedShape("visualization path")),
                }
                component = component
                    .parent()
                    .ok_or(UnsupportedShape("visualization path"))?;
            }
        }
        let expected: BTreeSet<_> = ["/**/*.key", "/**/*.pem", "/**/.env", "/**/.env.*"]
            .into_iter()
            .map(|pattern| DenyRule::Glob {
                base: Selector::Absolute(leaf.clone()),
                pattern: pattern.to_owned(),
            })
            .collect();
        if !expected.is_subset(&base.denies)
            || base.denies.iter().any(|deny| match deny {
                DenyRule::Path(Selector::Absolute(parts)) => parts.starts_with(leaf),
                DenyRule::Glob {
                    base: Selector::Absolute(parts),
                    ..
                } => parts.starts_with(leaf) && !expected.contains(deny),
                _ => false,
            })
        {
            return Err(UnsupportedShape("visualization denials"));
        }
        base.rules.remove(&Selector::Absolute(leaf.clone()));
        base.denies.retain(|deny| !expected.contains(deny));
        if base.rules.iter().any(|(selector, (access, _))| {
            *access == Access::Write
                && match selector {
                    Selector::Absolute(parts) => {
                        parts.starts_with(&namespace) || namespace.starts_with(parts)
                    }
                    Selector::Root => true,
                    Selector::SlashTmp => {
                        namespace_path.starts_with("/tmp")
                            || namespace_path.starts_with("/private/tmp")
                    }
                    Selector::Tmpdir => {
                        check_live_path
                            && std::fs::canonicalize(std::env::temp_dir())
                                .ok()
                                .is_some_and(|tmp| namespace_path.starts_with(tmp))
                    }
                    Selector::WorkspaceRelative(_) => false,
                }
        }) {
            return Err(UnsupportedShape("visualization ancestor or sibling write"));
        }
        Ok(Self { base, namespace })
    }

    /// Returns the v3 domain-separated identity of the accepted namespace and remaining shape.
    pub(crate) fn digest(&self) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(VISUALIZATION_DIGEST_DOMAIN);
        hasher.update(
            canonical_json(&serde_json::json!({
                "namespace": self.namespace,
                "base": self.base.canonical(),
            }))
            .as_bytes(),
        );
        hasher.finalize()
    }

    /// Applies unchanged v2 narrowing after the stored structure and live leaf checks succeed.
    pub(crate) fn prove_narrower(&self, accepted: &Self) -> bool {
        self.namespace == accepted.namespace && self.base.prove_narrower(&accepted.base)
    }
}

/// Checks the strict numeric date segments of a visualization leaf.
fn valid_visualization_date(parts: &[String]) -> bool {
    if parts.len() != 3
        || ![4, 2, 2]
            .into_iter()
            .zip(parts)
            .all(|(len, part)| part.len() == len && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        parts[0].parse::<u16>(),
        parts[1].parse::<u8>(),
        parts[2].parse::<u8>(),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year != 0 && (1..=max_day).contains(&day)
}

/// Checks the captured task-id spelling as a UUID; no thread identity is inferred from it.
fn valid_task_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// Outcome of the conservative per-path native-read proof (T36B).
///
/// `Proven` means permission coverage of exactly one path under the supported conservative
/// model of the live derived shape — never catalog admission, never a reusable filesystem
/// capability, and never a claim about the path's current filesystem state. Everything this
/// build cannot reason about is `Unproven`; there is no third, "probably fine" answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadProof {
    /// The live shape's own grants positively cover this path and no deny can reach it.
    Proven,
    /// The path grammar, the binding, the grants, or any deny is missing, ambiguous, or
    /// unsupported. The caller must refuse the native read.
    Unproven,
}

/// Proves one worktree-relative path may be read natively under one live derived shape (T36B).
///
/// This is the design's exact four-condition decision, evaluated against the shape bound to
/// `trusted_cwd` — the authoritative worktree the operation will actually touch. The shape
/// itself carries no cwd (it never enters serialization or digests), so the binding is a
/// per-call argument and a shape derived against a different directory simply refuses here
/// through [`trusted_path_components`] and the [`Selector::WorkspaceRelative`] resolution.
///
/// The four conditions:
///
/// 1. **Path and binding:** `relative_path` must be nonempty with normal components only —
///    absolute paths, `.`, `..`, empty components, NUL, and backslashes refuse before any
///    normalization — and is joined component-wise beneath `trusted_cwd`.
/// 2. **Positive coverage:** a `Root` read/write rule, or a `WorkspaceRelative` read/write
///    rule whose components prefix the target's, with an *absent* (not `skip`)
///    `missing_path_behavior`: a grant the host declared skipped when absent cannot establish
///    authority. Temporary selectors and outside grants never count.
/// 3. **Path denies:** any deny whose resolved selector is an ancestor of, or equal to, the
///    target makes the result `Unproven` — even where a narrower grant might reopen it; this
///    proof deliberately solves no precedence.
/// 4. **Glob denies:** a glob whose literal base cannot reach the target is irrelevant;
///    otherwise the pattern must provably not match the target suffix *and every intervening
///    ancestor suffix up to and including base equality*, under the conservative matcher
///    ([`glob_match_segments`]). A relevant glob under a present `glob_scan_max_depth` beyond
///    the configured expansion depth refuses; the limit is never used to declare a glob
///    irrelevant.
///
/// ASCII case folding is applied on deny matching only (patterns, path denies, and glob
/// bases alike), so a casing variant of a denied name can never read; grants are never folded.
/// Non-ASCII is never folded and never provably disjoint: a non-ASCII byte in the target or
/// in any relevant deny's path or glob base leaves the whole proof `Unproven`, because
/// filesystem normalization can identify such a spelling with an ASCII name and Unicode
/// wildcard semantics are out of scope (T36B-r).
pub(crate) fn read_proof(
    shape: &ProfileShapeV2,
    trusted_cwd: &Path,
    relative_path: &Path,
) -> ReadProof {
    // 1. Path and binding grammar: strict raw components only, joined beneath the cwd.
    //    The relative path is split on raw bytes, never `Path::components`, which silently
    //    normalizes away `.` and empty components the grammar must refuse before normalization.
    let cwd = match trusted_path_components(trusted_cwd) {
        Ok(cwd) => cwd,
        Err(_) => return ReadProof::Unproven,
    };
    let raw = match std::str::from_utf8(relative_path.as_os_str().as_encoded_bytes()) {
        Ok(raw) => raw,
        Err(_) => return ReadProof::Unproven,
    };
    if raw.is_empty() || raw.starts_with('/') || raw.contains(['\\', '\0']) {
        return ReadProof::Unproven;
    }
    let mut target = cwd.clone();
    for component in raw.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return ReadProof::Unproven;
        }
        target.push(component.to_owned());
    }
    let relative = &target[cwd.len()..];
    // 2. Positive coverage: Root, or a workspace-relative selector prefixing the target.
    //    `MissingPath::Skip` grants are not discarded — an absent skipped grant establishes
    //    no authority — and temporary selectors or outside grants never cover a cwd path.
    let covered = shape.rules.iter().any(|(selector, (_, missing))| {
        *missing == MissingPath::Absent
            && match selector {
                Selector::Root => true,
                Selector::WorkspaceRelative(components) => relative.starts_with(components),
                Selector::Absolute(_) | Selector::SlashTmp | Selector::Tmpdir => false,
            }
    });
    if !covered {
        return ReadProof::Unproven;
    }
    // Deny matching folds ASCII case on both sides — a casing variant of a denied name must
    // never read — and any non-ASCII byte on either side of a deny comparison refuses outright:
    // filesystem normalization can identify a non-ASCII spelling with an ASCII name (NFC folds
    // `K` U+212A onto `K`), so byte inequality never proves disjointness (T36B-r). Grants are
    // never folded: coverage is decided above on the exact components.
    let target_folded = fold_components(&target);
    for deny in &shape.denies {
        match deny {
            // 3. Any ancestor-or-equal path deny refuses, regardless of grant precedence.
            DenyRule::Path(selector) => {
                let Some(denied) = resolve_selector(selector, &cwd) else {
                    return ReadProof::Unproven;
                };
                match (&target_folded, fold_components(&denied)) {
                    (Some(target), Some(denied)) => {
                        if target.starts_with(denied.as_slice()) {
                            return ReadProof::Unproven;
                        }
                    }
                    // A non-ASCII target is ambiguous against every deny, and a non-ASCII deny
                    // is ambiguous against every target under this cwd: Unicode wildcard and
                    // filesystem-normalization semantics are out of scope, so neither side is
                    // ever provably disjoint (T36B-r) and the read refuses.
                    (None, _) | (Some(_), None) => return ReadProof::Unproven,
                }
            }
            // 4. Glob denies: an unreachable base is irrelevant; every reachable suffix —
            //    the target's own and each ancestor's, including the empty base-equality
            //    suffix — must provably not match.
            DenyRule::Glob { base, pattern } => {
                let Some(base_components) = resolve_selector(base, &cwd) else {
                    return ReadProof::Unproven;
                };
                // Relevance is decided on the folded components, so a casing variant of the
                // base is never declared provably disjoint. A non-ASCII component on either
                // side is ambiguity, never disjointness: filesystem normalization could bind
                // the base to an ASCII spelling, so every target under this cwd refuses
                // (T36B-r) instead of clearing the glob.
                match (&target_folded, fold_components(&base_components)) {
                    (Some(target), Some(base)) => {
                        if !target.starts_with(base.as_slice()) {
                            continue;
                        }
                    }
                    (None, _) | (Some(_), None) => return ReadProof::Unproven,
                }
                if let Some(limit) = shape.glob_scan_max_depth {
                    // Conservative depth accounting: base depth zero, one per target component
                    // below the base. Exceeding a present cap refuses; it never clears a glob.
                    if target.len() - base_components.len() > limit as usize {
                        return ReadProof::Unproven;
                    }
                }
                let Some(pattern_segments) = glob_pattern_segments(pattern) else {
                    return ReadProof::Unproven;
                };
                // The compared paths are the target and every ancestor at or below the base:
                // prefix by prefix below the base, so a pattern matching an ancestor directory
                // (for example `*.key` against `x.key/child`) refuses the descendant too. The
                // empty prefix is base equality itself, which a wildcard-free stored remainder
                // (empty pattern) matches exactly.
                let below = &target[base_components.len()..];
                for end in 0..=below.len() {
                    match glob_match_segments(&pattern_segments, &below[..end]) {
                        GlobMatch::DoesNotMatch => {}
                        GlobMatch::Matches | GlobMatch::Unknown => return ReadProof::Unproven,
                    }
                }
            }
        }
    }
    ReadProof::Proven
}

/// ASCII-folds every component of one path, or `None` when any component is non-ASCII (T36B).
///
/// `None` is ambiguity, never disjointness: callers must treat it as `Unproven` on whichever
/// side of a deny comparison it appears (T36B-r).
fn fold_components(components: &[String]) -> Option<Vec<String>> {
    components
        .iter()
        .map(|component| {
            if component.is_ascii() {
                Some(component.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// Resolves one deny selector into absolute components in the proof's coordinate system (T36B).
///
/// `cwd` is the trusted cwd's strict components; workspace-relative selectors join beneath it
/// and absolute selectors are taken as-is. Special selectors cannot be resolved to a path in
/// this coordinate system and deliberately yield `None` (which callers treat as unproven);
/// derivation never produces them for denies, so this is defense in depth, not a live branch.
fn resolve_selector(selector: &Selector, cwd: &[String]) -> Option<Vec<String>> {
    match selector {
        Selector::WorkspaceRelative(components) => {
            let mut resolved = cwd.to_vec();
            resolved.extend(components.iter().cloned());
            Some(resolved)
        }
        Selector::Absolute(components) => Some(components.clone()),
        Selector::Root | Selector::SlashTmp | Selector::Tmpdir => None,
    }
}

/// Splits one stored glob deny remainder into conservative pattern segments (T36B).
///
/// The stored nonempty remainder starts with the base boundary separator; exactly that one
/// leading separator is removed and the rest splits on separators. Any empty segment
/// (doubled or trailing separator), any bracket/brace/negation/escape syntax, any `**` that
/// is not a complete segment, and any non-ASCII byte are unsupported: the caller must treat
/// the whole glob as unprovable instead of falling back to literal matching.
fn glob_pattern_segments(pattern: &str) -> Option<Vec<&str>> {
    if pattern.is_empty() {
        // A wildcard-free glob stores the empty remainder and matches exactly its base.
        return Some(Vec::new());
    }
    let pattern = pattern.strip_prefix('/')?;
    if pattern.is_empty() {
        // A lone separator leaves no pattern at all: a malformed base boundary.
        return None;
    }
    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        if segment.is_empty()
            || !segment.is_ascii()
            || segment.contains(['\\', '[', ']', '{', '}', '!'])
            || (segment.contains("**") && segment != "**")
        {
            return None;
        }
        segments.push(segment);
    }
    Some(segments)
}

/// Outcome of one conservative anchored whole-path glob comparison (T36B).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GlobMatch {
    /// The pattern provably matches the candidate path.
    Matches,
    /// The pattern provably cannot match the candidate path.
    DoesNotMatch,
    /// The pattern uses syntax or characters this conservative matcher does not interpret;
    /// the caller must refuse rather than fall back to a literal comparison.
    Unknown,
}

/// Anchored whole-path conservative glob comparison over complete segments (T36B).
///
/// Supported syntax: literal bytes (ASCII, case-folded on both sides — a deny match must
/// survive a casing variant), `*` for zero or more characters within one segment, `?` for
/// exactly one character within one segment, and a complete `**` segment for zero or more
/// complete segments (including zero depth). Dotfiles get no special treatment. Both inputs
/// must be ASCII — the caller rejects non-ASCII patterns, and any non-ASCII candidate segment
/// is `Unknown` because Unicode wildcard and filesystem-normalization semantics are out of
/// scope. Matching is bounded dynamic programming: the per-segment wildcard DP is
/// O(pattern × name) and the segment sweep is O(pattern segments × path segments), so no
/// input can trigger exponential backtracking.
fn glob_match_segments(pattern: &[&str], path: &[String]) -> GlobMatch {
    let mut reachable = vec![0_usize];
    for segment in pattern {
        let mut next = Vec::new();
        if *segment == "**" {
            // Zero or more complete segments: every position from the minimum reachable one
            // onward stays reachable. `reachable` is always sorted and dense from its start.
            let start = reachable[0];
            next.extend(start..=path.len());
        } else {
            let folded = match ascii_fold(segment) {
                Some(folded) => folded,
                None => return GlobMatch::Unknown,
            };
            for &position in &reachable {
                if position < path.len() {
                    let candidate = match ascii_fold(&path[position]) {
                        Some(candidate) => candidate,
                        None => return GlobMatch::Unknown,
                    };
                    if wildcard_segment_matches(folded.as_bytes(), candidate.as_bytes()) {
                        next.push(position + 1);
                    }
                }
            }
        }
        if next.is_empty() {
            return GlobMatch::DoesNotMatch;
        }
        reachable = next;
    }
    if reachable.contains(&(path.len())) {
        GlobMatch::Matches
    } else {
        GlobMatch::DoesNotMatch
    }
}

/// Folds one ASCII segment to lowercase, or `None` when it contains non-ASCII bytes (T36B).
fn ascii_fold(segment: &str) -> Option<String> {
    if !segment.is_ascii() {
        return None;
    }
    Some(segment.to_ascii_lowercase())
}

/// Bounded wildcard DP for one segment: `*` spans characters, `?` matches one (T36B).
///
/// `pattern` and `name` are already ASCII-folded. Classic two-row dynamic programming over
/// prefix lengths, so worst-case work is linear in the product of the input lengths.
fn wildcard_segment_matches(pattern: &[u8], name: &[u8]) -> bool {
    let mut previous = vec![false; name.len() + 1];
    let mut current = vec![false; name.len() + 1];
    previous[0] = true;
    for &byte in pattern {
        current[0] = previous[0] && byte == b'*';
        for (name_index, &name_byte) in name.iter().enumerate() {
            current[name_index + 1] = match byte {
                // `*` extends a match that either already covered this name prefix without
                // consuming it, or covered one fewer name character and consumes this one.
                b'*' => previous[name_index + 1] || current[name_index],
                b'?' => previous[name_index],
                literal => previous[name_index] && literal == name_byte,
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[name.len()]
}

/// Returns the closed `missing_path_behavior` spelling, or `None` when the entry was absent.
fn missing_behavior_tag(missing: MissingPath) -> Option<&'static str> {
    match missing {
        MissingPath::Absent => None,
        MissingPath::Skip => Some("skip"),
    }
}

/// Builds one JSON array whose elements are sorted by canonical encoded bytes and deduplicated.
///
/// `super::canonical_json` preserves array order, so set ordering must happen before
/// serialization; this is that ordering step, applied to the already-parsed rule values.
fn sorted_canonical_array(rules: impl Iterator<Item = Value>) -> Vec<Value> {
    let encoded: BTreeSet<String> = rules.map(|rule| canonical_json(&rule)).collect();
    encoded
        .into_iter()
        .map(|encoded| serde_json::from_str(&encoded).expect("canonical JSON reparse"))
        .collect()
}

/// Returns the cwd's canonical components after binding them to the trusted candidate (T35B-r).
///
/// The RAW `sandboxCwd` string is validated with the full local path/URI grammar BEFORE any
/// canonicalization is consulted, so a fragment, query, host part, or percent-encoding in the
/// original spelling refuses derivation instead of surviving as an ordinary path character.
/// Both sides are then canonicalized through the filesystem (`realpath` of an existing
/// directory; a failed canonicalization refuses) and compared component-wise: the trusted
/// candidate or Workspace worktree is the directory the operation actually touches, and
/// cwd-derived authority is never allowed to relocate beneath different restrictions. Two
/// spellings of the same real directory — for example the symlinked `/var/folders/...` form of
/// the per-user temporary directory and its canonical `/private/var/folders/...` form — are one
/// directory and bind, while a different real directory still refuses. A `/` cwd also refuses:
/// it would make every absolute path a workspace-relative descendant and widen the template.
fn cwd_components(
    state: &HostSandboxState,
    trusted_cwd: &Path,
) -> Result<Vec<String>, UnsupportedShape> {
    let raw = parse_path_components(state.sandbox_cwd())?;
    if raw.is_empty() {
        return Err(UnsupportedShape("root cwd"));
    }
    let components = trusted_path_components(state.cwd())?;
    if trusted_path_components(trusted_cwd)? != components {
        return Err(UnsupportedShape("cwd binding"));
    }
    Ok(components)
}

/// Returns one trusted candidate/worktree path's canonical strict components (T35B-r).
///
/// The path is canonicalized through the filesystem first: `realpath` of an existing directory
/// is the filesystem's own identity for that directory, so the symlinked `$TMPDIR` spelling and
/// its canonical form bind to one directory while a failed canonicalization (a missing or
/// otherwise unresolvable candidate) refuses. Only an absolute canonical path whose every
/// component is a normal, nonempty, non-UTF8-lossy name is accepted; the comparison against the
/// state's cwd is component-wise, never a string prefix, so a different real directory — a
/// `/work-escape` sibling, for example — is a different directory and refuses.
fn trusted_path_components(trusted_cwd: &Path) -> Result<Vec<String>, UnsupportedShape> {
    canonical_path_components(
        &std::fs::canonicalize(trusted_cwd)
            .map_err(|_| UnsupportedShape("trusted cwd canonicalization"))?,
    )
}

/// Returns one already-canonical absolute path's strict raw components (T35B-r).
fn canonical_path_components(canonical: &Path) -> Result<Vec<String>, UnsupportedShape> {
    let mut components = Vec::new();
    for component in canonical.components() {
        match component {
            std::path::Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or(UnsupportedShape("non-UTF8 trusted cwd"))?;
                if name.is_empty() || name == "." || name == ".." {
                    return Err(UnsupportedShape("trusted cwd components"));
                }
                components.push(name.to_owned());
            }
            std::path::Component::RootDir => {}
            _ => return Err(UnsupportedShape("trusted cwd components")),
        }
    }
    if components.is_empty() {
        return Err(UnsupportedShape("trusted root cwd"));
    }
    Ok(components)
}

/// Refuses cwd/absolute-restriction overlaps whose precedence this build cannot prove (T35B-r).
///
/// Moving the portable grant is only sound while every absolute rule keeps the same precedence
/// relative to the cwd subtree. Two conservative directions refuse: any absolute rule sitting
/// inside the cwd subtree (structurally unreachable after cwd-relative normalization, but
/// asserted so a future classification change can never silently admit an unproven overlap),
/// and any absolute *path* deny whose subtree contains the cwd, where a relocated more-specific
/// cwd grant could reopen the denied subtree — the reviewed relocation attack. An absolute
/// *grant* that contains the cwd is the normal portability case and does not refuse, and a glob
/// deny whose literal base contains the cwd does not refuse either: real captured credential
/// globs carry exactly that shape in both the accepted capture and the live state, so their
/// grant interplay is equally (un)proven on both sides and is deliberately not modeled here.
fn refuse_cwd_overlap(
    rules: &BTreeMap<Selector, (Access, MissingPath)>,
    denies: &BTreeSet<DenyRule>,
    cwd_components: &[String],
) -> Result<(), UnsupportedShape> {
    let inside_cwd = |absolute: &Vec<String>| absolute.starts_with(cwd_components);
    let contains_cwd = |absolute: &Vec<String>| {
        absolute.len() < cwd_components.len() && cwd_components.starts_with(absolute.as_slice())
    };
    for selector in rules.keys() {
        if let Selector::Absolute(absolute) = selector
            && inside_cwd(absolute)
        {
            return Err(UnsupportedShape("cwd overlap"));
        }
    }
    for deny in denies {
        match deny {
            DenyRule::Path(Selector::Absolute(absolute)) => {
                if inside_cwd(absolute) || contains_cwd(absolute) {
                    return Err(UnsupportedShape("cwd overlap"));
                }
            }
            DenyRule::Glob { base, .. } => {
                if let Selector::Absolute(absolute) = base
                    && inside_cwd(absolute)
                {
                    return Err(UnsupportedShape("cwd overlap"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Splits one absolute local path or `file://` local URI into strict raw components (T35B).
///
/// The grammar is deliberately minimal and rejects before any normalization: only an absolute
/// local path (`/…`) or a `file://` URI whose remainder is itself an absolute local path is
/// accepted. Refused are `..` or embedded `.` components (checked on raw components, not
/// `Path::components`, which silently normalizes), NUL bytes, empty components from `//` or a
/// trailing separator, a URI host part, percent-encoding, and query or fragment suffixes. Case
/// and Unicode bytes are preserved: no folding or normalization ever merges two spellings.
fn parse_path_components(raw: &str) -> Result<Vec<String>, UnsupportedShape> {
    // A backslash is a literal byte in a path but an escape in a Codex glob, so a path or glob
    // base containing one can change which files a byte-identical glob deny matches once the cwd
    // is normalized away. No supported workspace path needs one: refuse before normalization
    // (T35B-r2).
    if raw.contains('\\') {
        return Err(UnsupportedShape("backslash in path"));
    }
    let path = if let Some(rest) = raw.strip_prefix("file://") {
        if !rest.starts_with('/') || rest.contains(['%', '?', '#']) {
            // `file://host/…`, percent-encoded, query, and fragment variants are all outside
            // the explicitly supported local grammar.
            return Err(UnsupportedShape("file URI"));
        }
        rest
    } else if raw.starts_with('/') {
        raw
    } else {
        return Err(UnsupportedShape("relative path"));
    };
    if path.as_bytes().contains(&0) {
        return Err(UnsupportedShape("NUL byte"));
    }
    let mut components = Vec::new();
    for component in path.split('/').skip(1) {
        if component.is_empty() || component == "." || component == ".." {
            return Err(UnsupportedShape("path components"));
        }
        components.push(component.to_owned());
    }
    Ok(components)
}

/// Returns the glob metacharacters this module splits patterns on.
fn is_glob_metacharacter(byte: u8) -> bool {
    matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}')
}

/// Classifies one `{"type":"path"}` or `{"type":"glob_pattern"}` deny selector value.
fn parse_deny_selector(
    path: &serde_json::Map<String, Value>,
    cwd_components: &[String],
) -> Result<DenyRule, UnsupportedShape> {
    match path.get("type").and_then(Value::as_str) {
        Some("path") => {
            if path.len() != 2 {
                return Err(UnsupportedShape("deny path fields"));
            }
            let raw = path
                .get("path")
                .and_then(Value::as_str)
                .ok_or(UnsupportedShape("deny path value"))?;
            Ok(DenyRule::Path(parse_path_selector(raw, cwd_components)?))
        }
        Some("glob_pattern") => {
            if path.len() != 2 {
                return Err(UnsupportedShape("deny glob fields"));
            }
            let pattern = path
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or(UnsupportedShape("deny glob value"))?;
            parse_glob_deny(pattern, cwd_components)
        }
        _ => Err(UnsupportedShape("deny selector type")),
    }
}

/// Splits one exact glob pattern into its base selector and byte-exact pattern remainder (T35B).
///
/// A wildcard-free glob stays a tagged glob deny — it is never collapsed into a path deny,
/// because subtree containment and exact glob matching are different semantics and a backslash
/// has literal path meaning but glob-escape meaning. The base is the literal prefix ending at
/// the segment boundary at or before the first metacharacter (the whole pattern when it has
/// none); it is classified by component-wise containment in the trusted cwd (a `/work-escape`
/// sibling never joins a `/work` base), and the remainder starting at that boundary is kept
/// byte-for-byte. No glob containment or rewriting is ever computed, so a weakened pattern is
/// simply a different required rule.
fn parse_glob_deny(pattern: &str, cwd_components: &[String]) -> Result<DenyRule, UnsupportedShape> {
    // Escapes are refused in the whole pattern, not only in the base the path grammar sees:
    // an escaped metacharacter in the tail would otherwise survive normalization (T35B-r2).
    if pattern.contains('\\') {
        return Err(UnsupportedShape("backslash in glob"));
    }
    let (base, glob) = match pattern
        .as_bytes()
        .iter()
        .position(|byte| is_glob_metacharacter(*byte))
    {
        None => (pattern, ""),
        // The literal base ends at the segment boundary at or before the first metacharacter.
        Some(first_meta) => {
            let base_end = pattern[..first_meta]
                .rfind('/')
                .ok_or(UnsupportedShape("deny glob base"))?;
            pattern.split_at(base_end)
        }
    };
    Ok(DenyRule::Glob {
        base: parse_path_selector(base, cwd_components)?,
        pattern: glob.to_owned(),
    })
}

/// Parses one `{"type":"path"}` selector value into its tagged selector form.
///
/// `cwd_components` is the trusted cwd's strict components; a path at or below the cwd becomes
/// [`Selector::WorkspaceRelative`] with the suffix components, everything else stays
/// [`Selector::Absolute`]. Containment is component-wise, never a string prefix.
fn parse_path_selector(raw: &str, cwd_components: &[String]) -> Result<Selector, UnsupportedShape> {
    let components = parse_path_components(raw)?;
    if components.len() >= cwd_components.len()
        && components[..cwd_components.len()] == *cwd_components
    {
        Ok(Selector::WorkspaceRelative(
            components[cwd_components.len()..].to_owned(),
        ))
    } else {
        Ok(Selector::Absolute(components))
    }
}

/// Parses one entry object into its rule, accumulating into the shape's maps (T35B).
fn derive_entry(
    entry: &Value,
    cwd_components: &[String],
    rules: &mut BTreeMap<Selector, (Access, MissingPath)>,
    denies: &mut BTreeSet<DenyRule>,
) -> Result<(), UnsupportedShape> {
    let entry = entry.as_object().ok_or(UnsupportedShape("entry object"))?;
    if !entry
        .keys()
        .all(|key| matches!(key.as_str(), "access" | "path" | "missing_path_behavior"))
    {
        return Err(UnsupportedShape("entry fields"));
    }
    let access = match entry.get("access").and_then(Value::as_str) {
        Some("read") => Access::Read,
        Some("write") => Access::Write,
        Some("deny") => {
            // `missing_path_behavior` on a deny has unreviewed semantics in this build, so a
            // deny carrying it refuses derivation instead of guessing.
            if entry.contains_key("missing_path_behavior") {
                return Err(UnsupportedShape("deny missing_path_behavior"));
            }
            let path = entry
                .get("path")
                .and_then(Value::as_object)
                .ok_or(UnsupportedShape("deny path"))?;
            denies.insert(parse_deny_selector(path, cwd_components)?);
            return Ok(());
        }
        _ => return Err(UnsupportedShape("access")),
    };
    let missing = match entry.get("missing_path_behavior") {
        None => MissingPath::Absent,
        Some(Value::String(value)) if value == "skip" => MissingPath::Skip,
        Some(_) => return Err(UnsupportedShape("missing_path_behavior")),
    };
    let path = entry
        .get("path")
        .and_then(Value::as_object)
        .ok_or(UnsupportedShape("entry path"))?;
    let selector = match path.get("type").and_then(Value::as_str) {
        Some("path") => {
            if path.len() != 2 {
                return Err(UnsupportedShape("selector fields"));
            }
            let raw = path
                .get("path")
                .and_then(Value::as_str)
                .ok_or(UnsupportedShape("path value"))?;
            parse_path_selector(raw, cwd_components)?
        }
        Some("special") => {
            if path.len() != 2 {
                return Err(UnsupportedShape("selector fields"));
            }
            let value = path
                .get("value")
                .and_then(Value::as_object)
                .ok_or(UnsupportedShape("special value"))?;
            if value.len() != 1 {
                return Err(UnsupportedShape("special value fields"));
            }
            match value.get("kind").and_then(Value::as_str) {
                Some("root") => Selector::Root,
                Some("slash_tmp") => Selector::SlashTmp,
                Some("tmpdir") => Selector::Tmpdir,
                _ => return Err(UnsupportedShape("special kind")),
            }
        }
        _ => return Err(UnsupportedShape("selector type")),
    };
    match rules.entry(selector) {
        std::collections::btree_map::Entry::Occupied(occupied) => {
            // Identical duplicates deduplicate; anything else at the same selector is a
            // conflict this build refuses rather than resolving by array order or strength.
            if occupied.get() != &(access, missing) {
                return Err(UnsupportedShape("conflicting accesses"));
            }
        }
        std::collections::btree_map::Entry::Vacant(vacant) => {
            vacant.insert((access, missing));
        }
    }
    Ok(())
}

/// Exercises the opt-in family with disposable, sanitized sandbox states.
#[cfg(test)]
mod visualization_tests {
    use super::*;
    use crate::execution::{
        CommandKind, ControlledCommand, D03ProfileEvidence, ExecutionProfileCatalog,
        ExecutionProfileTemplate, LocalExecutionPolicy, PersistedProfileRecord, RequestError,
        ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
    };
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};

    /// Creates two real leaf directories and one capture whose only outside write is the first.
    fn fixture() -> (std::path::PathBuf, Value, String, String) {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "agent-ide-v3-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
        let namespace = root.join(".codex/visualizations");
        let first = namespace.join("2026/09/23/11111111-1111-1111-1111-111111111111");
        let second = namespace.join("2026/09/24/22222222-2222-2222-2222-222222222222");
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.to_str().unwrap().to_owned();
        let second = second.to_str().unwrap().to_owned();
        let mut entries = vec![
            json!({"access":"write","path":{"type":"path","path":root.join("work")}}),
            json!({"access":"write","path":{"type":"path","path":first}}),
        ];
        for suffix in ["/**/*.key", "/**/*.pem", "/**/.env", "/**/.env.*"] {
            entries.push(json!({"access":"deny","path":{"type":"glob_pattern","pattern":format!("{first}{suffix}")}}));
        }
        let value = json!({
            "permissionProfile":{"type":"managed","network":"enabled","file_system":{"type":"restricted","entries":entries}},
            "codexLinuxSandboxExe":null,
            "sandboxCwd":root.join("work"),
            "useLegacyLandlock":false,
        });
        (root, value, first, second)
    }

    /// Changes only the complete leaf and its four matching denies in a copied capture.
    fn moved(value: &Value, first: &str, second: &str) -> Value {
        serde_json::from_str(&value.to_string().replace(first, second)).unwrap()
    }

    /// Keeps a v3 visualization leaf from becoming a write route into linked Git metadata.
    #[test]
    fn visualization_leaf_cannot_cover_linked_common_git_directory() {
        let (root, captured, first, _) = fixture();
        let common = std::path::PathBuf::from(&first).join("git-common");
        let admin = common.join("worktrees/guard");
        std::fs::create_dir_all(&admin).unwrap();
        let cwd = root.join("work");
        std::fs::write(cwd.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), format!("{}\n", common.display())).unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", cwd.join(".git").display()),
        )
        .unwrap();
        let state = HostSandboxState::parse(Some(captured)).unwrap();
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence_v3("visualization", 1, &state)
                .unwrap(),
        ])
        .unwrap();
        let authority = WorkspaceAuthority::from_workspace_with_git_common_dir(
            "v3",
            "1",
            cwd.clone(),
            common,
            1,
        )
        .unwrap();
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Job,
            "/usr/bin/true".into(),
            vec![],
            cwd,
            BTreeMap::new(),
        )
        .unwrap();
        let policy =
            LocalExecutionPolicy::new(BTreeSet::from(["/usr/bin/true".into()]), 4096, 0, false)
                .unwrap();
        assert_eq!(
            ValidatedExecutionRequest::validate(
                ValidatedHostInvocation::from_verified_binding("v3", state).unwrap(),
                authority,
                command,
                &policy,
                &catalog,
            )
            .unwrap_err(),
            RequestError::GitMetadataWriteOverlap
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Proves two distinct tasks share one v3 record while v2 and record restoration stay exact.
    #[test]
    fn two_visualization_leaves_share_only_v3_family() {
        let (root, captured, first, second) = fixture();
        let accepted = HostSandboxState::parse(Some(captured.clone())).unwrap();
        let live = HostSandboxState::parse(Some(moved(&captured, &first, &second))).unwrap();
        let v3 =
            ExecutionProfileTemplate::from_execution_evidence_v3("visualization", 1, &accepted)
                .unwrap();
        let v2 =
            ExecutionProfileTemplate::from_execution_evidence_v2("exact", 1, &accepted).unwrap();
        let both =
            ExecutionProfileCatalog::from_execution_evidence(vec![v3.clone(), v2.clone()]).unwrap();
        assert_eq!(
            both.permit(&accepted, accepted.cwd()).unwrap().template.id,
            "exact"
        );
        let rotated =
            ExecutionProfileTemplate::from_execution_evidence_v3("rotated", 2, &live).unwrap();
        assert!(
            ExecutionProfileCatalog::from_execution_evidence(vec![v3.clone(), rotated]).is_err()
        );
        assert!(
            ExecutionProfileCatalog::from_execution_evidence(vec![v3])
                .unwrap()
                .permit(&live, live.cwd())
                .is_ok()
        );
        assert!(
            ExecutionProfileCatalog::from_execution_evidence(vec![v2])
                .unwrap()
                .permit(&live, live.cwd())
                .is_err()
        );
        let record = PersistedProfileRecord::from_execution_evidence_versioned(
            "visualization",
            1,
            D03ProfileEvidence {
                provider_binary: "codex".into(),
                toolchain: "toolchain".into(),
                configuration: "default".into(),
                trust: "local".into(),
                transport: "managed".into(),
                d03_evidence: "real-d03".into(),
            },
            &accepted,
            3,
        )
        .unwrap();
        assert_eq!(
            PersistedProfileRecord::from_json(&record.to_json()).unwrap(),
            record
        );
        assert!(record.matches_state(&accepted));
        assert!(!record.matches_state(&live));
        std::fs::remove_dir_all(root.join(".codex")).unwrap();
        assert!(
            record.matches_state(&accepted),
            "the accepted namespace may have been cleaned up"
        );
        let catalog = ExecutionProfileCatalog::from_persisted_records(
            vec![(record.clone(), accepted.clone())],
            &[record],
        )
        .unwrap();
        assert!(catalog.permit(&live, live.cwd()).is_err());
        std::os::unix::fs::symlink(root.join("work"), root.join(".codex")).unwrap();
        assert!(catalog.permit(&live, live.cwd()).is_err());
        std::fs::remove_file(root.join(".codex")).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let permit = catalog.permit(&live, live.cwd()).unwrap();
        std::fs::remove_dir_all(root.join(".codex")).unwrap();
        std::os::unix::fs::symlink(root.join("work"), root.join(".codex")).unwrap();
        assert!(permit.recheck_live_v3(&live, live.cwd()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Refuses malformed paths, omitted or changed denies, extra roots, and ancestor/sibling writes.
    #[test]
    fn visualization_family_refuses_extra_authority_or_incomplete_denials() {
        let (root, captured, first, second) = fixture();
        let mut cases = Vec::new();
        cases.push(moved(&captured, &first, &format!("{second}/../bad")));
        cases.push(moved(
            &captured,
            &first,
            &second.replace("2026/09/24", "2026/02/30"),
        ));
        let mut missing = captured.clone();
        missing["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap()
            .pop();
        cases.push(missing);
        let mut altered = captured.clone();
        altered["permissionProfile"]["file_system"]["entries"][2]["path"]["pattern"] =
            json!(format!("{first}/**/*.keys"));
        cases.push(altered);
        for extra in [
            root.join(".codex/visualizations"),
            std::path::PathBuf::from(&second),
        ] {
            let mut value = captured.clone();
            value["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(json!({"access":"write","path":{"type":"path","path":extra}}));
            cases.push(value);
        }
        let mut root_write = captured.clone();
        root_write["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"access":"write","path":{"type":"special","value":{"kind":"root"}}}));
        cases.push(root_write);
        let mut ancestor_write = captured.clone();
        ancestor_write["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"access":"write","path":{"type":"path","path":root.join(".codex")}}));
        cases.push(ancestor_write);
        for value in cases {
            let state = HostSandboxState::parse(Some(value)).unwrap();
            assert!(state.shape_v3(state.cwd()).is_err());
        }
        let mut outside = captured.clone();
        outside["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"access":"write","path":{"type":"path","path":root.join("outside")}}));
        let accepted = HostSandboxState::parse(Some(captured)).unwrap();
        let live = HostSandboxState::parse(Some(outside)).unwrap();
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence_v3("visualization", 1, &accepted)
                .unwrap(),
        ])
        .unwrap();
        assert!(catalog.permit(&live, live.cwd()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Refuses a leaf whose directory resolves outside the accepted namespace through a symlink.
    #[test]
    fn visualization_family_refuses_symlink_escape() {
        let (root, captured, first, second) = fixture();
        std::fs::remove_dir_all(&second).unwrap();
        std::os::unix::fs::symlink(root.join("work"), &second).unwrap();
        let live = HostSandboxState::parse(Some(moved(&captured, &first, &second))).unwrap();
        assert!(live.shape_v3(live.cwd()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Focused unit tests for the conservative matcher and pattern splitter (T36B).
///
/// Patterns are given in their exact stored spelling: the stored nonempty remainder starts
/// with the base boundary separator, and `glob_pattern_segments` removes exactly that one
/// separator before splitting.
#[cfg(test)]
mod matcher_tests {
    use super::{GlobMatch, glob_match_segments, glob_pattern_segments};

    /// Builds pattern segments exactly as a derived deny stores them.
    fn pattern(raw: &str) -> Vec<&str> {
        glob_pattern_segments(raw).unwrap_or_else(|| panic!("pattern must parse: {raw}"))
    }

    /// Classifies one comparison against a candidate path of complete segments.
    fn classify(raw: &str, path: &[&str]) -> GlobMatch {
        glob_match_segments(
            &pattern(raw),
            &path.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn literals_match_themselves_whole_path_only() {
        assert_eq!(classify("/.env", &[".env"]), GlobMatch::Matches);
        assert_eq!(classify("/.env", &[".env2"]), GlobMatch::DoesNotMatch);
        // Anchored: a whole-path match never matches a deeper or shallower path.
        assert_eq!(classify("/.env", &["src", ".env"]), GlobMatch::DoesNotMatch);
        assert_eq!(classify("/src/.env", &[".env"]), GlobMatch::DoesNotMatch);
        assert_eq!(
            classify("/src/.env", &["src", ".env", "child"]),
            GlobMatch::DoesNotMatch
        );
    }

    #[test]
    fn star_spans_within_one_segment_only() {
        assert_eq!(classify("/*.key", &["x.key"]), GlobMatch::Matches);
        assert_eq!(classify("/*.key", &[".key"]), GlobMatch::Matches);
        assert_eq!(
            classify("/*.key", &["dir", "x.key"]),
            GlobMatch::DoesNotMatch
        );
        assert_eq!(
            classify("/src/*.rs", &["a", "b.rs"]),
            GlobMatch::DoesNotMatch
        );
        assert_eq!(classify("/src/*.rs", &["src", "b.rs"]), GlobMatch::Matches);
        assert_eq!(classify("/a*c", &["abc"]), GlobMatch::Matches);
        // `*` spans any characters, so `a*c` does match `abbc`.
        assert_eq!(classify("/a*c", &["abbc"]), GlobMatch::Matches);
    }

    #[test]
    fn question_matches_exactly_one_character() {
        assert_eq!(classify("/?.key", &["x.key"]), GlobMatch::Matches);
        assert_eq!(classify("/?.key", &[".key"]), GlobMatch::DoesNotMatch);
        assert_eq!(classify("/?.key", &["xy.key"]), GlobMatch::DoesNotMatch);
        assert_eq!(classify("/??", &["ab"]), GlobMatch::Matches);
        assert_eq!(classify("/??", &["abc"]), GlobMatch::DoesNotMatch);
    }

    #[test]
    fn double_star_segment_matches_zero_or_many_segments() {
        assert_eq!(classify("/**/.env", &[".env"]), GlobMatch::Matches);
        assert_eq!(classify("/**/.env", &["a", ".env"]), GlobMatch::Matches);
        assert_eq!(
            classify("/**/.env", &["a", "b", "c", ".env"]),
            GlobMatch::Matches
        );
        assert_eq!(
            classify("/**/.env", &["a", "b", "c"]),
            GlobMatch::DoesNotMatch
        );
        assert_eq!(classify("/a/**/b", &["a", "b"]), GlobMatch::Matches);
        assert_eq!(
            classify("/a/**/b", &["a", "x", "y", "b"]),
            GlobMatch::Matches
        );
        assert_eq!(
            classify("/a/**/b", &["a", "x", "y", "c"]),
            GlobMatch::DoesNotMatch
        );
        assert_eq!(classify("/**", &[]), GlobMatch::Matches);
        assert_eq!(classify("/**", &["a", "b"]), GlobMatch::Matches);
    }

    #[test]
    fn dotfiles_and_boundaries_get_no_special_treatment() {
        // A literal dot is a dot, and `*` still spans dotfile names.
        assert_eq!(classify("/*", &[".env"]), GlobMatch::Matches);
        assert_eq!(classify("/.env.*", &[".env.local"]), GlobMatch::Matches);
        assert_eq!(classify("/.env.*", &[".env"]), GlobMatch::DoesNotMatch);
        assert_eq!(
            classify("/.env.*", &[".env.production.key"]),
            GlobMatch::Matches
        );
    }

    #[test]
    fn ancestor_prefixes_are_compared_by_the_caller_not_the_matcher() {
        // The matcher itself is a plain anchored whole-path comparison; read_proof applies it
        // to every ancestor-or-equal prefix below the base. `*.key` cannot match the
        // two-segment child path here...
        assert_eq!(
            classify("/*.key", &["x.key", "child"]),
            GlobMatch::DoesNotMatch
        );
        // ...but it does match the ancestor directory itself, which read_proof therefore
        // refuses on (`*.key` against `x.key/child` is Unproven).
        assert_eq!(classify("/*.key", &["x.key"]), GlobMatch::Matches);
    }

    #[test]
    fn ascii_case_folding_makes_deny_matches_case_insensitive() {
        assert_eq!(classify("/.ENV", &[".env"]), GlobMatch::Matches);
        assert_eq!(
            classify("/**/*.KEY", &["certs", "X.key"]),
            GlobMatch::Matches
        );
        assert_eq!(classify("/Readme?", &["README2"]), GlobMatch::Matches);
    }

    #[test]
    fn unsupported_syntax_and_characters_are_unknown_never_literal() {
        for raw in [
            "/[a-z].env",  // bracket class
            "/se?ret[!x]", // negated bracket class
            "/{a,b}.env",  // brace alternation
            "/a**b",       // embedded `**`
            "/**//x",      // doubled separator
            "/**/x/",      // trailing separator
            "/café",       // non-ASCII pattern
            "/ba\\ck",     // escape (already refused at derivation, never literal here)
        ] {
            assert_eq!(
                glob_pattern_segments(raw),
                None,
                "{raw} must be unsupported"
            );
        }
    }

    #[test]
    fn non_ascii_candidates_are_unknown() {
        assert_eq!(classify("/**/x", &["café"]), GlobMatch::Unknown);
        assert_ne!(classify("/**/x", &["x"]), GlobMatch::Unknown);
    }

    #[test]
    fn empty_remainder_matches_only_base_equality() {
        let empty = glob_pattern_segments("").unwrap();
        assert!(empty.is_empty(), "a wildcard-free glob stores no remainder");
        assert_eq!(classify("", &[]), GlobMatch::Matches);
        assert_eq!(classify("", &["anything"]), GlobMatch::DoesNotMatch);
    }

    #[test]
    fn matching_is_bounded_on_pathological_inputs() {
        // Bounded DP: an alternating-star pattern stays polynomial and exact. (Runs of
        // adjacent stars would be unsupported `**` syntax, not a backtracking hazard.)
        let pattern = format!("/x{}", "*y".repeat(64));
        let name = format!("x{}", "y".repeat(64));
        let longer = format!("{name}z");
        assert_eq!(classify(&pattern, &[name.as_str()]), GlobMatch::Matches);
        assert_eq!(
            classify(&pattern, &[longer.as_str()]),
            GlobMatch::DoesNotMatch
        );
    }
}
