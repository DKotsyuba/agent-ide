//! Installs one sealed release bundle into the immutable standalone layout.
//!
//! `agent-ide self-install` is the single code path that writes the standalone `current`
//! symlink, the managed launcher shim, and the plugin `current` symlink. It verifies the
//! sealed bundle (`metadata.json`, per-file SHA-256 digests in `SHA256SUMS`, and the final
//! `COMPLETE` marker), installs it immutably under `<prefix>/releases/<version>`, stages the
//! host plugin parts exactly like the historical `scripts/install-local.sh`, and writes the
//! managed launcher — all under one advisory `flock` on `<prefix>/.install.lock`, with no
//! daemon involved.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::userhome;

/// Exact bytes of the final seal marker; its digest is already recorded in `SHA256SUMS`.
const SEAL_MARKER: &[u8] = b"complete\n";

/// Bundle-relative names of the three seal files the packager writes.
const METADATA_FILE: &str = "metadata.json";
const CHECKSUMS_FILE: &str = "SHA256SUMS";
const COMPLETE_FILE: &str = "COMPLETE";

/// Prefix tree names the installer owns below the operator-selected roots.
const LOCK_FILE: &str = ".install.lock";
const RELEASES_DIR: &str = "releases";
const PLUGIN_DIR: &str = "plugin";
const BINARY_NAME: &str = "agent-ide";

/// Second line of every managed launcher shim; its presence marks the file as ours.
const LAUNCHER_MARKER: &str = "# agent-ide managed launcher v1";

/// Plugin parts staged into `<share-dir>/plugin/<version>/`, in the fixed bundle order.
const PLUGIN_PARTS: &[&str] = &[".claude-plugin", ".codex-plugin", "hooks", "skills"];

/// Distinguishes temporary staging names created by one process.
static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Raw `self-install` CLI flags; optional paths resolve against the user home in [`resolve`].
#[derive(Debug, Eq, PartialEq)]
pub struct Args {
    /// Existing sealed release bundle directory to install.
    pub release: PathBuf,
    /// Exact `X.Y.Z` the bundle's `metadata.json` must carry and the layout must name.
    pub version: String,
    /// State home; default is the effective user home (`AGENT_IDE_HOME` override or passwd)
    /// plus `.agent-ide`.
    pub home: Option<PathBuf>,
    /// Standalone prefix; default is `<home>/standalone`.
    pub prefix: Option<PathBuf>,
    /// Launcher directory; default is `<home>/.local/bin`.
    pub bin_dir: Option<PathBuf>,
    /// Plugin root parent; default is `<home>/.local/share/agent-ide`.
    pub share_dir: Option<PathBuf>,
    /// Source builds only: replace an already installed release of the same version whose
    /// bytes differ instead of refusing (published releases never change bytes).
    pub replace: bool,
}

/// Everything one install needs, with every path resolved absolute and normalized.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Options {
    /// Existing sealed release bundle directory to install.
    pub release: PathBuf,
    /// Exact `X.Y.Z` the bundle's `metadata.json` must carry and the layout must name.
    pub version: String,
    /// State home embedded in the managed launcher shim.
    pub home: PathBuf,
    /// Standalone prefix holding `releases/` and `current`.
    pub prefix: PathBuf,
    /// Directory receiving the managed `agent-ide` launcher shim.
    pub bin_dir: PathBuf,
    /// Directory holding `plugin/<version>/` and `plugin/current`.
    pub share_dir: PathBuf,
    /// Replace a same-version release with different bytes (source builds only).
    pub replace: bool,
}

/// One completed install, rendered as the command's JSON summary line.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Summary {
    /// Installed version, identical to `Options::version`.
    pub version: String,
    /// Standalone prefix that now owns `releases/<version>` and `current`.
    pub prefix: PathBuf,
    /// Prefix `current` symlink, now selecting `version`.
    pub current: PathBuf,
    /// Managed launcher shim path.
    pub launcher: PathBuf,
    /// Plugin root `current` symlink, now selecting `version`.
    pub plugin_current: PathBuf,
    /// `installed` when this run swapped `current`, `refreshed` when only the launcher moved.
    pub action: &'static str,
}

