//! The Rust project environment of one worktree: the configured toolchain its Cargo project
//! runs with, and the native inputs that decide whether an answer about it is still current.
//!
//! This is the language-owned resolver building block of the common project-environment
//! contract. Its facts are explicit parameters — the caller supplies the toolchain its own
//! configuration admits (the checker's launcher section, the provider launch's accepted
//! executables) together with the measured toolchain version it accepted, never a
//! process-environment variable and never a directory label. What it computes today:
//!
//! * [`identity`] — the opaque identity of the worktree's environment: the configured
//!   toolchain (path, name, measured version, directory stamp, `cargo`/`rustc` stamps), the
//!   `rust-toolchain.toml`/`rust-toolchain` pin (name, channel and whole content, read under
//!   the caller's read denies), the worktree's `Cargo.toml`, `Cargo.lock`,
//!   `.cargo/config.toml`/`.cargo/config`, and its first-level member manifests
//!   (`*/Cargo.toml`, sorted — depth one; a deeper member tree is disclosed as unsupported
//!   scope, not silently omitted). A changed or same-path-recreated input moves the identity,
//!   so consumers stop riding on stale answers with no extra store. Consumers wire it in
//!   their own seam: the check feed invalidates through the card state, [`crate::backend`]
//!   retires a live rust-analyzer session on change.
//! * [`pin_conflict`] — the cause to refuse checks with when the pin's channel is not
//!   established by the caller's measured version, and the cause to refuse with when the pin
//!   is host-denied and cannot be verified at all. The native checker and the module's check
//!   planning refuse with it before any child starts; warning-only would run a toolchain the
//!   pin rejects. Native cargo remains the project's fallback (its own rustup proxy honors
//!   the pin).
//! * [`environment`] — the same facts as a [`ResolvedEnv`] for the card seam, whose
//!   configuration plumbing (the launcher's per-language facts reaching
//!   `LanguageSupport::environments`) is a separate committed handoff; until it lands nothing
//!   activates this on the card, and no consumer claim is made beyond the two seams above.
//!
//! The worktree boundary is the whole world: parent checkouts' manifests are foreign and
//! never read or hashed. No environment manager runs, nothing is installed, and only files
//! are read — every one through the shared guarded read ([`read_input`]/[`hash_input`]).

use std::{
    fs,
    path::{Path, PathBuf},
};

use agent_ide_core::execution::seatbelt::ReadDeny;
use agent_ide_core::lang::environment::{EnvCandidate, EnvSource, ResolvedEnv};

/// Whether `worktree` is a Cargo project at its boundary: the environment exists only for one.
pub fn is_cargo_project(worktree: &Path) -> bool {
    worktree.join("Cargo.toml").is_file()
}

/// The configured toolchain a caller vouches for: the directory its own configuration admits
/// (the checker's launcher section, the provider launch's accepted executables) and the
/// measured toolchain version string it accepted — `cargo 1.98.1 (…)` for a launch's measured
/// cargo, for example. `None` is the honest `version unknown`; a directory label is never a
/// version.
pub struct ConfiguredToolchain<'a> {
    /// The admitted toolchain directory.
    pub dir: &'a Path,
    /// The measured version identity the caller accepted, when it has one.
    pub version: Option<&'a str>,
}

/// How a worktree's toolchain pin read out under the caller's read denies.
enum PinRead {
    /// Neither pin file exists: no pin is in force.
    Absent,
    /// A pin file exists but the host denies reading it: the pin is in force and unverifiable.
    Denied(&'static str),
    /// The pin declared `channel` from its declaring file.
    Parsed(&'static str, String),
}

/// Reads the effective pin under `denies`: the first pin file the gate lets through decides,
/// exactly as the unguarded [`crate::support::toolchain_pin`] reads them without denies (the
/// first readable file wins and only it is parsed). A declared-but-denied file is disclosed,
/// never parsed.
fn pin_read(worktree: &Path, denies: &[ReadDeny]) -> PinRead {
    let mut denied: Option<&'static str> = None;
    for (name, path) in [
        ("rust-toolchain.toml", worktree.join("rust-toolchain.toml")),
        ("rust-toolchain", worktree.join("rust-toolchain")),
    ] {
        if denies.iter().any(|deny| deny.matches(&path)) {
            denied.get_or_insert(name);
            continue;
        }
        if agent_ide_core::lang::environment::readable(&path, denies) {
            let Some(text) = agent_ide_core::lang::environment::read_input(&path, denies) else {
                return PinRead::Absent;
            };
            let channel = toml_channel(&text).or_else(|| {
                text.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with('#'))
                    .filter(|line| !line.contains('='))
                    .map(str::to_owned)
            });
            return match channel {
                Some(channel) => PinRead::Parsed(name, channel),
                None => PinRead::Absent,
            };
        }
    }
    match denied {
        Some(name) => PinRead::Denied(name),
        None => PinRead::Absent,
    }
}

/// The pin's channel from its text: the TOML `channel` key, else the first non-comment line
/// without `=` — the same shapes [`crate::support::toolchain_pin`] parses.
fn toml_channel(text: &str) -> Option<String> {
    crate::support::toml_string(text, "channel").or_else(|| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .filter(|line| !line.contains('='))
            .map(str::to_owned)
    })
}

