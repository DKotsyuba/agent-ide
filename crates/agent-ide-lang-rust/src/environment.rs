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
                    .is_none_or(|hash| !hash.to_hex().eq_ignore_ascii_case(&accepted.blake3))
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
    resolve_at(worktree, worktree, config)
}

/// Resolves the effective project selected by the actual child cwd (or manifest selection).
/// The admitted worktree remains the boundary; no foreign ancestor contributes native inputs.
pub(crate) fn resolve_at(
    worktree: &Path,
    cwd: &Path,
    config: &agent_ide_core::modules::contract::ModuleConfig,
) -> Result<(Tools, ResolvedEnv), String> {
    let active = project_root(worktree, cwd, &config.read_denies)?;
    let pin = pin_root(worktree, &active, &config.read_denies)?;
    let compiler = config
        .provider
        .as_ref()
        .and_then(|provider| provider.get("rustc_version"))
        .and_then(|value| value.as_str());
    if let Some(cause) = pin_conflict(&pin, worktree, compiler, &config.read_denies) {
        return Err(cause);
    }
    let tools = tools(config)?;
    let mut resolved = environment_at(
        worktree,
        &active,
        Some(&ConfiguredToolchain {
            dir: &tools.dir,
            version: tools.compiler.as_deref(),
        }),
        &config.read_denies,
    );
    resolved.identity.push_str(
        blake3::hash(serde_json::to_string(config).unwrap_or_default().as_bytes())
            .to_hex()
            .as_str(),
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

/// Components a pin may list: optional rustup tools that never change how a package builds or
/// tests. Every tool the IDE itself runs (`rustfmt`, `cargo clippy`) is resolved and checked
/// where it is used, so listing one here adds no unverified requirement.
const KNOWN_COMPONENTS: [&str; 8] = [
    "rustfmt",
    "clippy",
    "rust-src",
    "rust-analyzer",
    "rust-docs",
    "llvm-tools",
    "llvm-tools-preview",
    "rustc-dev",
];

/// Whether `value` is a one-line array of quoted [`KNOWN_COMPONENTS`] (`["rustfmt", "clippy"]`).
fn known_components(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    inner
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .all(|item| quoted(item).is_some_and(|name| KNOWN_COMPONENTS.contains(&name)))
}

/// Parses a plain legacy channel or one toolchain table with a channel, an optional known
/// profile and an optional list of known components. Targets/path/unknown components and
/// complex TOML need native qualification and are refused.
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
            "components" if known_components(value.trim()) => {}
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

/// Resolves a native input under the admitted worktree before opening it. Canonicalization
/// observes path metadata only; an external link or excluded lexical/resolved target refuses
/// before contents can influence parsing or hashing. Missing in-bound inputs remain optional.
fn input_path(worktree: &Path, path: &Path, denies: &[ReadDeny]) -> Result<PathBuf, String> {
    if denies.iter().any(|deny| deny.matches(path)) {
        return Err("native input is read restricted; use native cargo".into());
    }
    let relative = path
        .strip_prefix(worktree)
        .map_err(|_| "native input is outside the worktree; use native cargo".to_owned())?;
    let mut prefix = worktree.to_path_buf();
    for component in relative.components() {
        match component {
            Component::ParentDir if prefix != worktree => {
                prefix.pop();
            }
            Component::Normal(name) => prefix.push(name),
            Component::CurDir => continue,
            _ => return Err("native input escapes the admitted worktree; use native cargo".into()),
        }
        if denies.iter().any(|deny| deny.matches(&prefix)) {
            return Err("native input is read restricted; use native cargo".into());
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("symlinked native input is unsupported; use native cargo".into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("native input path is unreadable; use native cargo".into()),
            _ => {}
        }
    }
    let roots = [worktree.to_path_buf()];
    let resolved = agent_ide_core::assistance::launcher::admit_path(&roots, &prefix)
        .map_err(|_| "native input escapes the admitted worktree; use native cargo".to_owned())?;
    if denies.iter().any(|deny| deny.matches(&resolved)) {
        return Err("native input target is read restricted; use native cargo".into());
    }
    Ok(resolved)
}

/// Reads one bounded manifest only after containing its actual path. A missing manifest is
/// absent; unreadable, denied, invalid UTF-8 and oversized authoritative input are refusals.
pub(crate) fn manifest_text(
    worktree: &Path,
    root: &Path,
    denies: &[ReadDeny],
) -> Result<Option<String>, String> {
    let path = input_path(worktree, &root.join("Cargo.toml"), denies)?;
    match fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(metadata) if metadata.is_file() => read_input(&path, denies)
            .map(Some)
            .ok_or_else(|| "Cargo manifest is unreadable/oversized; use native cargo".into()),
        _ => Err("Cargo manifest is not a readable regular file; use native cargo".into()),
    }
}

/// The existing narrow workspace-array syntax: literal quoted paths, multiline arrays and
/// whole-component '*' expansion. Unknown/escaped/foreign syntax explicitly needs native Cargo.
fn workspace_paths(text: &str, key: &str) -> Result<Vec<String>, String> {
    let mut workspace = false;
    let mut collecting = false;
    let mut value = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if !collecting && line.starts_with('[') {
            workspace = line == "[workspace]";
        }
        if collecting {
            value.push_str(line);
        } else if workspace
            && let Some((name, rhs)) = line.split_once('=')
            && name.trim() == key
        {
            collecting = true;
            value.push_str(rhs.trim());
        }
        if collecting && value.contains(']') {
            break;
        }
    }
    if !collecting {
        return Ok(Vec::new());
    }
    let values = value
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| format!("unsupported workspace {key}; use native cargo"))?;
    values
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let value = quoted(item)
                .ok_or_else(|| format!("unsupported workspace {key} string; use native cargo"))?;
            if Path::new(value).is_absolute()
                || Path::new(value)
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
                || value.split('/').any(|part| {
                    part.contains(['?', '[', ']', '{', '}']) || (part.contains('*') && part != "*")
                })
            {
                return Err(format!(
                    "unsupported workspace {key} path; use native cargo"
                ));
            }
            Ok(value.to_owned())
        })
        .collect()
}