impl Summary {
    /// Renders the fixed six-field JSON summary printed on success.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "version": self.version,
            "prefix": self.prefix,
            "current": self.current,
            "launcher": self.launcher,
            "plugin_current": self.plugin_current,
            "action": self.action,
        })
        .to_string()
    }
}

/// Parses the flags after the `self-install` subcommand; every flag is `--name value` and no
/// flag may repeat. Unknown flags, missing values, and non-UTF-8 arguments are rejected before
/// any filesystem access.
pub fn parse_args(rest: &[OsString]) -> Result<Args, String> {
    let mut release: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    let mut home: Option<PathBuf> = None;
    let mut prefix: Option<PathBuf> = None;
    let mut bin_dir: Option<PathBuf> = None;
    let mut share_dir: Option<PathBuf> = None;
    let mut replace = false;
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index]
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 self-install argument: {:?}", rest[index]))?;
        if flag == "--replace" {
            replace = true;
            index += 1;
            continue;
        }
        let value = rest
            .get(index + 1)
            .ok_or_else(|| format!("{flag} needs a value"))?
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 value for {flag}"))?;
        match flag {
            "--release" => set_path(&mut release, flag, value)?,
            "--home" => set_path(&mut home, flag, value)?,
            "--prefix" => set_path(&mut prefix, flag, value)?,
            "--bin-dir" => set_path(&mut bin_dir, flag, value)?,
            "--share-dir" => set_path(&mut share_dir, flag, value)?,
            "--version" => {
                if version.is_some() {
                    return Err("--version given twice".to_owned());
                }
                version = Some(value.to_owned());
            }
            _ => return Err(format!("unknown self-install flag: {flag}")),
        }
        index += 2;
    }
    Ok(Args {
        release: release.ok_or("self-install needs --release <dir>")?,
        version: version.ok_or("self-install needs --version X.Y.Z")?,
        home,
        prefix,
        bin_dir,
        share_dir,
        replace,
    })
}

/// Stores one path flag, refusing a second occurrence.
fn set_path(slot: &mut Option<PathBuf>, flag: &str, value: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{flag} given twice"));
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

/// Resolves the documented defaults: `--home` is the effective user home plus `.agent-ide`,
/// `--prefix` is `<home>/standalone`, `--bin-dir` is `<user home>/.local/bin`, and
/// `--share-dir` is `<user home>/.local/share/agent-ide`. `AGENT_IDE_HOME` relocates the
/// whole per-user tree, exactly as the daemon and the journal treat it. Every result is absolute, normalized,
/// not the filesystem root, and free of single quotes so the launcher shim can quote it.
pub fn resolve(args: Args) -> Result<Options, String> {
    checked_version(&args.version)?;
    let default_home = userhome::user_home();
    let home = match args.home {
        Some(home) => home,
        None => default_home
            .clone()
            .ok_or("cannot resolve the user home; pass --home or set AGENT_IDE_HOME")?
            .join(".agent-ide"),
    };
    let prefix = args.prefix.unwrap_or_else(|| home.join("standalone"));
    let bin_dir = match args.bin_dir {
        Some(bin_dir) => bin_dir,
        None => default_home
            .clone()
            .ok_or("cannot resolve the user home; pass --bin-dir or set AGENT_IDE_HOME")?
            .join(".local/bin"),
    };
    let share_dir = match args.share_dir {
        Some(share_dir) => share_dir,
        None => default_home
            .ok_or("cannot resolve the user home; pass --share-dir or set AGENT_IDE_HOME")?
            .join(".local/share/agent-ide"),
    };
    Ok(Options {
        release: checked_absolute(args.release, "--release")?,
        version: args.version,
        home: checked_absolute(home, "--home")?,
        prefix: checked_absolute(prefix, "--prefix")?,
        bin_dir: checked_absolute(bin_dir, "--bin-dir")?,
        share_dir: checked_absolute(share_dir, "--share-dir")?,
        replace: args.replace,
    })
}

/// Accepts only a numeric `X.Y.Z`; the version names release directories and symlinks.
fn checked_version(version: &str) -> Result<(), String> {
    let well_formed = version.split('.').count() == 3
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if well_formed {
        Ok(())
    } else {
        Err(format!("--version must be numeric X.Y.Z, got: {version}"))
    }
}

/// Absolutizes one operator path against the current directory and rejects the filesystem
/// root, `..` and `.` components, and single quotes (the shim quotes every embedded path).
fn checked_absolute(path: PathBuf, flag: &str) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path
    } else {
        let current =
            std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
        current.join(path)
    };
    if path == Path::new("/") {
        return Err(format!("{flag} must not be the filesystem root"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "{flag} must be a normalized absolute path without '..': {}",
            path.display()
        ));
    }
    if path.as_os_str().to_string_lossy().contains('\'') {
        return Err(format!(
            "{flag} must not contain a single quote: {}",
            path.display()
        ));
    }
    Ok(path)
}

