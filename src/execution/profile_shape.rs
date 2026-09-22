//! Execution-private profile-shape v2 derivation and conservative narrowing proofs (T35B).
//!
//! This module replaces the opaque v1 digest *comparison* for admitted managed Codex sandbox
//! states with a closed, versioned semantic shape. It never rewrites replay JSON: every function
//! here only reads the already-validated [`HostSandboxState`]. The design is deliberately
//! incomplete subtyping: `prove_narrower` implements exactly seven sufficient conditions and
//! refuses everything else, so an unproven change is never admitted by a looser fallback.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::{HostSandboxState, ProfileClass, canonical_json};

/// Domain separator prefixing every profile-shape v2 digest input (T35B).
const SHAPE_DIGEST_DOMAIN: &[u8] = b"agent-ide/profile-shape/v2\0";

/// Reports why one managed sandbox state has no derivable profile-shape v2 value (T35B).
///
/// The reason is one closed static description for operator diagnostics, never host-supplied
/// text. An unsupported shape never falls back to a looser comparison: the state keeps replaying
/// byte-for-byte and can only be admitted by a v1 template's exact legacy digest.
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

/// Returns the cwd's strict raw components after binding them to the trusted candidate (T35B-r).
///
/// The RAW `sandboxCwd` string is validated with the full local path/URI grammar before any
/// `file://` conversion is consulted, so a fragment, query, host part, or percent-encoding in the
/// original spelling refuses derivation instead of surviving as an ordinary path character. The
/// result must then equal `trusted_cwd` component-wise: the trusted candidate or Workspace
/// worktree is the directory the operation actually touches, and cwd-derived authority is never
/// allowed to relocate beneath different restrictions. A `/` cwd also refuses: it would make
/// every absolute path a workspace-relative descendant and widen the template.
fn cwd_components(
    state: &HostSandboxState,
    trusted_cwd: &Path,
) -> Result<Vec<String>, UnsupportedShape> {
    let components = parse_path_components(state.sandbox_cwd())?;
    if components.is_empty() {
        return Err(UnsupportedShape("root cwd"));
    }
    if trusted_path_components(trusted_cwd)? != components {
        return Err(UnsupportedShape("cwd binding"));
    }
    Ok(components)
}

/// Returns one trusted candidate/worktree path's strict raw components (T35B-r).
///
/// Only an absolute path whose every component is a normal, nonempty, non-UTF8-lossy name is
/// accepted; the comparison against the state's cwd is component-wise, never a string prefix,
/// so a symlinked alias or a `/work-escape` sibling is a different directory and refuses.
fn trusted_path_components(trusted_cwd: &Path) -> Result<Vec<String>, UnsupportedShape> {
    let mut components = Vec::new();
    for component in trusted_cwd.components() {
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