/// Matches the supported workspace path syntax, without treating '*' as a recursive glob.
fn member_matches(pattern: &str, path: &Path) -> bool {
    let pattern: Vec<_> = Path::new(pattern).components().collect();
    let path: Vec<_> = path.components().collect();
    pattern.len() == path.len()
        && pattern.iter().zip(path).all(|(pattern, value)| {
            *pattern == Component::Normal(std::ffi::OsStr::new("*")) || *pattern == value
        })
}

/// Whether an independently discovered semantic project is explicitly excluded by the root
/// workspace. This shares guarded input/supported pattern semantics with native resolution.
pub(crate) fn excluded_project(root: &Path, project: &Path) -> Result<bool, String> {
    let text = manifest_text(root, root, &[])?.unwrap_or_default();
    let relative = project
        .strip_prefix(root)
        .map_err(|_| "project outside worktree".to_owned())?;
    Ok(workspace_paths(&text, "exclude")?
        .iter()
        .any(|pattern| member_matches(pattern, relative)))
}

/// Expands only declared workspace member path segments, never a repository-wide tree walk.
/// Candidate/member bounds apply to relevant directories, not unrelated source/assets files.
fn member_roots(
    worktree: &Path,
    patterns: &[String],
    excluded: &[String],
    denies: &[ReadDeny],
) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for pattern in patterns {
        let mut candidates = vec![worktree.to_path_buf()];
        for component in Path::new(pattern).components() {
            let mut next = Vec::new();
            for parent in candidates {
                if component == Component::Normal(std::ffi::OsStr::new("*")) {
                    let parent = input_path(worktree, &parent, denies)?;
                    for entry in fs::read_dir(parent).map_err(|_| {
                        "workspace member directory unavailable; use native cargo".to_owned()
                    })? {
                        let entry = entry
                            .map_err(|_| "workspace member changed; use native cargo".to_owned())?;
                        let relative = entry
                            .path()
                            .strip_prefix(worktree)
                            .map(Path::to_path_buf)
                            .map_err(|_| "workspace member escaped".to_owned())?;
                        if excluded
                            .iter()
                            .any(|exclude| member_matches(exclude, &relative))
                        {
                            continue;
                        }
                        let kind = entry
                            .file_type()
                            .map_err(|_| "workspace member metadata unavailable".to_owned())?;
                        if kind.is_dir() || kind.is_symlink() {
                            let path = input_path(worktree, &entry.path(), denies)?;
                            if path.is_dir() {
                                next.push(path);
                            }
                        }
                    }
                } else {
                    next.push(parent.join(component));
                }
                if next.len() > 1024 {
                    return Err(
                        "workspace member expansion exceeds 1024 directories; use native cargo"
                            .into(),
                    );
                }
            }
            candidates = next;
        }
        for root in candidates {
            let relative = root
                .strip_prefix(worktree)
                .map_err(|_| "workspace member escaped".to_owned())?;
            if excluded
                .iter()
                .any(|exclude| member_matches(exclude, relative))
            {
                continue;
            }
            let root = input_path(worktree, &root, denies)?;
            if manifest_text(worktree, &root, denies)?.is_none() {
                return Err(
                    "declared workspace member manifest is missing; use native cargo".into(),
                );
            }
            roots.push(root);
        }
        if roots.len() > 1024 {
            return Err("workspace has more than 1024 supported members; use native cargo".into());
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// The nearest effective manifest at/below the current directory, never a foreign ancestor.
/// File demands pass their parent directory; explicit manifests pass their containing directory.
pub(crate) fn project_root(
    worktree: &Path,
    cwd: &Path,
    denies: &[ReadDeny],
) -> Result<PathBuf, String> {
    let absolute = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        worktree.join(cwd)
    };
    let absolute = input_path(worktree, &absolute, denies)?;
    for dir in absolute
        .ancestors()
        .take_while(|dir| dir.starts_with(worktree))
    {
        if manifest_text(worktree, dir, denies)?.is_some() {
            return Ok(dir.to_path_buf());
        }
    }
    Ok(worktree.to_path_buf())
}

/// The cwd selected by Cargo's explicit manifest option, or the supplied actual cwd. This
/// interprets only project selection, never executable/install authority or arbitrary flags.
pub(crate) fn command_directory(cwd: &Path, argv: &[String]) -> Result<PathBuf, String> {
    let cargo = argv
        .first()
        .and_then(|program| Path::new(program).file_name())
        .is_some_and(|name| name == "cargo");
    if !cargo {
        return Ok(cwd.to_path_buf());
    }
    let mut selected = None;
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg.starts_with('+') {
            return Err("per-command Rust toolchain selectors are unsupported; configure the launcher or use native cargo".into());
        }
        let path = if arg == "--manifest-path" {
            Some(
                args.next()
                    .ok_or_else(|| {
                        "Cargo manifest option has no path; use native cargo".to_owned()
                    })?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--manifest-path=")
        };
        if let Some(path) = path {
            if selected.is_some() {
                return Err(
                    "multiple Cargo manifest selectors are unsupported; use native cargo".into(),
                );
            }
            let path = Path::new(path);
            if path.file_name().is_none_or(|name| name != "Cargo.toml") {
                return Err("unsupported Cargo manifest path; use native cargo".into());
            }
            selected = Some(if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            });
        }
    }
    Ok(selected
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.to_path_buf()))
}