/// Runs one complete install: resolves defaults, locks the prefix, verifies and installs the
/// release, swaps both `current` symlinks when the selection changes, and writes the launcher.
pub fn run(args: Args) -> Result<Summary, String> {
    let options = resolve(args)?;
    let _lock = InstallLock::acquire(&options.prefix)?;
    install(&options)
}

/// Performs the locked install for already-resolved options.
fn install(options: &Options) -> Result<Summary, String> {
    verify_release(&options.release, &options.version)?;
    for part in PLUGIN_PARTS {
        if !options.release.join(part).is_dir() {
            return Err(format!("release bundle is missing the {part} plugin part"));
        }
    }
    let releases = options.prefix.join(RELEASES_DIR);
    create_dir(&releases)?;
    let selected = releases.join(&options.version);
    if fs::symlink_metadata(&selected).is_ok() {
        if let Err(reason) = identical_release(&selected, &options.release, &options.version) {
            if !options.replace {
                return Err(format!(
                    "refusing to overwrite a different immutable release: {reason}"
                ));
            }
            // A source build re-installed under the same version: retire the old copy first so
            // the immutable directory is rebuilt whole rather than patched in place.
            let retired = releases.join(format!(
                ".replaced-{}-{}",
                options.version,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0)
            ));
            fs::rename(&selected, &retired)
                .map_err(|error| format!("{}: {error}", selected.display()))?;
            let _ = fs::remove_dir_all(&retired);
            install_release(&options.release, &selected, &options.version)?;
        }
    } else {
        install_release(&options.release, &selected, &options.version)?;
    }

    let current = options.prefix.join("current");
    // `current` names the versioned release below `releases/`; the link target stays relative.
    let current_target_name = format!("{RELEASES_DIR}/{}", options.version);
    let action = if current_target(&current).as_deref() == Some(current_target_name.as_str()) {
        // Same version already selected: the immutable release and the plugin are already in
        // place, so only the launcher below is refreshed.
        "refreshed"
    } else {
        swap_symlink(&current, &current_target_name)?;
        stage_plugin(options)?;
        "installed"
    };
    let launcher = write_launcher(options)?;
    Ok(Summary {
        version: options.version.clone(),
        prefix: options.prefix.clone(),
        current,
        launcher,
        plugin_current: options.share_dir.join(PLUGIN_DIR).join("current"),
        action,
    })
}

/// Copies the verified bundle into a staged directory, writes `COMPLETE` last, re-verifies the
/// staged copy, and renames it into its immutable versioned name.
fn install_release(release: &Path, selected: &Path, version: &str) -> Result<(), String> {
    let staged = create_unique_dir(
        selected
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", selected.display()))?,
    )?;
    copy_tree(release, &staged, Some(COMPLETE_FILE))?;
    fs::write(staged.join(COMPLETE_FILE), SEAL_MARKER)
        .map_err(|error| format!("{}: {error}", staged.join(COMPLETE_FILE).display()))?;
    if let Err(reason) = verify_release(&staged, version) {
        return Err(format!("staged release failed verification: {reason}"));
    }
    fs::rename(&staged, selected).map_err(|error| format!("{}: {error}", selected.display()))
}

