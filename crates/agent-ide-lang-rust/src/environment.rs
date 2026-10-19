//! Rust environment resolution from accepted request facts and native project files.
//! Authoritative pins are bounded and guarded; incompatible, denied, invalid and unknown
//! compatibility refuse before children. Directory names and Cargo versions prove no rustc
//! release. Discovery stays inside the worktree and refuses exhausted bounds.

use agent_ide_core::{
    execution::seatbelt::ReadDeny,
    lang::environment::{EnvCandidate, EnvSource, ResolvedEnv, hash_input, read_input},
};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

/// Actual configured tools shared by card, checks, semantic and interactive routes. The compiler
/// identity is accepted evidence only when its executable digest still matches the declaration.
pub(crate) struct Tools {
    /// Prepared directory containing the accepted cargo/rustc pair.
    pub dir: PathBuf,
    /// Accepted rustc identity; absent in checks-only configurations means unknown.
    pub compiler: Option<String>,
    /// Explicit Cargo home, when configured.
    pub cargo_home: Option<PathBuf>,
}

/// Resolves only immutable launcher data, ignoring process AGENT variables. Accepted provider
/// bytes and check paths must agree; missing/changed/denied tools refuse before admission.
pub(crate) fn tools(
    config: &agent_ide_core::modules::contract::ModuleConfig,
) -> Result<Tools, String> {
    use agent_ide_core::assistance::launcher::{AcceptedExecutable, absolute};
    use std::os::unix::fs::PermissionsExt;
    let checks = config.checks.as_ref();
    let check_dir = checks
        .and_then(|section| section.get("toolchain_dir"))
        .and_then(|value| value.as_str())
        .map(PathBuf::from);
    let mut compiler = None;
    let mut provider_dir = None;
    if let Some(provider) = &config.provider {
        for name in ["cargo", "rustc"] {
            let accepted: AcceptedExecutable = serde_json::from_value(
                provider.get(name).cloned().unwrap_or_default(),
            )
            .map_err(|_| format!("accepted {name} declaration unavailable; use native cargo"))?;
            if accepted.validate().is_err()
                || config
                    .read_denies
                    .iter()
                    .any(|deny| deny.matches(&accepted.path))
                || hash_input(&accepted.path, &config.read_denies)
                    .map(|hash| hash.to_hex().to_string())
                    .as_deref()
                    != Some(accepted.blake3.as_str())
            {
                return Err(format!(
                    "accepted {name} is changed, denied or unqualified; use native cargo"
                ));
            }
            let dir = accepted
                .path
                .parent()
                .filter(|bin| bin.file_name().is_some_and(|part| part == "bin"))
                .and_then(Path::parent)
                .ok_or_else(|| {
                    "unsupported configured Rust executable layout; use native cargo".to_owned()
                })?
                .to_path_buf();
            if accepted.path.file_name().is_none_or(|file| file != name)
                || provider_dir
                    .as_ref()
                    .is_some_and(|previous| previous != &dir)
            {
                return Err(
                    "accepted cargo/rustc must share one prepared toolchain; use native cargo"
                        .into(),
                );
            }
            provider_dir = Some(dir);
            if name == "rustc" {
                if provider
                    .get("rustc_version")
                    .and_then(|value| value.as_str())
                    != Some(accepted.identity.as_str())
                {
                    return Err(
                        "accepted rustc identity disagrees with its declaration; use native cargo"
                            .into(),
                    );
                }
                compiler = Some(accepted.identity);
            }
        }
    }
    if check_dir
        .as_ref()
        .zip(provider_dir.as_ref())
        .is_some_and(|(check, provider)| check != provider)
    {
        return Err("configured check and provider toolchains disagree; align the launcher or use native cargo".into());
    }
    let dir = provider_dir.or(check_dir).ok_or_else(|| {
        "no accepted configured Rust toolchain; use native cargo or configure the launcher"
            .to_owned()
    })?;
    if !absolute(&dir) {
        return Err("configured Rust toolchain path is invalid; use native cargo".into());
    }
    for name in ["cargo", "rustc"] {
        let path = dir.join("bin").join(name);
        if config.read_denies.iter().any(|deny| deny.matches(&path))
            || !agent_ide_core::lang::environment::readable(&path, &config.read_denies)
            || !fs::metadata(&path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        {
            return Err(format!(
                "configured {name} is missing, denied or not executable; use native cargo"
            ));
        }
    }
    let cargo_home = checks
        .and_then(|section| section.get("cargo_home"))
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .or_else(|| {
            config
                .provider
                .as_ref()?
                .get("cargo_home")?
                .as_str()
                .map(PathBuf::from)
        });
    Ok(Tools {
        dir,
        compiler,
        cargo_home,
    })
}

/// Resolves the shared project environment and enforces its refusal before any owned child.
pub(crate) fn resolve(
    worktree: &Path,
    config: &agent_ide_core::modules::contract::ModuleConfig,
) -> Result<(Tools, ResolvedEnv), String> {
    let compiler = config
        .provider
        .as_ref()
        .and_then(|provider| provider.get("rustc_version"))
        .and_then(|value| value.as_str());
    if let Some(cause) = pin_conflict(worktree, worktree, compiler, &config.read_denies) {
        return Err(cause);
    }
    let tools = tools(config)?;
    let mut resolved = environment(
        worktree,
        Some(&ConfiguredToolchain {
            dir: &tools.dir,
            version: tools.compiler.as_deref(),
        }),
        &config.read_denies,
    );
    resolved.identity.push_str(
        &blake3::hash(serde_json::to_string(config).unwrap_or_default().as_bytes())
            .to_hex()
            .to_string(),
    );
    if let Some(cause) = &resolved.missing_next_step {
        return Err(cause.clone());
    }
    Ok((tools, resolved))
}

/// Caller-accepted compiler facts, never an ambient tool selection.
pub struct ConfiguredToolchain<'a> {
    /// Actual admitted toolchain directory; its name conveys no version evidence.
    pub dir: &'a Path,
    /// Accepted rustc identity or exact measured stable release; absent means unknown.
    pub version: Option<&'a str>,
}