/// Literal local dependency/patch paths in the supported line-oriented manifest subset. Reuses
/// the existing TOML string reader; escaped/multiline path forms are explicitly unsupported.
fn dependency_roots(
    worktree: &Path,
    package: &Path,
    text: &str,
    denies: &[ReadDeny],
) -> Result<Vec<PathBuf>, String> {
    let mut dependency = false;
    let mut package_table = false;
    let mut roots = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            package_table = line == "[package]";
            dependency =
                line.contains("dependencies") || line.starts_with("[patch.") || line == "[replace]";
        }
        if package_table
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().trim_matches(['"', '\'']) == "workspace")
        {
            return Err(
                "explicit package.workspace relocation is unsupported; use native cargo".into(),
            );
        }
        if !dependency {
            continue;
        }
        for piece in line.split(['{', ',']) {
            let Some((key, value)) = piece.split_once('=') else {
                continue;
            };
            if key.contains('\\') {
                return Err("escaped dependency keys are unsupported; use native cargo".into());
            }
            if key.trim().trim_matches(['"', '\'']) != "path" {
                continue;
            }
            let value = value.trim().trim_end_matches('}').trim();
            if quoted(value).is_none() {
                return Err("unsupported local dependency path syntax; use native cargo".into());
            }
            let line = format!("path = {value}");
            let path = crate::support::toml_string(&line, "path").ok_or_else(|| {
                "unsupported local dependency path string; use native cargo".to_owned()
            })?;
            let root = input_path(worktree, &package.join(path), denies)?;
            if manifest_text(worktree, &root, denies)?.is_none() {
                return Err(
                    "declared local dependency manifest is missing; use native cargo".into(),
                );
            }
            roots.push(root);
        }
    }
    Ok(roots)
}