/// Accepts an already-installed release only when it is a real directory that verifies with
/// byte-identical `SHA256SUMS`; any difference refuses the install at the caller.
fn identical_release(existing: &Path, candidate: &Path, version: &str) -> Result<(), String> {
    if fs::symlink_metadata(existing)
        .map_err(|error| format!("{}: {error}", existing.display()))?
        .file_type()
        .is_symlink()
    {
        return Err("the existing release directory is a symlink".to_owned());
    }
    verify_release(existing, version)?;
    let existing_sums = fs::read(existing.join(CHECKSUMS_FILE))
        .map_err(|error| format!("{}: {error}", existing.join(CHECKSUMS_FILE).display()))?;
    let candidate_sums = fs::read(candidate.join(CHECKSUMS_FILE))
        .map_err(|error| format!("{}: {error}", candidate.join(CHECKSUMS_FILE).display()))?;
    if existing_sums != candidate_sums {
        return Err("the existing SHA256SUMS differs from the candidate".to_owned());
    }
    Ok(())
}

/// Stages the four plugin parts, regenerates the Claude hook to exec the managed launcher
/// (the `scripts/install-local.sh` contract), replaces the version directory, and swaps
/// `plugin/current`.
fn stage_plugin(options: &Options) -> Result<(), String> {
    let plugin_root = options.share_dir.join(PLUGIN_DIR);
    create_dir(&plugin_root)?;
    let staged = create_unique_dir(&plugin_root)?;
    for part in PLUGIN_PARTS {
        copy_tree(&options.release.join(part), &staged.join(part), None)?;
    }
    let hook = staged.join("hooks/claude-hook.sh");
    // The bundle copy is replaced: the installed hook execs the managed launcher, so the
    // installed copy no longer needs `AGENT_IDE_BIN` (the install-local.sh contract).
    remove_path(&hook)?;
    write_executable(
        &hook,
        format!(
            "#!/bin/sh\nexec \"{}\" claude-hook\n",
            options.bin_dir.join(BINARY_NAME).display()
        )
        .as_bytes(),
    )?;
    let version_dir = plugin_root.join(&options.version);
    remove_path(&version_dir)?;
    fs::rename(&staged, &version_dir)
        .map_err(|error| format!("{}: {error}", version_dir.display()))?;
    swap_symlink(&plugin_root.join("current"), &options.version)
}

/// Verifies one sealed release directory end to end: the `COMPLETE` seal, the metadata
/// version, a symlink-free regular-file tree, and `SHA256SUMS` coverage with matching
/// digests and safe unique relative paths.
pub fn verify_release(dir: &Path, version: &str) -> Result<(), String> {
    if fs::symlink_metadata(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?
        .file_type()
        .is_symlink()
    {
        return Err(format!(
            "release directory must not be a symlink: {}",
            dir.display()
        ));
    }
    if !dir.is_dir() {
        return Err(format!(
            "release directory does not exist: {}",
            dir.display()
        ));
    }
    let complete = fs::read(dir.join(COMPLETE_FILE))
        .map_err(|error| format!("{}: {error}", dir.join(COMPLETE_FILE).display()))?;
    if complete != SEAL_MARKER {
        return Err(format!(
            "release seal {COMPLETE_FILE} must be exactly {:?}",
            String::from_utf8_lossy(SEAL_MARKER)
        ));
    }
    let metadata_bytes = fs::read(dir.join(METADATA_FILE))
        .map_err(|error| format!("{}: {error}", dir.join(METADATA_FILE).display()))?;
    let metadata: serde_json::Value = serde_json::from_slice(&metadata_bytes).map_err(|error| {
        format!(
            "{} is not valid JSON: {error}",
            dir.join(METADATA_FILE).display()
        )
    })?;
    if metadata.get("version").and_then(serde_json::Value::as_str) != Some(version) {
        return Err(format!(
            "{METADATA_FILE} version {:?} does not match --version {version}",
            metadata.get("version").unwrap_or(&serde_json::Value::Null)
        ));
    }
    if metadata.get("format") != Some(&serde_json::json!(1)) {
        return Err(format!("{METADATA_FILE} format must be 1"));
    }
    let files = walk(dir, dir)?;
    let sums = fs::read_to_string(dir.join(CHECKSUMS_FILE))
        .map_err(|error| format!("{}: {error}", dir.join(CHECKSUMS_FILE).display()))?;
    let mut listed: BTreeMap<String, String> = BTreeMap::new();
    for (number, line) in sums.lines().enumerate() {
        if line.len() < 67 || &line[64..66] != "  " {
            return Err(format!(
                "malformed {CHECKSUMS_FILE} line {}: {line}",
                number + 1
            ));
        }
        let digest = &line[..64];
        let relative = &line[66..];
        if !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "malformed {CHECKSUMS_FILE} line {}: digest must be 64 lowercase hex digits",
                number + 1
            ));
        }
        safe_relative_path(relative)?;
        if listed
            .insert(relative.to_owned(), digest.to_owned())
            .is_some()
        {
            return Err(format!("{CHECKSUMS_FILE} lists {relative} twice"));
        }
    }
    for relative in &files {
        if relative != CHECKSUMS_FILE && !listed.contains_key(relative) {
            return Err(format!("{CHECKSUMS_FILE} is missing {relative}"));
        }
    }
    for (relative, digest) in &listed {
        let path = dir.join(relative);
        let actual =
            sha256_hex(&fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?);
        if &actual != digest {
            return Err(format!("{CHECKSUMS_FILE} hash mismatch for {relative}"));
        }
    }
    Ok(())
}