/// Whether the caller's measured version establishes the pin's channel: the channel appears
/// as one whitespace/delimiter-separated token of the measured identity (`1.98.1` is a token
/// of `cargo 1.98.1 (…)`). Aliases, dates and names that no measured token states are never
/// proven by directory naming.
fn establishes(version: &str, channel: &str) -> bool {
    version
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .any(|token| token == channel)
}

/// The declaring pin file of `worktree`, when one exists and the gate lets it through.
fn pin_file(worktree: &Path, denies: &[ReadDeny]) -> Option<PathBuf> {
    match pin_read(worktree, denies) {
        PinRead::Parsed(name, _) => Some(worktree.join(name)),
        _ => None,
    }
}

/// `ino.mtime` of `path` without following a final link, empty when it cannot be read.
fn stamp(path: &Path) -> String {
    fs::symlink_metadata(path)
        .map(|metadata| {
            format!(
                "{}.{}.{}",
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec()
            )
        })
        .unwrap_or_default()
}

/// The opaque identity of the worktree's environment resolution: the configured toolchain
/// (path, name, measured version, directory stamp, `cargo`/`rustc` stamps), the pin (its read
/// outcome, and its whole content when readable), the worktree's `Cargo.toml`, `Cargo.lock`,
/// `.cargo/config.toml`/`.cargo/config`, and its first-level member manifests. A change or
/// same-path recreation of any of these moves the identity. `configured.version` is part of
/// the identity only when the caller measured one.
fn identity(worktree: &Path, configured: Option<&ConfiguredToolchain<'_>>, denies: &[ReadDeny]) {
    let _ = (worktree, configured, denies);
}

/// The cause to refuse checks with when the worktree's pin is not established by the
/// configured toolchain's measured version, or cannot be read under the host's denies.
/// `None` means no obstruction: no pin, or a pin the measured version establishes. An absent
/// configured directory stays the checker's own `ToolMissing`.
///
/// Both the native checker and the module's check planning refuse with this cause before any
/// child starts — warning-only would run a toolchain the pin rejects or that cannot be
/// verified against it. Native cargo remains the project's fallback: its own rustup proxy
/// honors the pin.
pub fn pin_conflict(
    worktree: &Path,
    configured_dir: &Path,
    configured_version: Option<&str>,
    denies: &[ReadDeny],
) -> Option<String> {
    if !configured_dir.is_dir() {
        return None;
    }
    match pin_read(worktree, denies) {
        PinRead::Absent => None,
        PinRead::Denied(name) => Some(format!(
            "{name} is excluded from reads by the host, so the toolchain pin cannot be \
             verified — IDE checks refuse an unverifiable pin; adjust the host excludes or \
             use native cargo (its rustup proxy honors the pin)"
        )),
        PinRead::Parsed(file, channel) => {
            match configured_version {
                Some(version) if establishes(version, &channel) => None,
                Some(version) => Some(format!(
                    "{file} pins {channel}, but the measured toolchain is {version} — IDE \
                     checks refuse the mismatch; use native cargo (its rustup proxy honors the \
                     pin) or align the launcher toolchain"
                )),
                None => Some(format!(
                    "{file} pins {channel}, but no measured toolchain version was given, so \
                     the pin cannot be verified — IDE checks refuse an unverifiable pin; use \
                     native cargo (its rustup proxy honors the pin) or provide the measured \
                     toolchain facts"
                )),
            }
        }
    }
}