/// One successfully read authoritative pin.
struct Pin {
    /// Effective file name, with TOML taking precedence over the legacy file.
    file: &'static str,
    /// Declared channel; unsupported aliases remain explicit refusals.
    channel: String,
}

/// Whether the boundary has a Cargo manifest; no parent search occurs.
pub fn is_cargo_project(worktree: &Path) -> bool {
    worktree.join("Cargo.toml").is_file()
}

/// Whether native facts can be passed to the legacy detector without reading excluded inputs.
/// Discovery/pin uncertainty is not converted into a guessed project declaration.
pub(crate) fn detectable(worktree: &Path, denies: &[ReadDeny]) -> bool {
    native_inputs(worktree, denies).is_ok() && pin_read(worktree, denies).is_ok()
}

/// Reads a quoted unescaped value. Complex TOML strings are explicitly unsupported.
fn quoted(value: &str) -> Option<&str> {
    let first = value.chars().next()?;
    if !matches!(first, '"' | '\'') {
        return None;
    }
    let value = value.strip_prefix(first)?.strip_suffix(first)?;
    (!value.contains(['\\', '\n', '\r', first])).then_some(value)
}

/// Accepts bounded printable labels; compatibility is checked separately.
fn valid_channel(channel: &str) -> Result<String, &'static str> {
    if channel.is_empty()
        || channel.len() > 128
        || !channel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
    {
        return Err("invalid or unsupported toolchain channel");
    }
    Ok(channel.to_owned())
}