/// Collects every regular file below `root` as a bundle-relative path, refusing symlinks and
/// special files anywhere in the tree.
fn walk(root: &Path, dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut files = BTreeSet::new();
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "release bundle must not contain symlinks: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            files.extend(walk(root, &path)?);
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("{}: {error}", path.display()))?
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 bundle entry name: {}", path.display()))?;
            files.insert(relative.to_owned());
        } else {
            return Err(format!(
                "release bundle must contain only regular files: {}",
                path.display()
            ));
        }
    }
    Ok(files)
}

/// Accepts only non-empty relative paths without `..`, `.`, absolute, or prefix components.
fn safe_relative_path(relative: &str) -> Result<(), String> {
    if relative.is_empty() {
        return Err("SHA256SUMS contains an empty path".to_owned());
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe relative path in SHA256SUMS: {relative}"));
    }
    Ok(())
}

/// Copies one tree, refusing every entry that is not a regular file or directory. When
/// `skip` names a top-level entry it is left uncopied; the installer writes `COMPLETE`
/// itself after everything else is in place.
fn copy_tree(source_root: &Path, target_root: &Path, skip: Option<&str>) -> Result<(), String> {
    create_dir(target_root)?;
    let entries =
        fs::read_dir(source_root).map_err(|error| format!("{}: {error}", source_root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", source_root.display()))?;
        let source = entry.path();
        if skip.is_some_and(|skip| source.file_name().is_some_and(|name| name == skip)) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", source.display()))?;
        let target = target_root.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&source, &target, None)?;
        } else if file_type.is_file() {
            fs::copy(&source, &target).map_err(|error| format!("{}: {error}", source.display()))?;
        } else {
            return Err(format!(
                "refusing non-regular bundle entry: {}",
                source.display()
            ));
        }
    }
    Ok(())
}

/// Swaps one `current` symlink atomically: the replacement link is created under a temporary
/// name and renamed over the link, never moved into a directory it points at.
fn swap_symlink(link: &Path, target: &str) -> Result<(), String> {
    let parent = link
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", link.display()))?;
    create_dir(parent)?;
    let next = parent.join(format!(".current-next-{}", unique_suffix()));
    std::os::unix::fs::symlink(target, &next)
        .map_err(|error| format!("{}: {error}", next.display()))?;
    if let Err(error) = fs::rename(&next, link) {
        let _ = fs::remove_file(&next);
        return Err(format!("{}: {error}", link.display()));
    }
    Ok(())
}

/// Reads the current target of one `current` symlink, if it exists and names something.
fn current_target(link: &Path) -> Option<String> {
    let target = fs::read_link(link).ok()?;
    target.to_str().map(str::to_owned)
}

/// Writes the managed launcher shim: staged under a temporary name, mode 0755, fsynced, then
/// renamed over the launcher path.
fn write_launcher(options: &Options) -> Result<PathBuf, String> {
    create_dir(&options.bin_dir)?;
    let launcher = options.bin_dir.join(BINARY_NAME);
    ensure_launcher_owned(options, &launcher)?;
    let shim = launcher_shim(options);
    let temporary = options
        .bin_dir
        .join(format!(".agent-ide-{}", unique_suffix()));
    write_executable(&temporary, shim.as_bytes())?;
    fs::rename(&temporary, &launcher)
        .map_err(|error| format!("{}: {error}", launcher.display()))?;
    Ok(launcher)
}