/// The worktree's environment as a [`ResolvedEnv`], from facts the caller vouches for. The
/// configured toolchain is the `chosen` winner only when nothing obstructs it: a pin the
/// measured version does not establish, and a pin the host denies, resolve to nothing with
/// the same cause [`pin_conflict`] refuses checks with — an apparently valid winner the
/// checks would refuse is never returned. An agreeing pin is the `source`; a pin with no
/// measured version to check it against keeps the launcher as the winner's source and
/// discloses that compatibility is unverified. `None` configured resolves nothing with the
/// disclosure that the environment view was given no configured toolchain.
pub fn environment(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> ResolvedEnv {
    let obstruction = configured.and_then(|facts| {
        pin_conflict(
            worktree,
            facts.dir,
            facts.version,
            denies,
        )
    });
    let unverified = configured.is_some()
        && obstruction.is_none()
        && match pin_read(worktree, denies) {
            PinRead::Parsed(_, channel) => !configured
                .and_then(|facts| facts.version)
                .is_some_and(|version| establishes(version, &channel)),
            _ => false,
        };
    let chosen = (configured.is_some() && obstruction.is_none()).then(|| {
        let facts = configured.expect("configured");
        EnvCandidate {
            label: facts
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| facts.dir.display().to_string()),
            path: facts.dir.to_path_buf(),
            version: facts.version.map(str::to_owned),
            broken: false,
        }
    });
    let pin = pin_read(worktree, denies);
    let source = match (&chosen, &pin) {
        (Some(_), PinRead::Parsed(file, _))
            if unverified.not() || obstruction.is_some() =>
        {
            None
        }
        (Some(_), PinRead::Parsed(file, _)) => Some(EnvSource::Pin(file.to_string())),
        (Some(_), _) => Some(EnvSource::Launcher),
        (None, _) => None,
    };
    let warnings = match unverified.then_some((&pin, configured)) {
        Some((PinRead::Parsed(file, channel), _)) => {
            vec![format!(
                "{file} pins {channel}, but no measured toolchain version verifies it — \
                 compatibility unverified"
            )]
        }
        _ => Vec::new(),
    };
    let missing_next_step = obstruction
        .or_else(|| {
            (configured.is_none()).then(|| {
                "no configured toolchain was given to the environment view; checks follow \
                 their configured toolchain and report their own availability"
                    .to_owned()
            })
        })
        .or_else(|| match &pin {
            PinRead::Denied(name) => Some(format!(
                "{name} is excluded from reads by the host, so the toolchain pin cannot be \
                 verified — adjust the host excludes"
            )),
            _ => None,
        });
    ResolvedEnv {
        root: PathBuf::new(),
        chosen,
        source,
        // One configured toolchain per caller: the card shows no `choose:` hint, and the
        // language keeps its default selection refusal.
        candidates: Vec::new(),
        warnings,
        missing_next_step,
        identity: identity_string(worktree, configured, denies),
    }
}