/// Parses a plain legacy channel or one toolchain table with a channel and optional known
/// profile. Components/targets/path/complex TOML need native qualification and are refused.
fn parse_pin(text: &str, toml: bool) -> Result<String, &'static str> {
    let lines: Vec<_> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .collect();
    if !toml && lines.first().is_some_and(|line| !line.starts_with('[')) {
        if let [channel] = lines.as_slice() {
            return valid_channel(channel);
        }
        return Err("invalid legacy toolchain pin");
    }
    let mut table = false;
    let mut channel = None;
    for line in lines {
        if line == "[toolchain]" {
            if table {
                return Err("duplicate toolchain table");
            }
            table = true;
            continue;
        }
        if !table || line.starts_with('[') {
            return Err("unsupported toolchain pin table");
        }
        let (key, value) = line.split_once('=').ok_or("invalid toolchain pin")?;
        match key.trim() {
            "channel" => {
                if channel.is_some() {
                    return Err("duplicate toolchain channel");
                }
                channel = Some(valid_channel(
                    quoted(value.trim()).ok_or("invalid toolchain channel")?,
                )?);
            }
            "profile"
                if matches!(
                    quoted(value.trim()),
                    Some("minimal" | "default" | "complete")
                ) => {}
            _ => return Err("unsupported toolchain pin requirements"),
        }
    }
    channel.ok_or("toolchain pin has no verifiable channel")
}

/// Reads the first authoritative pin. Denied/invalid/unreadable TOML never falls through;
/// excluded channel content is neither read nor echoed. Symlinked pins are unsupported.
fn pin_read(worktree: &Path, denies: &[ReadDeny]) -> Result<Option<Pin>, String> {
    for file in ["rust-toolchain.toml", "rust-toolchain"] {
        let path = worktree.join(file);
        if denies.iter().any(|deny| deny.matches(&path)) {
            return Err(format!(
                "{file} is excluded from reads; the pin cannot be verified; use native cargo"
            ));
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(metadata) if metadata.is_file() => {}
            _ => {
                return Err(format!(
                    "{file} is unreadable or symlinked; use native cargo"
                ));
            }
        }
        let text = read_input(&path, denies).ok_or_else(|| {
            format!("{file} cannot be read as bounded UTF-8 (64 KiB); use native cargo")
        })?;
        let channel = parse_pin(&text, file.ends_with(".toml"))
            .map_err(|reason| format!("{file}: {reason}; use native cargo"))?;
        return Ok(Some(Pin { file, channel }));
    }
    Ok(None)
}

/// Extracts an exact stable compiler release. Cargo identities, substrings, extra numeric
/// components, leading zeroes and prereleases do not prove a stable rustc release.
fn compiler_release(identity: &str) -> Option<&str> {
    let release = identity.strip_prefix("rustc ").map_or(identity, |value| {
        value.split_whitespace().next().unwrap_or_default()
    });
    let parts: Vec<_> = release.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return None;
    }
    Some(release)
}

/// The same pre-child pin refusal used by all consumers. Only exact stable release pins are
/// supported. Unknown versions and unsupported aliases/date pins explicitly need native cargo.
pub fn pin_conflict(
    worktree: &Path,
    _configured_dir: &Path,
    configured_version: Option<&str>,
    denies: &[ReadDeny],
) -> Option<String> {
    let pin = match pin_read(worktree, denies) {
        Ok(None) => return None,
        Ok(Some(pin)) => pin,
        Err(reason) => return Some(reason),
    };
    if compiler_release(&pin.channel) != Some(pin.channel.as_str()) {
        return Some(format!(
            "{} pins {}; this channel is not qualified for IDE execution; use native cargo",
            pin.file, pin.channel
        ));
    }
    match configured_version.and_then(compiler_release) {
        Some(version) if version == pin.channel => None,
        Some(version) => Some(format!(
            "{} pins {}, but the accepted rustc is {version}; align the launcher or use native cargo",
            pin.file, pin.channel
        )),
        None => Some(format!(
            "{} pins {}, but accepted rustc version is unknown or unqualified; use native cargo",
            pin.file, pin.channel
        )),
    }
}

/// Link and target stamps detect same-path recreation, including a recreated target behind
/// an unchanged link. Call only on admitted readable paths.
fn stamp(path: &Path) -> String {
    let object = |metadata: fs::Metadata| {
        format!(
            "{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec()
        )
    };
    format!(
        "{:?}:{:?}",
        fs::symlink_metadata(path).map(object).ok(),
        fs::metadata(path).map(object).ok()
    )
}

/// Generated/dependency state excluded by project discovery. Declared workspace members in
/// these directories are explicitly unsupported rather than silently qualified.
const SKIPPED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
];