/// Renders the exact managed shim bytes for one prefix. The shim sets no environment: the
/// daemon resolves its home itself, and `AGENT_IDE_HOME` keeps its single meaning (a user-home
/// override for tests and relocation).
fn launcher_shim(options: &Options) -> String {
    format!(
        "#!/bin/sh\n{LAUNCHER_MARKER}\nexec '{}/current/{BINARY_NAME}' \"$@\"\n",
        options.prefix.display(),
    )
}

/// Recognizes the managed shim format, returning the embedded prefix (and an empty home, kept
/// for the earlier shape) when the bytes are exactly one shim for any prefix.
fn parse_shim(contents: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(contents).ok()?;
    let mut lines = text.split('\n');
    if lines.next()? != "#!/bin/sh" || lines.next()? != LAUNCHER_MARKER {
        return None;
    }
    let home = "";
    let prefix = lines
        .next()?
        .strip_prefix("exec '")?
        .strip_suffix(&format!("/current/{BINARY_NAME}' \"$@\""))?;
    if !lines.next()?.is_empty() || lines.next().is_some() {
        return None;
    }
    if prefix.is_empty() {
        return None;
    }
    Some((home.to_owned(), prefix.to_owned()))
}

/// Accepts an existing launcher path only when this product owns it: absent, a managed shim,
/// a symlink into the prefix `current`, or a previously installed Mach-O binary — which moves
/// aside exactly once as `agent-ide.bak-<old version>`.
fn ensure_launcher_owned(options: &Options, launcher: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(launcher) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", launcher.display())),
    };
    if metadata.is_symlink() {
        let target =
            fs::read_link(launcher).map_err(|error| format!("{}: {error}", launcher.display()))?;
        let absolute = if target.is_absolute() {
            target
        } else {
            launcher
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf()
                .join(target)
        };
        if absolute.starts_with(options.prefix.join("current")) {
            return Ok(());
        }
        return Err(format!(
            "unowned launcher: {} is a symlink outside {}",
            launcher.display(),
            options.prefix.join("current").display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "unowned launcher: {} is not a regular file",
            launcher.display()
        ));
    }
    let contents =
        fs::read(launcher).map_err(|error| format!("{}: {error}", launcher.display()))?;
    // Any file carrying the marker on its second line is a managed shim (including the first
    // 0.4.1 candidate shape that exported AGENT_IDE_HOME); it is rewritten, never refused.
    if parse_shim(&contents).is_some()
        || std::str::from_utf8(&contents)
            .ok()
            .and_then(|text| text.split('\n').nth(1))
            .is_some_and(|line| line == LAUNCHER_MARKER)
    {
        return Ok(());
    }
    if metadata.permissions().mode() & 0o111 != 0 && is_macho(&contents) {
        let label = binary_version_label(launcher);
        let mut backup = launcher.with_file_name(format!("{BINARY_NAME}.bak-{label}"));
        if fs::symlink_metadata(&backup).is_ok() {
            // An earlier installer already kept a copy under that name; keep this one too
            // rather than refusing the migration.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            backup = launcher.with_file_name(format!("{BINARY_NAME}.bak-{label}-{stamp}"));
        }
        fs::rename(launcher, &backup)
            .map_err(|error| format!("{}: {error}", launcher.display()))?;
        return Ok(());
    }
    Err(format!(
        "unowned launcher: {} is neither a managed shim, a symlink into {}, nor a previously installed binary",
        launcher.display(),
        options.prefix.join("current").display()
    ))
}

/// Recognizes the Mach-O magics a previously installed `agent-ide` binary can carry: a
/// little-endian 64-bit Mach-O image or a universal fat binary.
fn is_macho(contents: &[u8]) -> bool {
    matches!(
        contents.first_chunk::<4>(),
        Some([0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe])
    )
}

/// Names one backup from the binary's own first `--version` output line, else a UTC epoch
/// stamp, sanitized to the `scripts/install-local.sh` filename alphabet.
fn binary_version_label(binary: &Path) -> String {
    let label = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            format!("ts-{seconds}")
        });
    label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// Holds an exclusive advisory `flock` on `<prefix>/.install.lock` for one whole install.