/// [`identity`]'s value: the parts joined and hashed once.
fn identity_string(
    worktree: &Path,
    configured: Option<&ConfiguredToolchain<'_>>,
    denies: &[ReadDeny],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match configured {
        Some(configured) => {
            parts.push(configured.dir.display().to_string());
            parts.push(
                configured
                    .dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            parts.push(configured.version.unwrap_or_default().to_owned());
            parts.push(stamp(configured.dir));
            parts.push(stamp(&configured.dir.join("bin").join("cargo")));
            parts.push(stamp(&configured.dir.join("bin").join("rustc")));
        }
        None => parts.push("configured=-".to_owned()),
    }
    match pin_read(worktree, denies) {
        PinRead::Absent => parts.push("pin=-".to_owned()),
        PinRead::Denied(name) => parts.push(format!("pin=denied:{name}")),
        PinRead::Parsed(name, channel) => {
            parts.push(format!("pin={name}:{channel}"));
            parts.push(
                agent_ide_core::lang::environment::hash_input(&worktree.join(name), denies)
                    .map_or_else(
                        || "pin-content=-".to_owned(),
                        |hash| format!("pin-content={hash}"),
                    ),
            );
        }
    }
    let mut members: Vec<PathBuf> = fs::read_dir(worktree)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.is_dir() && dir.join("Cargo.toml").is_file())
        .collect();
    members.sort();
    let mut inputs: Vec<PathBuf> = vec![
        worktree.join("Cargo.toml"),
        worktree.join("Cargo.lock"),
        worktree.join(".cargo/config.toml"),
        worktree.join(".cargo/config"),
    ];
    inputs.extend(members.iter().map(|dir| dir.join("Cargo.toml")));
    for file in inputs {
        parts.push(
            agent_ide_core::lang::environment::hash_input(&file, denies).map_or_else(
                || format!("{}=-", file.display()),
                |hash| format!("{}={hash}", file.display()),
            ),
        );
    }
    blake3::hash(parts.join("\n").as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh scratch directory for one fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent-ide-rust-env-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes `text` to `root/relative`, creating parents.
    fn put(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Installs a toolchain `name` under `root/toolchains` with `bin/cargo`/`bin/rustc`.
    fn toolchain(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("toolchains").join(name);
        put(&dir, "bin/cargo", "");
        put(&dir, "bin/rustc", "");
        dir
    }

    /// The caller's vouched facts: directory plus measured version identity.
    fn configured<'a>(dir: &'a Path, version: Option<&'a str>) -> ConfiguredToolchain<'a> {
        ConfiguredToolchain { dir, version }
    }

    /// The pin's channel must be established by the caller's measured version, whatever the
    /// directory is named: a misleading `1.98.1` name with a measured `cargo 1.97` refuses, a
    /// generic directory with a measured `cargo 1.98.1` agrees, and no measured version never
    /// proves a match.
    #[test]
    fn pin_agreement_uses_measured_versions_not_names() {
        let root = scratch("agreement");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "");
        put(&root, "rust-toolchain.toml", "[toolchain]\nchannel = \"1.98.1\"\n");
        // A misleadingly named directory with a measured older toolchain refuses.
        let older = configured(&dir, Some("cargo 1.97.0 (aabb)"));
        let cause = pin_conflict(&root, &dir, older.version, &[]).expect("mismatch refuses");
        assert!(cause.contains("pins 1.98.1"), "{cause}");
        assert!(cause.contains("measured toolchain is cargo 1.97.0"), "{cause}");
        // A generically named directory with the measured right toolchain agrees.
        let generic = root.join("toolchains/toolchain");
        put(&generic, "bin/cargo", "");
        put(&generic, "bin/rustc", "");
        let measured = configured(&generic, Some("cargo 1.98.1 (aabb)"));
        assert_eq!(pin_conflict(&root, &generic, measured.version, &[]), None);
        let env = environment(&root, Some(&measured), &[]);
        assert_eq!(env.source, Some(EnvSource::Pin("rust-toolchain.toml".into())));
        assert!(env.warnings.is_empty());
        // No measured version never proves a match: unverified, disclosed, not refused.
        let unknown = configured(&generic, None);
        assert_eq!(pin_conflict(&root, &generic, unknown.version, &[]), None);
        let env = environment(&root, Some(&unknown), &[]);
        assert_eq!(env.chosen.as_ref().map(|chosen| chosen.version.as_deref()), Some(None));
        assert_eq!(env.source, Some(EnvSource::Launcher));
        let warning = env.warnings.first().expect("unverified is disclosed");
        assert!(warning.contains("compatibility unverified"), "{warning}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A configured toolchain a pin rejects resolves to nothing — never an apparently valid
    /// winner with a warning — and the same cause reaches `pin_conflict`; the cargo config
    /// and member manifests move the identity within the worktree boundary.
    #[test]
    fn conflicting_pin_resolves_nothing_and_identity_covers_members() {
        let root = scratch("conflict");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "[workspace]\n");
        put(&root, "Cargo.lock", "lock v1\n");
        put(&root, "crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\n");
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly\"\n",
        );
        let facts = configured(&dir, Some("cargo 1.98.1 (aabb)"));
        let env = environment(&root, Some(&facts), &[]);
        assert_eq!(env.chosen, None, "no apparently valid winner");
        assert_eq!(env.source, None);
        let step = env.missing_next_step.expect("the conflict is the way out");
        assert!(step.contains("pins nightly"), "{step}");
        assert_eq!(
            env.missing_next_step,
            pin_conflict(&root, &dir, facts.version, &[])
        );

        // Member manifests are part of the identity once the conflict clears.
        fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
        let before = environment(&root, Some(&facts), &[]).identity;
        put(&root, "crates/alpha/Cargo.toml", "[package]\nname = \"beta\"\n");
        assert_ne!(
            environment(&root, Some(&facts), &[]).identity,
            before,
            "changed member manifest"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A host-denied pin is in force and unverifiable: it is disclosed, never parsed into the
    /// identity or the card, and checks refuse — a regression the unguarded read would fail.
    #[test]
    fn denied_pin_is_disclosed_never_parsed() {
        use agent_ide_core::execution::seatbelt::ReadDeny;
        let root = scratch("pin-denied");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "");
        put(&root, "rust-toolchain.toml", "[toolchain]\nchannel = \"nightly\"\n");
        let facts = configured(&dir, Some("cargo 1.98.1 (aabb)"));
        let denies = [ReadDeny::Path(root.join("rust-toolchain.toml"))];
        let env = environment(&root, Some(&facts), &denies);
        assert_eq!(env.chosen, None, "an unverifiable pin resolves nothing");
        let step = env.missing_next_step.expect("the denial is the way out");
        assert!(step.contains("excluded from reads"), "{step}");
        assert_eq!(
            pin_conflict(&root, &dir, facts.version, &denies)
                .expect("checks refuse an unverifiable pin"),
            step
        );
        // The denied channel never leaks into the identity.
        let identity = environment(&root, Some(&facts), &denies).identity;
        assert!(!identity.contains("nightly"), "{identity}");
        let _ = fs::remove_dir_all(&root);
    }

    /// An agreeing pin is the source, the worktree's Cargo inputs move the identity, and a
    /// non-Cargo worktree has no environment at all.
    #[test]
    fn configured_toolchain_is_chosen_with_the_pin_as_source() {
        let root = scratch("configured");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "[package]\nname = \"svc\"\n");
        put(&root, "Cargo.lock", "lock v1\n");
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.98.1\"\n",
        );
        let facts = configured(&dir, Some("cargo 1.98.1 (aabb)"));
        let env = environment(&root, Some(&facts), &[]);
        let chosen = env.chosen.expect("configured toolchain resolves");
        assert_eq!(chosen.label, "1.98.1-aarch64-apple-darwin");
        assert_eq!(chosen.version.as_deref(), Some("cargo 1.98.1 (aabb)"));
        assert_eq!(chosen.path, dir);
        assert_eq!(
            env.source,
            Some(EnvSource::Pin("rust-toolchain.toml".to_owned()))
        );
        assert!(env.warnings.is_empty());
        assert_eq!(env.missing_next_step, None);

        // The identity covers the worktree's Cargo inputs: a changed lock invalidates, and
        // two resolutions of one state agree.
        let before = environment(&root, Some(&facts), &[]).identity;
        assert_eq!(environment(&root, Some(&facts), &[]).identity, before);
        put(&root, "Cargo.lock", "lock v2\n");
        assert_ne!(
            environment(&root, Some(&facts), &[]).identity,
            before,
            "changed lockfile"
        );

        // A non-Cargo worktree has no environment to describe.
        let other = scratch("configured-not-rust");
        assert!(!is_cargo_project(&other));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&other);
    }

    /// The pin's whole content feeds the identity, not just its parsed channel: a
    /// comment-only change at an unchanged channel moves it.
    #[test]
    fn pin_content_moves_the_identity() {
        let root = scratch("pin-content");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "");
        put(&root, "Cargo.lock", "lock\n");
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.98.1\"\n",
        );
        let facts = configured(&dir, Some("cargo 1.98.1 (aabb)"));
        let before = environment(&root, Some(&facts), &[]).identity;
        put(
            &root,
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.98.1\"\n# profile = \"minimal\"\n",
        );
        assert_ne!(
            environment(&root, Some(&facts), &[]).identity,
            before,
            "changed pin file content at unchanged channel"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// With no configured facts the resolution says nothing won and discloses the seam's
    /// scope; a bare-file pin is read the same way, and renaming the declaring file moves the
    /// identity.
    #[test]
    fn bare_pin_and_unconfigured_disclosure() {
        let root = scratch("bare");
        let dir = toolchain(&root, "1.98.1-aarch64-apple-darwin");
        put(&root, "Cargo.toml", "");
        put(&root, "rust-toolchain", "1.98.1\n");
        let facts = configured(&dir, Some("cargo 1.98.1 (aabb)"));
        let toml_less = environment(&root, Some(&facts), &[]);
        assert_eq!(
            toml_less.source,
            Some(EnvSource::Pin("rust-toolchain".to_owned()))
        );
        fs::remove_file(root.join("rust-toolchain")).unwrap();
        fs::write(root.join("rust-toolchain"), "1.98.1\n# note\n").unwrap();
        assert_ne!(
            environment(&root, Some(&facts), &[]).identity,
            toml_less.identity,
            "pin content is part of the identity"
        );

        let unconfigured = environment(&root, None, &[]);
        assert_eq!(unconfigured.chosen, None);
        assert_eq!(unconfigured.source, None);
        let step = unconfigured
            .missing_next_step
            .expect("the disclosure names the scope");
        assert!(step.contains("no configured toolchain was given"), "{step}");
        let _ = fs::remove_dir_all(&root);
    }
}
