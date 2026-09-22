//! Execution-private profile-shape v2 derivation and conservative narrowing proofs (T35B).
//!
//! This module replaces the opaque v1 digest *comparison* for admitted managed Codex sandbox
//! states with a closed, versioned semantic shape. It never rewrites replay JSON: every function
//! here only reads the already-validated [`HostSandboxState`]. The design is deliberately
//! incomplete subtyping: `prove_narrower` implements exactly seven sufficient conditions and
//! refuses everything else, so an unproven change is never admitted by a looser fallback.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{HostSandboxState, ProfileClass, canonical_json, is_normal_absolute};

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
    /// binding cwd-derived authority to a real directory stays with the existing strict
    /// cwd-equality checks, which this module does not change.
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
/// Only path denies and exact `glob_pattern` denies are recognized. Any other deny selector type
/// refuses the whole derivation, because an unknown deny vocabulary can hide a permission this
/// build cannot reason about.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DenyRule {
    /// A `{"type":"path"}` deny naming one recognized selector.
    Path(Selector),
    /// A `{"type":"glob_pattern"}` deny split into its literal absolute base selector and the
    /// raw glob tail that follows it. The tail is preserved byte-for-byte; no glob containment
    /// is ever computed, so a weakened pattern is simply a different required rule.
    Glob { base: Selector, tail: String },
}

impl DenyRule {
    /// Returns the tagged canonical JSON value for this deny rule.
    fn to_canonical(&self) -> Value {
        match self {
            DenyRule::Path(selector) => serde_json::json!({
                "type": "path",
                "selector": selector.to_canonical(),
            }),
            DenyRule::Glob { base, tail } => serde_json::json!({
                "type": "glob_pattern",
                "base": base.to_canonical(),
                "tail": tail,
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
    /// Exact `useLegacyLandlock` boolean: part of the replayed sandbox mechanism.
    pub(crate) use_legacy_landlock: bool,
    /// Exact `codexLinuxSandboxExe` helper path; a non-null helper is additionally pinned by the
    /// record's provider-binary identity, never matched loosely here.
    pub(crate) codex_linux_sandbox_exe: Option<String>,
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
    /// Every key set is closed: unknown top-level fields, unknown permission-profile or
    /// filesystem fields, unknown entry or selector fields, an unknown access, network, special
    /// kind, or missing-path value, a cwd of `/`, and any path outside the strict local grammar
    /// all return [`UnsupportedShape`] for the whole state. Identical duplicate rules
    /// deduplicate; the same selector with conflicting accesses refuses instead of guessing from
    /// array order or selecting the strongest access.
    pub(crate) fn derive(state: &HostSandboxState) -> Result<Self, UnsupportedShape> {
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
        let codex_linux_sandbox_exe = match object.get("codexLinuxSandboxExe") {
            Some(Value::Null) => None,
            Some(Value::String(path)) if is_normal_absolute(std::path::Path::new(path)) => {
                Some(path.clone())
            }
            Some(_) => return Err(UnsupportedShape("codexLinuxSandboxExe")),
            None => return Err(UnsupportedShape("codexLinuxSandboxExe")),
        };
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
        let cwd_components = cwd_components(state)?;
        let mut rules = BTreeMap::new();
        let mut denies = BTreeSet::new();
        for entry in entries {
            derive_entry(entry, &cwd_components, &mut rules, &mut denies)?;
        }
        Ok(Self {
            use_legacy_landlock,
            codex_linux_sandbox_exe,
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
                "codex_linux_sandbox_exe": self.codex_linux_sandbox_exe,
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
        //    type are invariants; the replayed mechanism must match exactly.
        if self.use_legacy_landlock != accepted.use_legacy_landlock
            || self.codex_linux_sandbox_exe != accepted.codex_linux_sandbox_exe
        {
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

/// Returns the cwd's strict raw components, refusing a root cwd (T35B).
///
/// A `/` cwd would make every absolute path a workspace-relative descendant, so no portable
/// shape is derived and the state stays with the strict legacy path.
fn cwd_components(state: &HostSandboxState) -> Result<Vec<String>, UnsupportedShape> {
    let text = state.cwd.to_str().ok_or(UnsupportedShape("non-UTF8 cwd"))?;
    let components = parse_path_components(text)?;
    if components.is_empty() {
        return Err(UnsupportedShape("root cwd"));
    }
    Ok(components)
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

/// Splits one exact glob pattern into its literal base selector and raw glob tail (T35B).
///
/// The base is every leading segment before the first metacharacter; it must itself satisfy the
/// strict path grammar and is classified workspace-relative only by component-wise containment
/// in the trusted cwd (a `/work-escape` sibling never joins a `/work` base). The tail is kept
/// byte-for-byte: no glob containment or rewriting is ever computed, so a weakened pattern is
/// simply a different required rule.
fn parse_glob_deny(pattern: &str, cwd_components: &[String]) -> Result<DenyRule, UnsupportedShape> {
    let Some(first_meta) = pattern
        .as_bytes()
        .iter()
        .position(|byte| is_glob_metacharacter(*byte))
    else {
        // A pattern with no metacharacter is a plain-path deny in glob notation; representing it
        // as the equivalent path deny keeps one identity per required exclusion.
        return parse_path_selector(pattern, cwd_components).map(DenyRule::Path);
    };
    // The base ends at the segment boundary at or before the first metacharacter.
    let base_end = pattern[..first_meta]
        .rfind('/')
        .ok_or(UnsupportedShape("deny glob base"))?;
    let (base, tail) = pattern.split_at(base_end);
    Ok(DenyRule::Glob {
        base: parse_path_selector(base, cwd_components)?,
        tail: tail.to_owned(),
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