struct InstallLock(File);

impl Drop for InstallLock {
    /// Releases the advisory lock explicitly; dropping the descriptor would too.
    fn drop(&mut self) {
        // SAFETY: `flock` needs only a valid open descriptor and stores nothing.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl InstallLock {
    /// Creates the lock file below `prefix` and blocks until this process owns the lock.
    fn acquire(prefix: &Path) -> Result<InstallLock, String> {
        create_dir(prefix)?;
        let path = prefix.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // SAFETY: `flock` needs only a valid open descriptor and stores nothing.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "{}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(InstallLock(file))
    }
}

/// Exclusively creates one directory, mapping the error to its path.
fn create_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Creates one uniquely named staging directory below `parent`.
fn create_unique_dir(parent: &Path) -> Result<PathBuf, String> {
    for _ in 0..8 {
        let candidate = parent.join(format!(".install-{}", unique_suffix()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", candidate.display())),
        }
    }
    Err("cannot create a unique staging directory".to_owned())
}

/// Writes owner-executable file bytes through a fresh 0755 file.
fn write_executable(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o755)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Removes one path of any supported kind, treating absence as success.
fn remove_path(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))
        }
        Ok(_) => fs::remove_file(path).map_err(|error| format!("{}: {error}", path.display())),
    }
}

/// Builds a per-process unique suffix for staging and temporary names; creation is still
/// exclusive, so a collision only costs one retry.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Computes the SHA-256 digest of `bytes` (FIPS 180-4), the sealed-bundle format's only hash.
///
/// ponytail: one-shot non-streaming digest sized for a few-megabyte bundle; add streaming only
/// if bundles outgrow memory.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let (blocks, remainder) = bytes.as_chunks::<64>();
    for chunk in blocks {
        compress(&mut state, chunk, &K);
    }
    let mut tail = [0u8; 128];
    let tail_len = if remainder.len() + 9 <= 64 { 64 } else { 128 };
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    tail[tail_len - 8..tail_len].copy_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for chunk in tail[..tail_len].as_chunks::<64>().0 {
        compress(&mut state, chunk, &K);
    }
    let mut digest = [0u8; 32];
    for (word, slice) in state.iter().zip(digest.as_chunks_mut::<4>().0) {
        slice.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Applies one 512-bit SHA-256 compression round block to `state`.
fn compress(state: &mut [u32; 8], chunk: &[u8], k: &[u32; 64]) {
    let mut w = [0u32; 64];
    for (word, input) in w.iter_mut().zip(chunk.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*input);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(k[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Returns the lowercase hexadecimal SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(64);
    for byte in sha256(bytes) {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 known-answer vectors pin the pure-std digest the whole seal relies on.
    #[test]
    fn sha256_matches_the_known_answer_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The shim parser accepts exactly the bytes `launcher_shim` renders, for any home.
    #[test]
    fn parse_shim_round_trips_the_rendered_shim() {
        let options = Options {
            release: PathBuf::from("/tmp/release"),
            version: "1.2.3".to_owned(),
            home: PathBuf::from("/Users/someone/.agent-ide"),
            prefix: PathBuf::from("/Users/someone/.agent-ide/standalone"),
            bin_dir: PathBuf::from("/Users/someone/.local/bin"),
            share_dir: PathBuf::from("/Users/someone/.local/share/agent-ide"),
            replace: false,
        };
        let shim = launcher_shim(&options);
        assert_eq!(
            parse_shim(shim.as_bytes()),
            Some((
                String::new(),
                "/Users/someone/.agent-ide/standalone".to_owned(),
            ))
        );
        assert_eq!(parse_shim(b"#!/bin/sh\nexit 0\n"), None);
        assert_eq!(parse_shim(b""), None);
    }

    /// Only plain relative normal paths pass the SHA256SUMS path check.
    #[test]
    fn safe_relative_path_rejects_escape_and_absolute_forms() {
        assert!(safe_relative_path("hooks/hooks.json").is_ok());
        assert!(safe_relative_path("../escape").is_err());
        assert!(safe_relative_path("/etc/passwd").is_err());
        assert!(safe_relative_path("./here").is_err());
        assert!(safe_relative_path("").is_err());
    }
}