/// Completes a bounded declared project set through literal local dependencies only. This
/// accounts for Cargo's implicit path-dependency members without scanning unrelated folders.
fn project_closure(
    worktree: &Path,
    mut pending: Vec<PathBuf>,
    denies: &[ReadDeny],
) -> Result<Vec<PathBuf>, String> {
    let mut seen = std::collections::BTreeSet::new();
    while let Some(root) = pending.pop() {
        if !seen.insert(root.clone()) {
            continue;
        }
        if seen.len() > 1024 {
            return Err("more than 1024 relevant Cargo projects; use native cargo".into());
        }
        if let Some(text) = manifest_text(worktree, &root, denies)? {
            pending.extend(dependency_roots(worktree, &root, &text, denies)?);
        }
    }
    Ok(seen.into_iter().collect())
}

/// Native inputs for the effective root/workspace and declared members. Excluded/unrelated
/// nested projects and assets do not affect root qualification; a selected independent project
/// is resolved separately. Unsupported member syntax refuses instead of certifying a partial set.
fn native_inputs_at(
    worktree: &Path,
    active: &Path,
    denies: &[ReadDeny],
) -> Result<Vec<PathBuf>, String> {
    let mut roots = vec![active.to_path_buf()];
    // A member's Cargo.lock and workspace configuration belong to its enclosing declared
    // workspace. An excluded independent project is not folded into that workspace.
    for ancestor in active
        .ancestors()
        .take_while(|dir| dir.starts_with(worktree))
    {
        let Some(text) = manifest_text(worktree, ancestor, denies)? else {
            continue;
        };
        if !text
            .lines()
            .any(|line| line.split('#').next().unwrap_or_default().trim() == "[workspace]")
        {
            continue;
        }
        let excluded = workspace_paths(&text, "exclude")?;
        let relative = active
            .strip_prefix(ancestor)
            .expect("ancestor contains project");
        if ancestor != active
            && excluded
                .iter()
                .any(|pattern| member_matches(pattern, relative))
        {
            continue;
        }
        let members = member_roots(
            ancestor,
            &workspace_paths(&text, "members")?,
            &excluded,
            denies,
        )?;
        let declared = std::iter::once(ancestor.to_path_buf())
            .chain(members)
            .collect::<Vec<_>>();
        if ancestor == active {
            roots = declared;
            break;
        }
        let declared = project_closure(worktree, declared, denies)?;
        if declared.iter().any(|member| member == active) {
            roots = declared;
            break;
        }
    }
    let mut inputs = Vec::new();
    for root in project_closure(worktree, roots, denies)? {
        for name in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "rust-toolchain",
            "rust-project.json",
            "rustfmt.toml",
            ".rustfmt.toml",
        ] {
            inputs.push(input_path(worktree, &root.join(name), denies)?);
        }
        for ancestor in root.ancestors().take_while(|dir| dir.starts_with(worktree)) {
            for name in [
                ".cargo/config.toml",
                ".cargo/config",
                "rust-toolchain.toml",
                "rust-toolchain",
            ] {
                inputs.push(input_path(worktree, &ancestor.join(name), denies)?);
            }
        }
    }
    if active != worktree {
        inputs.push(input_path(worktree, &worktree.join("Cargo.toml"), denies)?);
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

/// Root-context projection retained for card/check consumers.
fn native_inputs(worktree: &Path, denies: &[ReadDeny]) -> Result<Vec<PathBuf>, String> {
    native_inputs_at(worktree, worktree, denies)
}

/// Whether the file's nearest package belongs to the root check's supported Cargo closure.
/// Unsupported/blocked discovery is unknown, never evidence that a root check compiled it.
/// This reuses native input discovery, including explicit members and local dependencies.
pub(crate) fn checked_package(worktree: &Path, file: &Path) -> Result<bool, String> {
    let file = input_path(worktree, &worktree.join(file), &[])?;
    let package = project_root(worktree, file.parent().unwrap_or(worktree), &[])?;
    if manifest_text(worktree, &package, &[])?.is_none() {
        return Ok(false);
    }
    Ok(native_inputs(worktree, &[])?.contains(&package.join("Cargo.toml")))
}

/// The nearest authoritative cwd pin inside the admitted boundary. Other members' pins do not
/// govern a root command; a separately selected project resolves its own pin first.
fn pin_root(worktree: &Path, active: &Path, denies: &[ReadDeny]) -> Result<PathBuf, String> {
    for root in active
        .ancestors()
        .take_while(|root| root.starts_with(worktree))
    {
        for file in ["rust-toolchain.toml", "rust-toolchain"] {
            let path = input_path(worktree, &root.join(file), denies)?;
            if fs::symlink_metadata(path).is_ok() {
                return Ok(root.to_path_buf());
            }
        }
    }
    Ok(worktree.to_path_buf())
}

/// Full-byte native identity plus accepted tool facts. Refused discovery gets a stable refused
/// identity; no partial hash is called complete. Read-denied bytes never influence the digest.
pub fn identity(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> String {
    identity_at(worktree, worktree, configured, denies)
}

/// Complete relevant native/tool identity, partitioned by the active project root.
fn identity_at(
    worktree: &Path,
    active: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(active.as_os_str().as_encoded_bytes());
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
    match native_inputs_at(worktree, active, denies) {
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

/// Root-context resolution retained for card/check callers. Only the effective root pin
/// governs a child; unrelated or excluded subprojects cannot veto the command.
pub fn environment(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> ResolvedEnv {
    environment_at(worktree, worktree, configured, denies)
}

/// Resolves one actual project without certifying another project's constraints. A native scope
/// or authoritative-pin refusal has no chosen candidate and names native recovery.
fn environment_at(
    worktree: &Path,
    active: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> ResolvedEnv {
    let pin = pin_root(worktree, active, denies);
    let refusal = pin.as_ref().err().cloned()
        .or_else(|| pin.as_ref().ok().and_then(|root| configured.and_then(|facts| pin_conflict(root, facts.dir, facts.version, denies))))
        .or_else(|| pin.as_ref().ok().and_then(|root| pin_read(root, denies).err()))
        .or_else(|| native_inputs_at(worktree, active, denies).err())
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
    let source = chosen.as_ref().map(|_| {
        match pin
            .as_ref()
            .ok()
            .and_then(|root| pin_read(root, denies).ok().flatten().map(|pin| (root, pin)))
        {
            Some((root, pin)) => EnvSource::Pin(
                root.join(pin.file)
                    .strip_prefix(worktree)
                    .unwrap_or(Path::new(pin.file))
                    .display()
                    .to_string(),
            ),
            None => EnvSource::Launcher,
        }
    });
    let candidates = chosen.clone().into_iter().collect();
    ResolvedEnv {
        root: active
            .strip_prefix(worktree)
            .unwrap_or(Path::new(""))
            .to_path_buf(),
        chosen,
        source,
        candidates,
        warnings: Vec::new(),
        missing_next_step: refusal,
        identity: identity_at(worktree, active, configured, denies),
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

    /// Accepted hexadecimal digests have the same case-insensitive semantics as the launcher.
    #[test]
    fn uppercase_accepted_tool_digests_are_equivalent() {
        use std::os::unix::fs::PermissionsExt;
        let root = ScratchDir::new("rust-uppercase-tools");
        let mut provider = serde_json::json!({"rustc_version":"rustc 1.98.1"});
        for name in ["cargo", "rustc"] {
            let path = root.join("bin").join(name);
            put(&root, &format!("bin/{name}"), format!("accepted {name}"));
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            provider[name] = serde_json::json!({
                "path": path,
                "identity": if name == "rustc" { "rustc 1.98.1" } else { "cargo 1.98.1" },
                "blake3": hash_input(&path, &[]).unwrap().to_hex().to_string().to_uppercase(),
            });
        }
        let config = agent_ide_core::modules::contract::ModuleConfig {
            provider: Some(provider),
            ..Default::default()
        };
        assert_eq!(tools(&config).unwrap().dir, root.0);
        put(&root, "bin/cargo", "modified cargo");
        assert!(tools(&config).is_err());
    }

    /// Excluded independent crates cannot borrow root clean results; real path dependencies
    /// remain covered, while unsupported scope stays explicitly not analysed.
    #[test]
    fn check_applicability_uses_the_actual_cargo_closure() {
        use agent_ide_core::checks::LanguageChecks;
        let root = ScratchDir::new("rust-check-scope");
        put(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers=[\"crates/app\"]\nexclude=[\"legacy\"]\n",
        );
        put(
            &root,
            "crates/app/Cargo.toml",
            "[package]\nname=\"app\"\n[dependencies]\nsupport={path=\"../support\"}\n",
        );
        for package in ["crates/support", "legacy"] {
            put(
                &root,
                &format!("{package}/Cargo.toml"),
                "[package]\nname=\"other\"\n",
            );
        }
        for package in ["crates/app", "crates/support", "legacy"] {
            put(
                &root,
                &format!("{package}/src/lib.rs"),
                "pub fn value() {}\n",
            );
        }
        let checks = crate::checks::RustChecks;
        for package in ["crates/app", "crates/support"] {
            assert_eq!(
                checks.not_analysed(&root, Path::new(&format!("{package}/src/lib.rs"))),
                None
            );
        }
        let legacy = Path::new("legacy/src/lib.rs");
        assert!(
            checks
                .not_analysed(&root, legacy)
                .unwrap()
                .contains("outside the root Cargo check scope")
        );
        assert_eq!(
            checks.not_analysed(&root.join("legacy"), Path::new("src/lib.rs")),
            None
        );
        put(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers=[\"crates/**\"]\n",
        );
        assert!(
            checks
                .not_analysed(&root, Path::new("crates/app/src/lib.rs"))
                .is_some()
        );
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

    /// A pin listing known optional components is qualified like a bare channel pin; an unknown
    /// component, a target list or a multi-line array still needs native qualification.
    #[test]
    fn known_components_do_not_block_an_exact_pin() {
        let root = ScratchDir::new("rust-pin-components");
        put(&root, "Cargo.toml", "[package]\nname=\"x\"\n");
        let dir = root.join("tools");
        fs::create_dir_all(&dir).unwrap();
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.98.1\"\ncomponents = [\"rustfmt\", \"clippy\"]\n",
        );
        assert!(pin_conflict(&root, &dir, Some("rustc 1.98.1"), &[]).is_none());
        for extra in [
            "components = [\"rustfmt\", \"custom-tool\"]",
            "targets = [\"wasm32-unknown-unknown\"]",
            "components = [",
        ] {
            put(
                &root,
                "rust-toolchain.toml",
                format!("[toolchain]\nchannel = \"1.98.1\"\n{extra}\n"),
            );
            assert!(
                pin_conflict(&root, &dir, Some("rustc 1.98.1"), &[]).is_some(),
                "{extra}"
            );
        }
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

    /// Only declared workspace members govern root inputs. An excluded independent project
    /// resolves its pin when explicitly selected, while member commands retain workspace locks.
    #[test]
    fn effective_workspace_scope_excludes_unrelated_projects() {
        let base = ScratchDir::new("rust-effective-scope");
        let root = base.join("workspace");
        put(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers=[\"crates/app\"]\nexclude=[\"legacy\"]\n",
        );
        put(&root, "Cargo.lock", "root lock");
        put(&root, "crates/app/Cargo.toml", "[package]\nname=\"app\"\n");
        put(
            &root,
            "legacy/Cargo.toml",
            "[workspace]\n\n[package]\nname=\"legacy\"\n",
        );
        put(
            &root,
            "legacy/rust-toolchain.toml",
            "[toolchain]\nchannel=\"nightly\"\n",
        );
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel=\"1.98.1\"\n",
        );
        let tools = base.join("tools");
        fs::create_dir_all(&tools).unwrap();
        let facts = ConfiguredToolchain {
            dir: &tools,
            version: Some("rustc 1.98.1"),
        };
        let before = environment(&root, Some(&facts), &[]);
        assert!(before.chosen.is_some(), "{before:?}");
        put(
            &root,
            "legacy/rust-toolchain.toml",
            "[toolchain]\nchannel=\"nightly-2026-01-01\"\n",
        );
        assert_eq!(
            environment(&root, Some(&facts), &[]).identity,
            before.identity
        );
        let legacy = project_root(&root, &root.join("legacy"), &[]).unwrap();
        let selected = environment_at(&root, &legacy, Some(&facts), &[]);
        assert!(
            selected.chosen.is_none()
                && selected
                    .missing_next_step
                    .unwrap()
                    .contains("nightly-2026-01-01")
        );
        assert_eq!(
            crate::profile::linked_projects(&root),
            None,
            "excluded independent workspace must not be loaded by the root semantic session"
        );
        let member = project_root(&root, &root.join("crates/app"), &[]).unwrap();
        let before = identity_at(&root, &member, Some(&facts), &[]);
        put(&root, "Cargo.lock", "changed root lock");
        assert_ne!(identity_at(&root, &member, Some(&facts), &[]), before);
        let before = identity_at(&root, &member, Some(&facts), &[]);
        put(&root, "crates/app/Cargo.toml", "[package]\nname=\"app2\"\n");
        assert_ne!(identity_at(&root, &member, Some(&facts), &[]), before);
        put(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers=[\"crates/**\"]\n",
        );
        assert!(
            environment(&root, Some(&facts), &[])
                .missing_next_step
                .unwrap()
                .contains("unsupported workspace")
        );
    }

    /// Relevant Cargo/.cargo links refuse before external contents can influence qualification.
    /// Unrelated asset links and more ordinary files than the former global scan budget are inert.
    #[test]
    fn native_links_are_guarded_and_unrelated_assets_are_inert() {
        let base = ScratchDir::new("rust-input-links");
        let root = base.join("workspace");
        fs::create_dir_all(&root).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        put(&outside, "Cargo.toml", [0xff_u8]);
        std::os::unix::fs::symlink(outside.join("Cargo.toml"), root.join("Cargo.toml")).unwrap();
        let before = environment(&root, None, &[]);
        assert!(
            before
                .missing_next_step
                .as_deref()
                .unwrap()
                .contains("symlinked native input")
        );
        put(&outside, "Cargo.toml", "[package]\nname=\"external\"\n");
        let after = environment(&root, None, &[]);
        assert_eq!(before.identity, after.identity);
        assert_eq!(before.missing_next_step, after.missing_next_step);
        fs::remove_file(root.join("Cargo.toml")).unwrap();
        put(&root, "Cargo.toml", "[package]\nname=\"root\"\n");
        put(&outside, "config.toml", "[build]\njobs=1\n");
        std::os::unix::fs::symlink(&outside, root.join(".cargo")).unwrap();
        let before = environment(&root, None, &[]);
        assert!(
            before
                .missing_next_step
                .as_deref()
                .unwrap()
                .contains("symlinked native input")
        );
        put(&outside, "config.toml", "[build]\njobs=2\n");
        assert_eq!(before.identity, environment(&root, None, &[]).identity);
        fs::remove_file(root.join(".cargo")).unwrap();
        let tools = base.join("tools");
        fs::create_dir_all(&tools).unwrap();
        let facts = ConfiguredToolchain {
            dir: &tools,
            version: Some("rustc 1.98.1"),
        };
        let before = environment(&root, Some(&facts), &[]);
        assert!(before.chosen.is_some());
        std::os::unix::fs::symlink(&outside, root.join("README.md")).unwrap();
        let assets = root.join("assets");
        fs::create_dir_all(&assets).unwrap();
        for index in 0..16_400 {
            fs::write(assets.join(index.to_string()), []).unwrap();
        }
        let after = environment(&root, Some(&facts), &[]);
        assert!(after.chosen.is_some(), "{after:?}");
        assert_eq!(before.identity, after.identity);
    }

    /// Literal local path dependencies are effective inputs, including Cargo's implicit
    /// workspace members. Normal '..' paths remain bounded; foreign dependencies refuse.
    #[test]
    fn local_dependency_inputs_stay_in_the_actual_workspace() {
        let base = ScratchDir::new("rust-path-dependencies");
        let root = base.join("workspace");
        put(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers=[\"crates/app\"]\n",
        );
        put(&root, "Cargo.lock", "workspace lock");
        put(
            &root,
            "crates/app/Cargo.toml",
            "[package]\nname=\"app\"\n[dependencies]\nsupport={path=\"../support\"}\n",
        );
        put(
            &root,
            "crates/support/Cargo.toml",
            "[package]\nname=\"support\"\n",
        );
        let before = identity(&root, None, &[]);
        put(
            &root,
            "crates/support/Cargo.toml",
            "[package]\nname=\"support2\"\n",
        );
        assert_ne!(identity(&root, None, &[]), before);
        let active = project_root(&root, &root.join("crates/support"), &[]).unwrap();
        let before = identity_at(&root, &active, None, &[]);
        put(&root, "Cargo.lock", "changed workspace lock");
        assert_ne!(identity_at(&root, &active, None, &[]), before);
        put(&base, "foreign/Cargo.toml", "[package]\nname=\"foreign\"\n");
        put(
            &root,
            "crates/app/Cargo.toml",
            "[package]\nname=\"app\"\n[dependencies]\nforeign={path=\"../../../foreign\"}\n",
        );
        assert!(native_inputs(&root, &[]).unwrap_err().contains("escapes"));
    }
}