/// Validates the supported workspace member path syntax before filesystem discovery. This is
/// not a Cargo parser: escaped/complex declarations and foreign/generated roots refuse.
fn member_scope(text: &str) -> Result<(), String> {
    let mut workspace = false;
    let mut members = false;
    let mut value = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            workspace = line == "[workspace]";
        }
        if workspace && !members {
            if let Some((key, rhs)) = line.split_once('=') {
                if key.trim().trim_matches(['"', '\'']) == "members" {
                    members = true;
                    value.push_str(rhs);
                }
            }
        } else if members {
            value.push_str(line);
        }
        if members && value.contains(']') {
            break;
        }
    }
    if !members {
        return Ok(());
    }
    let array = value
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| "unsupported workspace member declaration; use native cargo".to_owned())?;
    for item in array
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let path = quoted(item)
            .ok_or_else(|| "unsupported escaped workspace member; use native cargo".to_owned())?;
        if Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || path.split('/').any(|part| SKIPPED.contains(&part))
        {
            return Err(
                "workspace members outside supported worktree input scope; use native cargo"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// Discovers all native manifests/configs inside the worktree, including crates/alpha and
/// deeper members. No symlink is followed. Exhausted entry/depth bounds explicitly refuse;
// no partial identity is certified as complete. Standard generated roots stay excluded.
fn native_inputs(worktree: &Path, denies: &[ReadDeny]) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![(worktree.to_path_buf(), 0_usize)];
    let mut inputs = Vec::new();
    let mut budget = 16_384_usize;
    while let Some((dir, depth)) = pending.pop() {
        let manifest = dir.join("Cargo.toml");
        if denies.iter().any(|deny| deny.matches(&manifest)) {
            return Err("Cargo manifest is read restricted; use native cargo".into());
        }
        if dir == worktree || manifest.is_file() {
            if manifest.is_file() {
                let text = read_input(&manifest, denies).ok_or_else(|| "Cargo manifest is unreadable/oversized for environment qualification; use native cargo".to_owned())?;
                member_scope(&text)?;
            }
            for name in [
                "Cargo.toml",
                "Cargo.lock",
                "rust-toolchain.toml",
                "rust-toolchain",
                "rust-project.json",
                "rustfmt.toml",
                ".rustfmt.toml",
            ] {
                inputs.push(dir.join(name));
            }
        }
        inputs.push(dir.join(".cargo/config.toml"));
        inputs.push(dir.join(".cargo/config"));
        inputs.push(dir.join("rust-toolchain.toml"));
        inputs.push(dir.join("rust-toolchain"));
        for entry in fs::read_dir(&dir)
            .map_err(|_| "native input directory is unreadable; use native cargo".to_owned())?
        {
            if budget == 0 {
                return Err(
                    "native input discovery exceeds its supported bound; use native cargo".into(),
                );
            }
            budget -= 1;
            let entry = entry.map_err(|_| {
                "native input directory changed; retry or use native cargo".to_owned()
            })?;
            if SKIPPED.iter().any(|skip| entry.file_name() == *skip) {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|_| "native input metadata unavailable; use native cargo".to_owned())?;
            if kind.is_symlink() {
                return Err(
                    "symlinked project input scope is unsupported; use native cargo".into(),
                );
            }
            if kind.is_dir() {
                if depth == 12 {
                    return Err(
                        "native input discovery exceeds its supported depth; use native cargo"
                            .into(),
                    );
                }
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

/// Full-byte native identity plus accepted tool facts. Refused discovery gets a stable refused
/// identity; no partial hash is called complete. Read-denied bytes never influence the digest.
pub fn identity(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> String {
    let mut hasher = blake3::Hasher::new();
    if let Some(facts) = configured {
        hasher.update(facts.dir.as_os_str().as_encoded_bytes());
        hasher.update(facts.version.unwrap_or_default().as_bytes());
        for path in [
            facts.dir.to_path_buf(),
            facts.dir.join("bin/cargo"),
            facts.dir.join("bin/rustc"),
            facts.dir.join("bin/rustfmt"),
        ] {
            if !denies.iter().any(|deny| deny.matches(&path)) {
                hasher.update(stamp(&path).as_bytes());
                if let Some(hash) = hash_input(&path, denies) {
                    hasher.update(hash.as_bytes());
                }
            }
        }
    }
    match native_inputs(worktree, denies) {
        Err(reason) => {
            hasher.update(reason.as_bytes());
        }
        Ok(inputs) => {
            for path in inputs {
                hasher.update(path.as_os_str().as_encoded_bytes());
                if denies.iter().any(|deny| deny.matches(&path)) {
                    hasher.update(b"denied");
                } else if let Some(hash) = hash_input(&path, denies) {
                    hasher.update(hash.as_bytes());
                    hasher.update(stamp(&path).as_bytes());
                } else {
                    hasher.update(b"absent-or-unreadable");
                }
            }
        }
    }
    hasher.finalize().to_string()
}

/// One resolution from caller-accepted facts. A pin or discovery refusal yields no chosen or
/// source candidate and the same actionable pre-run cause. Successful candidates include
/// their winner; the directory name is only a label, never compiler version evidence.
pub fn environment(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> ResolvedEnv {
    let nested_refusal = native_inputs(worktree, denies)
        .and_then(|inputs| {
            for path in inputs.iter().filter(|path| {
                path.file_name()
                    .is_some_and(|name| name == "rust-toolchain.toml")
            }) {
                let root = path.parent().expect("discovered input has its directory");
                if root != worktree
                    && let Some(cause) = pin_conflict(
                        root,
                        configured.map_or(worktree, |facts| facts.dir),
                        configured.and_then(|facts| facts.version),
                        denies,
                    )
                {
                    return Err(format!(
                        "{}: {cause}",
                        root.strip_prefix(worktree).unwrap_or(root).display()
                    ));
                }
            }
            Ok(())
        })
        .err();
    let refusal = configured.and_then(|facts| pin_conflict(worktree, facts.dir, facts.version, denies))
        .or_else(|| pin_read(worktree, denies).err())
        .or(nested_refusal)
        .or_else(|| configured.is_none().then(|| "no accepted configured Rust toolchain; use native cargo or configure the launcher".to_owned()));
    let chosen = if refusal.is_none() {
        configured.map(|facts| EnvCandidate {
            label: facts.dir.display().to_string(),
            path: facts.dir.to_path_buf(),
            version: facts.version.and_then(compiler_release).map(str::to_owned),
            broken: false,
        })
    } else {
        None
    };
    let source = chosen.as_ref().map(|_| match pin_read(worktree, denies) {
        Ok(Some(pin)) => EnvSource::Pin(pin.file.into()),
        _ => EnvSource::Launcher,
    });
    let candidates = chosen.clone().into_iter().collect();
    ResolvedEnv {
        root: PathBuf::new(),
        chosen,
        source,
        candidates,
        warnings: Vec::new(),
        missing_next_step: refusal,
        identity: identity(worktree, configured, denies),
    }
}

/// Guarded input, exact compiler evidence and native-scope regression fixtures.
#[cfg(test)]
mod tests {
    use super::*;
    /// Owns one scratch fixture under the executor's prepared TMPDIR.
    struct ScratchDir(PathBuf);
    impl ScratchDir {
        /// Creates a unique directory without mutating process environment.
        fn new(label: &str) -> Self {
            /// Separates fixture directories created by one test process.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl std::ops::Deref for ScratchDir {
        type Target = Path;
        /// Borrows the fixture boundary.
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for ScratchDir {
        /// Removes only this fixture's owned files.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Writes one fixture input, creating only its private parent directories.
    fn put(root: &Path, relative: &str, body: impl AsRef<[u8]>) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// Misleading names, unknown/Cargo identities and prerelease/substring shortcuts never
    /// establish a stable compiler pin; a generic dir with actual rustc evidence agrees.
    #[test]
    fn pin_uses_exact_rustc_release() {
        let root = ScratchDir::new("rust-pin-evidence");
        put(&root, "Cargo.toml", "[package]\nname=\"x\"\n");
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel=\"1.98.1\"\n",
        );
        let dir = root.join("1.98.1-misleading");
        fs::create_dir_all(&dir).unwrap();
        for evidence in [
            None,
            Some("rustc 1.97.0"),
            Some("cargo 1.98.1"),
            Some("rustc 1.98.1-nightly"),
            Some("rustc 11.98.1"),
            Some("rustc 1.98.1.2"),
            Some("rustc 01.98.1"),
        ] {
            assert!(
                pin_conflict(&root, &dir, evidence, &[]).is_some(),
                "{evidence:?}"
            );
        }
        let generic = root.join("generic");
        fs::create_dir_all(&generic).unwrap();
        assert!(pin_conflict(&root, &generic, Some("rustc 1.98.1 (accepted)"), &[]).is_none());
        let facts = ConfiguredToolchain {
            dir: &generic,
            version: Some("rustc 1.98.1"),
        };
        let resolved = environment(&root, Some(&facts), &[]);
        assert_eq!(
            resolved.source,
            Some(EnvSource::Pin("rust-toolchain.toml".into()))
        );
        assert_eq!(resolved.chosen.unwrap().version.as_deref(), Some("1.98.1"));
    }

    /// Denied, invalid, oversized and unknown TOML never fall through to valid legacy pins.
    /// Changing excluded channel bytes changes neither the refusal nor identity.
    #[test]
    fn authoritative_pin_failures_never_fall_through() {
        let root = ScratchDir::new("rust-pin-authority");
        put(&root, "Cargo.toml", "");
        put(&root, "rust-toolchain", "1.98.1\n");
        let dir = root.join("tools");
        fs::create_dir_all(&dir).unwrap();
        let facts = ConfiguredToolchain {
            dir: &dir,
            version: Some("rustc 1.98.1"),
        };
        for body in [
            "not toml".to_owned(),
            "[toolchain]\nprofile=\"minimal\"\n".into(),
            "x".repeat(65 * 1024),
            "[toolchain]\nchannel=\"nightly\"\n".into(),
        ] {
            put(&root, "rust-toolchain.toml", body);
            assert!(environment(&root, Some(&facts), &[]).chosen.is_none());
        }
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel=\"private-channel-a\"\n",
        );
        let denies = [ReadDeny::Path(root.join("rust-toolchain.toml"))];
        let before = environment(&root, Some(&facts), &denies);
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel=\"private-channel-b\"\n",
        );
        let after = environment(&root, Some(&facts), &denies);
        assert_eq!(before.identity, after.identity);
        assert_eq!(before.missing_next_step, after.missing_next_step);
        assert!(
            !before
                .missing_next_step
                .unwrap()
                .contains("private-channel")
        );
        assert!(after.chosen.is_none() && after.source.is_none());
    }

    /// Real nested member manifests/configs and a same-length changed lock tail invalidate
    /// inside the boundary; foreign parent locks never enter the identity.
    #[test]
    fn native_identity_covers_nested_members_and_full_bytes() {
        let outer = ScratchDir::new("rust-native-inputs");
        let root = outer.join("workspace");
        put(&root, "Cargo.toml", "[workspace]\nmembers=[\"crates/*\"]\n");
        put(
            &root,
            "crates/alpha/Cargo.toml",
            "[package]\nname=\"alpha\"\n",
        );
        put(
            &root,
            "crates/alpha/.cargo/config.toml",
            "[build]\njobs=1\n",
        );
        let before = identity(&root, None, &[]);
        put(
            &root,
            "crates/alpha/Cargo.toml",
            "[package]\nname=\"beta\"\n",
        );
        assert_ne!(identity(&root, None, &[]), before);
        let before = identity(&root, None, &[]);
        put(
            &root,
            "crates/alpha/.cargo/config.toml",
            "[build]\njobs=2\n",
        );
        assert_ne!(identity(&root, None, &[]), before);
        let body = vec![b'x'; 256 * 1024];
        put(&root, "Cargo.lock", &body);
        let before = identity(&root, None, &[]);
        let mut changed = body;
        *changed.last_mut().unwrap() = b'y';
        put(&root, "Cargo.lock", changed);
        assert_ne!(identity(&root, None, &[]), before);
        let before = identity(&root, None, &[]);
        put(&outer, "Cargo.lock", "foreign");
        assert_eq!(identity(&root, None, &[]), before);
    }
}
