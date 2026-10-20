//! The single Python environment resolver (environment-selection design §5.1).
//!
//! Every consumer — the project card, the project check, the Pyright session, test and format
//! commands and the syntax probe — asks [`resolve`] (directly or through `environments`), so
//! they can never disagree about which interpreter a project root uses. Precedence per root:
//!
//! 1. a Pyright config pin (`pyrightconfig.json` when it exists, else `[tool.pyright]`
//!    `venvPath`+`venv`); a pinned environment that is missing or broken resolves to nothing with
//!    a detail, never to a fallback;
//! 2. the agent's stored selection ([`agent_ide_core::lang::environment::selections`]), with the
//!    same no-fallback rule;
//! 3. discovery: the first runnable of `.venv`, `venv`, then suffixed `.venv*`/`venv*` sorted by
//!    name, beside the root, else (for a nested root) beside the worktree.
//!
//! `.python-version` is a version request only: a mismatch with the chosen environment becomes a
//! warning and never switches it. Only files are read, under the host's read denies; no
//! environment manager ever runs.
//!
//! A resolution's opaque [`ResolvedEnv::identity`] covers the native project inputs of the root
//! (`native_inputs`: manifest, pin file, `requirements*.txt` and their local includes,
//! lockfiles, the version request)
//! besides the chosen environment itself, so a changed or recreated manifest, lock or config
//! invalidates every consumer of the old answer — the card, the checks, the Pyright session,
//! tests and formatting — through the same identity comparison, with no extra store. These
//! text fingerprints do not attest installed dependencies. Incomplete requirement-reference
//! coverage produces a warning; requirements never request an interpreter version.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use agent_ide_core::execution::seatbelt::ReadDeny;
use agent_ide_core::lang::environment::{CommandEnv, EnvCandidate, EnvSource, ResolvedEnv};
use agent_ide_core::modules::contract::ModuleConfig;

use crate::checks::{
    PYPROJECT_FILE, PYRIGHT_CONFIG_FILE, allowed_file, read_config, read_pyproject_venv_keys,
    read_pyrightconfig_venv_keys,
};
use crate::support::{ROOT_MARKER_FILES, is_python_project, python_roots, venv_directories};

/// A Pyright config pin in force for one project root.
struct Pin {
    /// The file that sets it: `pyrightconfig.json` or `pyproject.toml`.
    file: &'static str,
    /// The `venv` key as written.
    venv: String,
    /// The environment directory `venvPath`/`venv` name.
    dir: PathBuf,
}

/// Reads the Pyright pin of `root`. Pyright reads `[tool.pyright]` only when no
/// `pyrightconfig.json` exists, so an existing JSON config without the two keys pins nothing.
fn pin(root: &Path, denies: &[ReadDeny]) -> Option<Pin> {
    let (file, (venv_path, venv)) = if fs::symlink_metadata(root.join(PYRIGHT_CONFIG_FILE)).is_ok()
    {
        (
            PYRIGHT_CONFIG_FILE,
            read_pyrightconfig_venv_keys(root, denies)?,
        )
    } else {
        (PYPROJECT_FILE, read_pyproject_venv_keys(root, denies)?)
    };
    // A relative `venvPath` is relative to the config file's directory, as Pyright reads it.
    let dir = root.join(venv_path).join(&venv);
    Some(Pin { file, venv, dir })
}

/// The `bin/python` interpreter of an environment directory.
fn python_of(dir: &Path) -> PathBuf {
    dir.join("bin").join("python")
}

/// The interpreter of a resolved environment, when one is chosen.
pub fn interpreter(env: &ResolvedEnv) -> Option<PathBuf> {
    env.chosen.as_ref().map(|chosen| python_of(&chosen.path))
}

/// Whether the environment `dir` has an interpreter: `Some(true)` when `bin/python` is a readable
/// file, `Some(false)` when it is a dangling link (its base interpreter is gone), `None` when it
/// is absent or hidden by host read denies.
fn interpreter_state(dir: &Path, denies: &[ReadDeny]) -> Option<bool> {
    let python = python_of(dir);
    if allowed_file(&python, denies) {
        Some(true)
    } else if denies.is_empty()
        && fs::symlink_metadata(&python).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        Some(false)
    } else {
        None
    }
}

/// `X.Y.Z` from a `pyvenv.cfg` version value: CPython writes `3.12.7`, uv `3.14.0`, virtualenv
/// `3.12.7.final.0`; anything that does not start with a number is no version.
fn release(value: &str) -> Option<String> {
    let parts: Vec<&str> = value
        .split('.')
        .take(3)
        .take_while(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("."))
}

/// What the card shows for an environment directory: relative to the root, else relative to the
/// worktree, else the absolute path.
fn label(worktree: &Path, root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .or_else(|_| dir.strip_prefix(worktree))
        .unwrap_or(dir)
        .display()
        .to_string()
}

/// Describes one environment directory from its `pyvenv.cfg` (PEP 405), read under the host's
/// denies: the version from `version`, else `version_info`, else none; broken when its
/// interpreter is a dangling link (`runnable` false) or the `home` directory is gone.
fn candidate(
    worktree: &Path,
    root: &Path,
    dir: &Path,
    runnable: bool,
    denies: &[ReadDeny],
) -> EnvCandidate {
    let config = read_config(&dir.join("pyvenv.cfg"), denies).unwrap_or_default();
    let value = |key: &str| {
        config.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().to_owned())
        })
    };
    EnvCandidate {
        label: label(worktree, root, dir),
        path: std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()),
        version: value("version")
            .or_else(|| value("version_info"))
            .and_then(|version| release(&version)),
        broken: !runnable || value("home").is_some_and(|home| !Path::new(&home).is_dir()),
    }
}

/// Discovered environments of `root` in discovery order, broken ones included; a nested root
/// without its own falls back to the worktree's, which covers every package beneath it.
fn discovered(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Vec<EnvCandidate> {
    let mut dirs = venv_directories(root);
    if dirs.is_empty() && root != worktree {
        dirs = venv_directories(worktree);
    }
    dirs.iter()
        .filter_map(|dir| {
            interpreter_state(dir, denies)
                .map(|runnable| candidate(worktree, root, dir, runnable, denies))
        })
        .collect()
}

/// The directory `selector` names for `root`: a candidate label, else a path (relative to the
/// root, or absolute).
fn selected_dir(root: &Path, selector: &str, candidates: &[EnvCandidate]) -> PathBuf {
    candidates
        .iter()
        .find(|candidate| candidate.label == selector)
        .map_or_else(|| root.join(selector), |candidate| candidate.path.clone())
}

/// The `ide.start environment` key that addresses `root`: `python`, or `python:<root>` for a
/// nested root.
fn selector_key(worktree: &Path, root: &Path) -> String {
    match root.strip_prefix(worktree) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            format!("python:{}", relative.display())
        }
        _ => "python".to_owned(),
    }
}

/// The candidates as the card lists them: `.venv, .venv-py314`, or `none`.
fn labels(candidates: &[EnvCandidate]) -> String {
    if candidates.is_empty() {
        return "none".to_owned();
    }
    candidates
        .iter()
        .map(|candidate| candidate.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Cause and next step when discovery finds nothing runnable for `root`.
fn nothing_found(worktree: &Path, root: &Path, key: &str, candidates: &[EnvCandidate]) -> String {
    let manifest = ROOT_MARKER_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| root.to_path_buf());
    let beside = label(worktree, worktree, &manifest);
    let beside = if beside.is_empty() {
        "the worktree root".to_owned()
    } else {
        beside
    };
    let cause = if candidates.is_empty() {
        format!("no .venv* beside {beside}")
    } else {
        format!(
            "every environment beside {beside} is broken (base interpreter gone): {}",
            labels(candidates)
        )
    };
    format!("{cause}; create one (uv venv) or ide.start environment {{\"{key}\": \"<path>\"}}")
}

/// The version `.python-version` requests for `root` (its own, else the worktree's), read under
/// the host's denies: a denied file is unreadable and says nothing.
fn version_request(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Option<String> {
    [root, worktree].iter().find_map(|dir| {
        read_config(&dir.join(".python-version"), denies)?
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    })
}

/// Lists `candidate` first among `candidates` unless discovery already found it.
fn take(candidate: &EnvCandidate, candidates: &mut Vec<EnvCandidate>) {
    if !candidates.iter().any(|known| known.path == candidate.path) {
        candidates.insert(0, candidate.clone());
    }
}

/// What one root resolves to, as `(chosen, source, missing)`.
type Outcome = (Option<EnvCandidate>, Option<EnvSource>, Option<String>);

/// Settles an environment a selection or pin demands: chosen when it runs, else missing with
/// `absent` (no interpreter at all) or a broken-interpreter cause followed by `next`. An existing
/// one is listed among the candidates either way.
fn settle(
    (worktree, root, dir): (&Path, &Path, &Path),
    source: EnvSource,
    absent: String,
    next: &str,
    candidates: &mut Vec<EnvCandidate>,
    denies: &[ReadDeny],
) -> Outcome {
    let Some(runnable) = interpreter_state(dir, denies) else {
        return (None, None, Some(absent));
    };
    let found = candidate(worktree, root, dir, runnable, denies);
    take(&found, candidates);
    if found.broken {
        let cause = format!(
            "environment {} is broken (base interpreter gone) — {next}",
            found.label
        );
        (None, None, Some(cause))
    } else {
        (Some(found), Some(source), None)
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

/// Opaque identity of a resolution: the chosen environment's path and version plus everything
/// that changes when it is recreated in place (directory and interpreter inodes, the interpreter
/// link's target, `pyvenv.cfg` stamp and content), the root's native project inputs
/// (`native_inputs`) and, when nothing resolves, the missing cause. Why it won (`source`) is
/// left out: selecting the current winner changes nothing a consumer depends on. Appends
/// warnings for incomplete local requirement-reference coverage without changing selection.
fn identity(
    worktree: &Path,
    root: &Path,
    chosen: Option<&EnvCandidate>,
    missing: Option<&str>,
    denies: &[ReadDeny],
    warnings: &mut Vec<String>,
) -> String {
    // The native inputs belong to the root, not to the winner: a changed lock or config
    // invalidates every consumer of this resolution, chosen or missing.
    let native = native_inputs(worktree, root, denies, warnings);
    let Some(chosen) = chosen else {
        return format!("missing|{}|{native}", missing.unwrap_or_default());
    };
    let python = python_of(&chosen.path);
    let config = chosen.path.join("pyvenv.cfg");
    format!(
        "{}|{:?}|{}|{}|{:?}|{}|{}|{native}",
        chosen.path.display(),
        chosen.version,
        stamp(&chosen.path),
        stamp(&python),
        fs::read_link(&python).ok(),
        stamp(&config),
        blake3::hash(read_config(&config, denies).unwrap_or_default().as_bytes())
    )
}

/// Size of one chunk a native input is streamed through while being hashed: the identity costs
/// this much memory per file, never the file's size.
const NATIVE_INPUT_CHUNK: usize = 64 * 1024;

/// Maximum distinct requirement/constraint files, references per file and queued references
/// per resolution. Canonical visited identities prevent cycles from causing repeated reads.
const REQUIREMENT_FILES: usize = 64;

/// Maximum requirement content bytes per resolution, plus one EOF/budget probe byte; no
/// over-budget prefix is hashed as complete. Fixed manifests and lockfiles retain their
/// existing whole-content streaming.
const REQUIREMENT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum physical requirement line length, excluding LF; longer lines are still fully
/// hashed within the byte budget, but their reference coverage is explicitly unsupported.
const REQUIREMENT_LINE: usize = 8 * 1024;

/// Parses one physical requirement line into an optional local include path. Supports `-r`,
/// `-c`, `--requirement`, `--constraint`, attached short arguments, `=` separators and one
/// whole single/double-quoted path (spaces allowed), with an optional whitespace-led comment.
/// Returns a static coverage reason for malformed references, continuations, expansion,
/// escaped paths and URLs; never expands variables, fetches URLs or interprets package lines.
fn requirement_reference(line: &str) -> Result<Option<&str>, &'static str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    if line.ends_with('\\') {
        return Err("line continuations are unsupported");
    }
    let options = ["--requirement", "--constraint", "-r", "-c"];
    let argument = options
        .into_iter()
        .find_map(|option| line.strip_prefix(option).map(|argument| (option, argument)));
    let Some((option, argument)) = argument else {
        if line
            .split_whitespace()
            .take_while(|word| !word.starts_with('#'))
            .any(|word| options.iter().any(|option| word.starts_with(*option)))
        {
            return Err("references must be standalone directives");
        }
        return Ok(None);
    };
    if option.starts_with("--") && !argument.is_empty() && !argument.starts_with(['=', ' ', '\t']) {
        return Err("malformed local reference");
    }
    let argument = argument
        .trim_start()
        .strip_prefix('=')
        .unwrap_or(argument.trim_start())
        .trim_start();
    let (path, tail) = if argument.starts_with(['\'', '"']) {
        let quote = argument.as_bytes()[0] as char;
        let rest = &argument[1..];
        let end = rest.find(quote).ok_or("malformed local reference")?;
        (&rest[..end], &rest[end + 1..])
    } else {
        let end = argument.find(char::is_whitespace).unwrap_or(argument.len());
        (&argument[..end], &argument[end..])
    };
    if path.is_empty()
        || path.starts_with('-')
        || (!tail.is_empty()
            && (!tail.starts_with(char::is_whitespace) || !tail.trim_start().starts_with('#')))
    {
        return Err("malformed local reference");
    }
    if path.contains(['$', '~', '%', '\\', ':', '\0', '\'', '"']) {
        return Err("remote, expanded or escaped references are unsupported");
    }
    Ok(Some(path))
}

/// Resolves a requirement path inside the explicit worktree, checking lexical prefixes and
/// the canonical target against read exclusions before opening content. Refuses every link
/// below the worktree (including directory links), missing paths and lexical/resolved escapes;
/// the authoritative worktree itself may have a canonical alias such as macOS `/var`.
fn requirement_path(
    worktree: &Path,
    path: &Path,
    denies: &[ReadDeny],
) -> Result<PathBuf, &'static str> {
    if denies
        .iter()
        .any(|deny| deny.matches(path) || deny.matches(worktree))
    {
        return Err("local reference excluded from reads");
    }
    let relative = path
        .strip_prefix(worktree)
        .map_err(|_| "local reference escapes worktree")?;
    let mut checked = worktree.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => checked.push(name),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir if checked != worktree => {
                checked.pop();
            }
            _ => return Err("local reference escapes worktree"),
        }
        if denies.iter().any(|deny| deny.matches(&checked)) {
            return Err("local reference excluded from reads");
        }
        let metadata =
            fs::symlink_metadata(&checked).map_err(|_| "missing or unreadable local reference")?;
        if metadata.file_type().is_symlink() {
            return Err("symlinked local references are unsupported");
        }
    }
    let canonical =
        fs::canonicalize(&checked).map_err(|_| "missing or unreadable local reference")?;
    let boundary = fs::canonicalize(worktree).map_err(|_| "unavailable worktree boundary")?;
    if !canonical.starts_with(boundary) {
        return Err("local reference escapes worktree");
    }
    if denies.iter().any(|deny| deny.matches(&canonical)) {
        return Err("local reference excluded from reads");
    }
    Ok(canonical)
}

/// Opens an absolute, already-contained requirement path with `openat` and `O_NOFOLLOW`
/// on every component, so a directory replaced by a symlink after validation cannot redirect
/// the content read. Rejects non-normal components, I/O failures and non-regular final files;
/// descriptors are owned by `File` and closed on every success/failure path. This mirrors the
/// core source reader's descriptor walk, whose opener is private to that crate.
fn open_requirement(path: &Path) -> Result<fs::File, &'static str> {
    if !path.is_absolute() {
        return Err("local reference escapes worktree");
    }
    let mut directory = fs::File::open("/").map_err(|_| "unavailable worktree boundary")?;
    let mut components = path.components().skip(1).peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err("local reference escapes worktree");
        };
        let name =
            std::ffi::CString::new(name.as_bytes()).map_err(|_| "malformed local reference")?;
        let final_component = components.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if final_component {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        // SAFETY: `directory` owns a live descriptor and `name` is NUL-terminated for the
        // duration of this call. No pointer or borrowed descriptor escapes `openat`.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err("unreadable or symlinked local reference");
        }
        // SAFETY: successful openat returns a fresh owned descriptor; File takes exclusive
        // ownership and closes it, including when this assignment drops the previous one.
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    if !directory
        .metadata()
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err("local reference is not a regular file");
    }
    Ok(directory)
}

/// Streams a regular requirement file through a whole-content hash and bounded line parser.
/// Decrements the shared byte budget, returns discovered relative/absolute references and
/// records unsupported coverage reasons without including private line text. I/O or budget
/// failures return no hash, so a prefix is never certified as complete; references and line
/// storage are bounded independently. The caller has already checked containment and denies.
fn hash_requirements(
    path: &Path,
    remaining: &mut usize,
    reasons: &mut BTreeSet<&'static str>,
) -> Result<(blake3::Hash, Vec<PathBuf>), &'static str> {
    let mut file = open_requirement(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut chunk = vec![0u8; NATIVE_INPUT_CHUNK];
    let mut line = Vec::new();
    let mut references = Vec::new();
    let mut oversized = false;
    loop {
        let window = chunk.len().min(remaining.saturating_add(1));
        let read = file
            .read(&mut chunk[..window])
            .map_err(|_| "unreadable local reference")?;
        if read > *remaining {
            *remaining = 0;
            return Err("requirement byte budget exceeded (8 MiB)");
        }
        *remaining -= read;
        hasher.update(&chunk[..read]);
        // A synthetic LF at EOF handles a final line with no terminator, without hashing it.
        for byte in chunk[..read]
            .iter()
            .copied()
            .chain((read == 0).then_some(b'\n'))
        {
            if byte == b'\n' {
                if oversized {
                    reasons.insert("requirement line exceeds 8 KiB");
                } else {
                    match std::str::from_utf8(&line) {
                        Ok(line) => match requirement_reference(line) {
                            Ok(Some(reference)) if references.len() < REQUIREMENT_FILES => {
                                references.push(PathBuf::from(reference))
                            }
                            Ok(Some(_)) => {
                                reasons.insert("requirement reference budget exceeded (64)");
                            }
                            Ok(None) => {}
                            Err(reason) => {
                                reasons.insert(reason);
                            }
                        },
                        Err(_) => {
                            reasons.insert("non-UTF-8 requirement lines are unsupported");
                        }
                    }
                }
                line.clear();
                oversized = false;
            } else if line.len() < REQUIREMENT_LINE {
                line.push(byte);
            } else {
                oversized = true;
            }
        }
        if read == 0 {
            return Ok((hasher.finalize(), references));
        }
    }
}

/// Adds sorted root `requirements*.txt` and their local requirement/constraint closure to the
/// native fingerprint. Processes at most 64 canonical files and 64 queued references using
/// an iterative visited set, with an 8 MiB total read budget and 8 KiB physical lines.
/// Unsupported, denied, missing and over-budget coverage appends one explicit warning;
/// unreadable files contribute no content, preserving denied/absent input equivalence.
/// `ponytail:` this is local reference discovery, not pip syntax; extend the documented subset
/// only when needed, keeping unsupported scope visible and never installing dependencies.
fn requirement_inputs(
    worktree: &Path,
    root: &Path,
    denies: &[ReadDeny],
    parts: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let mut reasons = BTreeSet::new();
    let entries = requirement_path(worktree, root, denies)
        .and_then(|_| fs::read_dir(root).map_err(|_| "unreadable requirement directory"));
    let mut requirements: Vec<_> = match entries {
        Ok(entries) => entries
            .filter_map(|entry| match entry {
                Ok(entry) => match entry.file_name().into_string() {
                    Ok(name) => Some(name),
                    Err(_) => {
                        reasons.insert("non-UTF-8 requirement names are unsupported");
                        None
                    }
                },
                Err(_) => {
                    reasons.insert("unreadable requirement directory entry");
                    None
                }
            })
            .filter(|name| name.starts_with("requirements") && name.ends_with(".txt"))
            .take(REQUIREMENT_FILES + 1)
            .collect(),
        Err(reason) => {
            reasons.insert(reason);
            Vec::new()
        }
    };
    if requirements.len() > REQUIREMENT_FILES {
        reasons.insert("requirement file budget exceeded (64)");
        requirements.clear();
    }
    requirements.sort();
    let mut pending: VecDeque<_> = requirements
        .into_iter()
        .map(|name| root.join(name))
        .collect();
    let mut visited = HashSet::new();
    let mut remaining = REQUIREMENT_BYTES;
    while let Some(path) = pending.pop_front() {
        let canonical = match requirement_path(worktree, &path, denies) {
            Ok(path) => path,
            Err(reason) => {
                reasons.insert(reason);
                continue;
            }
        };
        if visited.contains(&canonical) {
            continue;
        }
        if visited.len() == REQUIREMENT_FILES {
            reasons.insert("requirement file budget exceeded (64)");
            break;
        }
        visited.insert(canonical.clone());
        match hash_requirements(&canonical, &mut remaining, &mut reasons) {
            Ok((hash, references)) => {
                parts.push(format!("{}={hash}", label(worktree, root, &path)));
                for reference in references {
                    if pending.len() == REQUIREMENT_FILES {
                        reasons.insert("requirement reference budget exceeded (64)");
                        break;
                    }
                    pending.push_back(path.parent().unwrap_or(root).join(reference));
                }
            }
            Err(reason) => {
                reasons.insert(reason);
                if remaining == 0 {
                    break;
                }
            }
        }
    }
    if !reasons.is_empty() {
        warnings.push(format!("Python requirements input coverage incomplete: {}; text fingerprints do not attest installed dependencies", reasons.into_iter().collect::<Vec<_>>().join("; ")));
    }
}

/// The native project inputs of `root` whose content feeds the resolver identity, in fixed
/// order: the manifest and pin files the resolver and `detect` already read (`pyproject.toml`,
/// `pyrightconfig.json`, every root `requirements*.txt` sorted by name and its bounded local
/// requirement/constraint closure), the dependency lockfiles (`uv.lock`, `poetry.lock`) and the version request
/// (`.python-version`, the root's own, then the worktree's, in `version_request`'s lookup
/// order, the worktree copy only when `root` differs from `worktree`).
///
/// Each entry contributes `<name>=<blake3 of the whole content>` — any changed byte, including
/// deep inside a large lockfile, invalidates the resolution; a missing or host-denied fixed
/// file contributes `<name>=-` — unreadable says nothing, exactly as `version_request`
/// treats a denied `.python-version`. An enumerated `requirements*.txt` that says nothing
/// contributes no entry at all, so an unreadable one fingerprints exactly like an absent one.
/// Hashing streams the file in `NATIVE_INPUT_CHUNK` chunks through the same gated open
/// `read_config` uses for fixed files — under denies, a denied or symlinked input is refused without being
/// followed; without denies, links are followed as `read_config`'s plain read follows them —
/// so no input is ever held in memory whole. Requirements use the stricter contained,
/// link-free reader described by `requirement_inputs`, which also appends coverage warnings.
fn native_inputs(
    worktree: &Path,
    root: &Path,
    denies: &[ReadDeny],
    warnings: &mut Vec<String>,
) -> String {
    /// One fingerprint entry: the whole-content hash, or `-` when the file says nothing.
    fn entry(name: &str, path: &Path, denies: &[ReadDeny]) -> String {
        match hash_config(path, denies) {
            Some(hash) => format!("{name}={hash}"),
            None => format!("{name}=-"),
        }
    }
    let mut parts = Vec::new();
    parts.push(entry("pyproject.toml", &root.join(PYPROJECT_FILE), denies));
    parts.push(entry(
        "pyrightconfig.json",
        &root.join(PYRIGHT_CONFIG_FILE),
        denies,
    ));
    requirement_inputs(worktree, root, denies, &mut parts, warnings);
    parts.push(entry("uv.lock", &root.join("uv.lock"), denies));
    parts.push(entry("poetry.lock", &root.join("poetry.lock"), denies));
    parts.push(entry(
        ".python-version",
        &root.join(".python-version"),
        denies,
    ));
    if root != worktree {
        parts.push(entry(
            "worktree/.python-version",
            &worktree.join(".python-version"),
            denies,
        ));
    }
    blake3::hash(parts.join("\n").as_bytes())
        .to_hex()
        .to_string()
}

/// Whole-content BLAKE3 of a native input, hashed with bounded memory: the file is gated
/// exactly like `read_config` gates a config — without denies a regular file is read as
/// usual, under denies a denied or symlinked input is refused without being followed
/// (`O_NOFOLLOW`) — then streamed in `NATIVE_INPUT_CHUNK` chunks straight into the hasher, so
/// no input is ever held in memory whole. `None` when the file says nothing: missing, denied or
/// otherwise unopenable. Any content, UTF-8 or not, is hashed as the bytes it is.
///
/// The gate mirrors `read_config`'s two branches (its `allowed_config` is private to
/// `checks`); the denied-input equivalence is pinned by the `denied_native_inputs_say_nothing`
/// test.
fn hash_config(path: &Path, denies: &[ReadDeny]) -> Option<blake3::Hash> {
    let mut file = if denies.is_empty() {
        if !path.is_file() {
            return None;
        }
        fs::File::open(path).ok()?
    } else {
        if denies.iter().any(|deny| deny.matches(path)) {
            return None;
        }
        if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return None;
        }
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?
    };
    let mut hasher = blake3::Hasher::new();
    let mut chunk = vec![0u8; NATIVE_INPUT_CHUNK];
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            return Some(hasher.finalize());
        }
        hasher.update(&chunk[..read]);
    }
}

/// One root's resolution, with whether a selection or pin governs it: an authoritative
/// environment that is missing must never be replaced by another interpreter.
#[derive(Clone, Debug)]
pub(crate) struct Resolution {
    /// The resolver's answer for the root.
    pub(crate) env: ResolvedEnv,
    /// A selection, a pin, or an unreadable (denied) Pyright config decides this root.
    pub(crate) authoritative: bool,
    /// The environment directory the selection or pin names, whether or not it exists.
    pub(crate) demanded: Option<PathBuf>,
}

/// Resolves the environment of the project `root` (absolute, inside `worktree`) with the agent's
/// `selector`, per the module's precedence, reading every file under the host's `denies`.
/// Incomplete requirement-reference coverage warns without overriding a pin, selection or
/// discovered interpreter; requirements only contribute text fingerprints, never versions.
pub(crate) fn resolve_with_denies(
    worktree: &Path,
    root: &Path,
    selector: Option<&str>,
    denies: &[ReadDeny],
) -> Resolution {
    let key = selector_key(worktree, root);
    let mut candidates = discovered(worktree, root, denies);
    let mut warnings = Vec::new();
    let mut authoritative = true;
    let mut demanded = None;
    let paths = (worktree, root);
    let (chosen, source, missing): Outcome = if [PYRIGHT_CONFIG_FILE, PYPROJECT_FILE]
        .iter()
        .any(|name| denies.iter().any(|deny| deny.matches(&root.join(name))))
    {
        let cause = format!(
            "{PYRIGHT_CONFIG_FILE} or {PYPROJECT_FILE} is excluded from reads by the host, so the environment cannot be resolved"
        );
        (None, None, Some(cause))
    } else if let Some(pin) = pin(root, denies) {
        if let Some(selector) = selector {
            warnings.push(format!(
                "selection {selector} ignored: {} pins venv \"{}\"",
                pin.file, pin.venv
            ));
        }
        let absent = format!(
            "{} pins venv \"{}\" ({}), which has no bin/python; candidates {}; create it, or edit venv there or remove the pin",
            pin.file,
            pin.venv,
            label(worktree, root, &pin.dir),
            labels(&candidates)
        );
        demanded = Some(pin.dir.clone());
        settle(
            (paths.0, paths.1, &pin.dir),
            EnvSource::Pin(pin.file.to_owned()),
            absent,
            "recreate it, or edit venv there or remove the pin",
            &mut candidates,
            denies,
        )
    } else if let Some(selector) = selector {
        let dir = selected_dir(root, selector, &candidates);
        let next = format!("recreate it or ide.start environment {{\"{key}\": \"auto\"}}");
        let absent = format!("environment {selector} missing (selected) — {next}");
        demanded = Some(dir.clone());
        settle(
            (paths.0, paths.1, &dir),
            EnvSource::Selected,
            absent,
            &next,
            &mut candidates,
            denies,
        )
    } else {
        authoritative = false;
        match candidates.iter().find(|candidate| !candidate.broken) {
            Some(first) => (Some(first.clone()), Some(EnvSource::Discovered), None),
            None => (
                None,
                None,
                Some(nothing_found(worktree, root, &key, &candidates)),
            ),
        }
    };
    if let (Some(chosen), Some(request)) = (&chosen, version_request(worktree, root, denies))
        && let Some(version) = &chosen.version
        && request.starts_with(|first: char| first.is_ascii_digit())
        && *version != request
        && !version.starts_with(&format!("{request}."))
    {
        warnings.push(format!("≠ .python-version {request}"));
    }
    let identity = identity(
        worktree,
        root,
        chosen.as_ref(),
        missing.as_deref(),
        denies,
        &mut warnings,
    );
    Resolution {
        env: ResolvedEnv {
            root: root
                .strip_prefix(worktree)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            chosen,
            source,
            candidates,
            warnings,
            missing_next_step: missing,
            identity,
        },
        authoritative,
        demanded,
    }
}

/// `resolve_with_denies` without host read denies.
pub fn resolve(worktree: &Path, root: &Path, selector: Option<&str>) -> ResolvedEnv {
    resolve_with_denies(worktree, root, selector, &[]).env
}

/// The project roots environments are resolved for: [`python_roots`], or the worktree alone
/// when it is a Python project only by an environment directory. Empty for a non-Python tree.
fn roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = python_roots(worktree);
    if roots.is_empty() && is_python_project(worktree) {
        roots.push(worktree.to_path_buf());
    }
    roots
}

/// The agent's stored selector for `root`, if any.
fn stored_selection(worktree: &Path, root: &Path) -> Option<String> {
    agent_ide_core::lang::environment::selections(worktree, crate::LANGUAGE)
        .into_iter()
        .find(|selection| worktree.join(&selection.root) == root)
        .map(|selection| selection.selector)
}

/// Resolves `root` with whatever the agent stored for it.
pub(crate) fn resolve_root(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Resolution {
    resolve_with_denies(
        worktree,
        root,
        stored_selection(worktree, root).as_deref(),
        denies,
    )
}

/// One [`Resolution`] per project root of `worktree`, honouring stored selections.
pub(crate) fn resolutions(worktree: &Path, denies: &[ReadDeny]) -> Vec<Resolution> {
    roots(worktree)
        .iter()
        .map(|root| resolve_root(worktree, root, denies))
        .collect()
}

/// One [`ResolvedEnv`] per project root of `worktree`, honouring stored selections.
pub(crate) fn environments(worktree: &Path, denies: &[ReadDeny]) -> Vec<ResolvedEnv> {
    resolutions(worktree, denies)
        .into_iter()
        .map(|resolution| resolution.env)
        .collect()
}

/// The resolution that speaks for the whole worktree: the first root (the worktree itself sorts
/// first) that chose an environment or whose selection or pin governs it, so a missing
/// authoritative environment is never papered over by another root's. `ponytail:` one
/// interpreter per session, because Pyright has no per-root interpreter; one session per
/// interpreter if monorepos diverge.
pub(crate) fn governing(worktree: &Path) -> Option<Resolution> {
    resolutions(worktree, &[])
        .into_iter()
        .find(|resolution| resolution.env.chosen.is_some() || resolution.authoritative)
}

/// The resolution of the deepest project root containing `path` (absolute, or relative to
/// `worktree`), else the [`governing`] one.
pub(crate) fn for_path(worktree: &Path, path: &Path) -> Option<Resolution> {
    let path = worktree.join(path);
    resolutions(worktree, &[])
        .into_iter()
        .filter(|resolution| path.starts_with(absolute_root(worktree, &resolution.env)))
        .max_by_key(|resolution| resolution.env.root.components().count())
        .or_else(|| governing(worktree))
}

/// The roots a run of `worktree`'s currently chosen Python environments reads: each selected
/// interpreter's invocation prefix and the installation prefix the checker derives for that
/// interpreter. Empty read-deny policies use filesystem canonicalization; nonempty policies use
/// the checker's deny-safe link resolver. A missing or unprovable canonical target contributes no
/// roots; the recipe's separately admitted environment root remains independent.
pub fn accepted_roots(worktree: &Path, config: &ModuleConfig) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for env in environments(worktree, &config.read_denies) {
        if env.missing_next_step.is_some() {
            continue;
        }
        let Some(interpreter) = interpreter(&env) else {
            continue;
        };
        let canonical = if config.read_denies.is_empty() {
            fs::canonicalize(&interpreter).ok()
        } else {
            crate::checks::resolved_link_target(&interpreter, &config.read_denies)
        };
        let Some(canonical) = canonical else {
            continue;
        };
        for path in [&interpreter, &canonical] {
            let prefix = crate::checks::grandparent_or_self(path);
            if !roots.contains(&prefix) {
                roots.push(prefix);
            }
        }
    }
    roots
}

/// The session's interpreter and the identity a live session is compared against.
pub(crate) fn session(worktree: &Path) -> (Option<PathBuf>, String) {
    governing(worktree).map_or_else(
        || (None, "none".to_owned()),
        |resolution| (interpreter(&resolution.env), resolution.env.identity),
    )
}

/// Accepts `selector` for the project root `root` (relative to `worktree`): `auto`, a candidate
/// label, or a path to an environment directory; refuses an unknown root, a Pyright pin in force
/// and an unknown selector, each with the way out.
pub(crate) fn check_selection(worktree: &Path, root: &Path, selector: &str) -> Result<(), String> {
    let absolute = worktree.join(root);
    let known = roots(worktree);
    if !known.contains(&absolute) {
        let names: Vec<String> = known
            .iter()
            .map(|known| match label(worktree, worktree, known) {
                name if name.is_empty() => ".".to_owned(),
                name => name,
            })
            .collect();
        return Err(format!(
            "{} is not a Python project root; roots {}",
            root.display(),
            names.join(", ")
        ));
    }
    if selector == "auto" {
        return Ok(());
    }
    if let Some(pin) = pin(&absolute, &[]) {
        return Err(format!(
            "{} pins venv \"{}\" — selection ignored; edit venv there or remove the pin",
            pin.file, pin.venv
        ));
    }
    let candidates = discovered(worktree, &absolute, &[]);
    match interpreter_state(&selected_dir(&absolute, selector, &candidates), &[]) {
        Some(true) => Ok(()),
        Some(false) => Err(format!(
            "\"{selector}\" is broken (base interpreter gone); candidates {}",
            labels(&candidates)
        )),
        None => Err(format!(
            "\"{selector}\" not found; candidates {}",
            labels(&candidates)
        )),
    }
}

/// A root's missing-environment cause and next step, prefixed with the root when it is nested.
pub(crate) fn root_detail(env: &ResolvedEnv) -> Option<String> {
    env.missing_next_step.as_ref().map(|step| {
        if env.root.as_os_str().is_empty() {
            step.clone()
        } else {
            format!("{}: {step}", env.root.display())
        }
    })
}

/// Absent demanded interpreter → the resolver's cause and next step for it.
type MissingSteps = HashMap<PathBuf, String>;

/// The resolver's way out for each demanded interpreter `detect` found absent. A
/// `LanguageProject` carries that interpreter path but no worktree, so test and format commands
/// built from it look the text up here instead of guessing whether a pin or a selection (and
/// which `python:<root>` key) is in force. Entries are a few short strings per worktree.
static MISSING_STEPS: LazyLock<Mutex<MissingSteps>> = LazyLock::new(Mutex::default);

/// Records `step` as the way out for the absent demanded interpreter `python`.
pub(crate) fn remember_missing(python: &Path, step: String) {
    MISSING_STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(python.to_path_buf(), step);
}

/// The way out recorded for the absent demanded interpreter `python`, if `detect` saw it.
pub(crate) fn missing_step(python: &Path) -> Option<String> {
    MISSING_STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(python)
        .cloned()
}

/// The absolute project root of `env`: the worktree itself (without a trailing separator) for
/// the worktree's own root.
pub(crate) fn absolute_root(worktree: &Path, env: &ResolvedEnv) -> PathBuf {
    if env.root.as_os_str().is_empty() {
        worktree.to_path_buf()
    } else {
        worktree.join(&env.root)
    }
}

/// Whether the environment's `bin` holds `program` as an executable file: a bare name
/// (`pytest`), or an absolute path into that `bin` (the language's own commands).
fn owns(bin: &Path, program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let tool = Path::new(program);
    let path = match tool.parent() {
        Some(parent) if parent == bin => tool.to_path_buf(),
        Some(parent) if parent.as_os_str().is_empty() => bin.join(tool),
        _ => return false,
    };
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Activation of the resolved environment of the deepest project root containing `cwd`, for
/// programs that environment owns only: a bare `python`/`python3` is replaced by the
/// environment's interpreter, and a tool installed in its `bin` (`pytest`, `ruff`) runs with that
/// `bin` first on `PATH` and `VIRTUAL_ENV` set. `None` for every other program (`cargo`, `make`),
/// so another language's toolchain is never taken over, and when that root resolves nothing.
pub(crate) fn command_env(worktree: &Path, cwd: &Path, program: &str) -> Option<CommandEnv> {
    let venv = for_path(worktree, cwd)?.env.chosen?.path;
    let bin = venv.join("bin");
    let argv_prefix = if matches!(program, "python" | "python3") {
        vec![python_of(&venv).into_os_string()]
    } else if owns(&bin, program) {
        Vec::new()
    } else {
        return None;
    };
    Some(CommandEnv {
        argv_prefix,
        path_prefix: Some(bin),
        vars: vec![(OsString::from("VIRTUAL_ENV"), venv.into_os_string())],
    })
}

#[cfg(test)]
mod tests {
    //! The resolver's precedence table and its agreement with every consumer (environment
    //! selection design §5.6).

    use std::sync::Arc;
    use std::time::Duration;

    use agent_ide_core::checks::runner::{ConfinedRunner, FakeRunner, RunOutput};
    use agent_ide_core::checks::{
        CheckRequest, CheckState, Checker, LanguageChecks, UnavailableReason,
    };
    use agent_ide_core::lang::environment::{EnvSelection, replace_selections};
    use agent_ide_core::lang::{LangError, LanguageSupport, TestTarget};

    use super::*;
    use crate::checks::{PythonChecker, PythonChecks};
    use crate::support::Python;

    /// Fresh scratch directory for one fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-python-env-{name}-{}",
            std::process::id()
        ));
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

    /// Creates the environment `name` under `root` with `bin/python` and, unless empty, the
    /// given `pyvenv.cfg` text.
    fn venv(root: &Path, name: &str, config: &str) {
        put(root, &format!("{name}/bin/python"), "");
        if !config.is_empty() {
            put(root, &format!("{name}/pyvenv.cfg"), config);
        }
    }

    /// Stores the agent's selection `selector` for the project root `root` of `worktree`.
    fn select(worktree: &Path, root: &str, selector: &str) {
        replace_selections(
            worktree,
            crate::LANGUAGE,
            vec![EnvSelection {
                root: PathBuf::from(root),
                selector: selector.to_owned(),
            }],
        );
    }

    /// The chosen environment's label, if any.
    fn chosen(env: &ResolvedEnv) -> Option<&str> {
        env.chosen.as_ref().map(|chosen| chosen.label.as_str())
    }

    /// A checker over placeholder tools that exist (so it gets past its tool check) and a fake
    /// runner answering one clean report per run.
    fn checker(root: &Path, runs: usize) -> (PythonChecker, Arc<FakeRunner>) {
        put(root, "tools/node", "");
        put(root, "tools/pyright.js", "");
        let clean = br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 1}}"#;
        let fake = Arc::new(FakeRunner::new(
            (0..runs)
                .map(|_| {
                    Ok(RunOutput {
                        status: Some(0),
                        stdout: clean.to_vec(),
                        ..RunOutput::default()
                    })
                })
                .collect(),
        ));
        let runner: Arc<dyn ConfinedRunner> = fake.clone();
        (
            PythonChecker::new(
                runner,
                root.join("tools/node"),
                root.join("tools/pyright.js"),
                Duration::from_secs(60),
            ),
            fake,
        )
    }

    /// One check request for `worktree` with a scratch cache directory.
    fn request(worktree: &Path, cache: &Path) -> CheckRequest {
        CheckRequest {
            worktree: worktree.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            input_generation: 1,
            read_denies: Vec::new(),
        }
    }

    /// One case per precedence step: suffixed discovery sorted by name, `venv` over suffixed,
    /// `.venv` over `venv`, a selection over discovery, `[tool.pyright]` over a selection,
    /// `pyrightconfig.json` over `[tool.pyright]`, and nothing at all.
    #[test]
    fn precedence_runs_pin_then_selection_then_discovery() {
        let root = scratch("precedence");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv-b", "");
        venv(&root, "venv-a", "");
        venv(&root, ".venv-a", "");
        let env = resolve(&root, &root, None);
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-a"), Some(EnvSource::Discovered)),
            "suffixed candidates sort by name"
        );
        venv(&root, "venv", "");
        assert_eq!(chosen(&resolve(&root, &root, None)), Some("venv"));
        venv(&root, ".venv", "");
        let env = resolve(&root, &root, None);
        assert_eq!(chosen(&env), Some(".venv"));
        let order: Vec<&str> = env
            .candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(order, [".venv", "venv", ".venv-a", ".venv-b", "venv-a"]);

        let env = resolve(&root, &root, Some(".venv-b"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-b"), Some(EnvSource::Selected))
        );

        put(
            &root,
            "pyproject.toml",
            "[project]\nname = \"svc\"\n\n[tool.pyright]\nvenvPath = \".\"\nvenv = \"venv\"\n",
        );
        let env = resolve(&root, &root, Some(".venv-b"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (
                Some("venv"),
                Some(EnvSource::Pin("pyproject.toml".to_owned()))
            )
        );
        assert_eq!(
            env.warnings,
            ["selection .venv-b ignored: pyproject.toml pins venv \"venv\""]
        );

        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-a"}"#,
        );
        let env = resolve(&root, &root, None);
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (
                Some(".venv-a"),
                Some(EnvSource::Pin("pyrightconfig.json".to_owned()))
            )
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("precedence-none");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        let env = resolve(&root, &root, None);
        assert_eq!((env.chosen.clone(), env.source.clone()), (None, None));
        assert_eq!(
            env.missing_next_step.as_deref(),
            Some(
                "no .venv* beside pyproject.toml; create one (uv venv) or ide.start environment {\"python\": \"<path>\"}"
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Versions come from `pyvenv.cfg` `version`, else `version_info` (trimmed to `X.Y.Z`),
    /// else none, which is not broken; a missing `home` directory is.
    #[test]
    fn pyvenv_cfg_supplies_version_and_broken() {
        let root = scratch("pyvenv");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "home = /usr/bin\nversion = 3.12.7\n");
        venv(
            &root,
            ".venv-uv",
            "home = /nonexistent-agent-ide-home/bin\nversion_info = 3.14.0\n",
        );
        venv(&root, ".venv-virtualenv", "version_info = 3.12.7.final.0\n");
        venv(&root, ".venv-bare", "");
        let env = resolve(&root, &root, None);
        let described: Vec<(&str, Option<&str>, bool)> = env
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.label.as_str(),
                    candidate.version.as_deref(),
                    candidate.broken,
                )
            })
            .collect();
        assert_eq!(
            described,
            [
                (".venv", Some("3.12.7"), false),
                (".venv-bare", None, false),
                (".venv-uv", Some("3.14.0"), true),
                (".venv-virtualenv", Some("3.12.7"), false),
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Failure (a): a pin naming an absent environment resolves to nothing — never a fallback
    /// to `.venv-py314` — and the card, the resolver and the check report the same cause.
    #[tokio::test]
    async fn pinned_missing_environment_is_env_missing_everywhere() {
        let root = scratch("pin-missing");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
        );
        venv(&root, ".venv-py314", "");
        let envs = environments(&root, &[]);
        assert_eq!(envs.len(), 1);
        assert_eq!(chosen(&envs[0]), None);
        let step = envs[0].missing_next_step.clone().unwrap();
        assert_eq!(
            step,
            "pyrightconfig.json pins venv \".venv-gone\" (.venv-gone), which has no bin/python; candidates .venv-py314; create it, or edit venv there or remove the pin"
        );
        assert_eq!(Python.detect(&root).unwrap().commands.test, None);
        assert_eq!(crate::checks::session_interpreter(&root), None);

        let (checker, fake) = checker(&root, 0);
        let snapshot = checker.check(request(&root, &root.join("cache"))).await;
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::EnvMissing)
        );
        assert_eq!(snapshot.detail, Some(step));
        assert!(fake.specs().is_empty(), "pyright never runs without an env");
        fs::remove_dir_all(&root).unwrap();
    }

    /// Failure (b): a worktree whose only Python marker is `.venv-py314` is a Python project for
    /// the card and the checks alike; a suffixed directory without `bin/python` is not.
    #[test]
    fn suffixed_environment_alone_marks_a_python_project() {
        let root = scratch("suffixed-only");
        venv(&root, ".venv-py314", "");
        assert!(PythonChecks.is_present(&root));
        assert_eq!(
            Python.detect(&root).unwrap().interpreter,
            Some(root.join(".venv-py314/bin/python"))
        );
        let envs = environments(&root, &[]);
        assert_eq!(envs.len(), 1);
        assert_eq!(chosen(&envs[0]), Some(".venv-py314"));
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("suffixed-empty");
        put(&root, ".venv-py314/lib/readme", "");
        assert!(!PythonChecks.is_present(&root));
        assert!(environments(&root, &[]).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// One resolver: the card's interpreter, the probe's program, the session interpreter and
    /// the check's `--pythonpath` are the same path on discovery, selection, pin and nested
    /// fallback fixtures.
    #[tokio::test]
    async fn card_check_session_and_probe_agree() {
        type Setup = fn(&Path);
        let cases: [(&str, Setup, &str); 4] = [
            (
                "agree-discovered",
                |root| {
                    put(root, "pyproject.toml", "");
                    venv(root, ".venv-py314", "");
                    venv(root, ".venv", "");
                },
                ".venv",
            ),
            (
                "agree-selected",
                |root| {
                    put(root, "pyproject.toml", "");
                    venv(root, ".venv-py314", "");
                    venv(root, ".venv", "");
                    select(root, "", ".venv-py314");
                },
                ".venv-py314",
            ),
            (
                "agree-pinned",
                |root| {
                    put(
                        root,
                        "pyproject.toml",
                        "[tool.pyright]\nvenvPath = \"envs\"\nvenv = \"main\"\n",
                    );
                    venv(root, ".venv", "");
                    venv(root, "envs/main", "");
                },
                "envs/main",
            ),
            (
                "agree-nested",
                |root| {
                    venv(root, ".venv", "");
                    put(root, "svc/pyproject.toml", "");
                    put(root, "svc/app.py", "");
                },
                ".venv",
            ),
        ];
        for (name, setup, expected) in cases {
            let root = scratch(name);
            setup(&root);
            let python = root.join(expected).join("bin/python");
            let project = Python.detect(&root).unwrap();
            assert_eq!(project.interpreter.as_ref(), Some(&python), "{name}: card");
            assert_eq!(
                Python
                    .syntax_probe_command(&project, &root, Path::new("a.py"), None)
                    .unwrap()[0],
                python.display().to_string(),
                "{name}: probe"
            );
            assert_eq!(
                crate::checks::session_interpreter(&root).as_ref(),
                Some(&python),
                "{name}: session"
            );
            let (checker, fake) = checker(&root, 1);
            checker.check(request(&root, &root.join("cache"))).await;
            let specs = fake.specs();
            assert_eq!(specs.len(), 1, "{name}: one root checked");
            let at = specs[0]
                .args
                .iter()
                .position(|arg| arg == "--pythonpath")
                .unwrap();
            assert_eq!(specs[0].args[at + 1], python.as_os_str(), "{name}: check");
            replace_selections(&root, crate::LANGUAGE, Vec::new());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// `.python-version` is a version request: a mismatch warns and never switches the env.
    #[test]
    fn python_version_mismatch_warns_without_switching() {
        let root = scratch("python-version");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "version = 3.12.7\n");
        venv(&root, ".venv-py314", "version_info = 3.14.0\n");
        put(&root, ".python-version", "3.14\n");
        let env = resolve(&root, &root, None);
        assert_eq!(chosen(&env), Some(".venv"));
        assert_eq!(env.warnings, ["≠ .python-version 3.14"]);
        put(&root, ".python-version", "3.12\n");
        assert!(resolve(&root, &root, None).warnings.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// A stored selection is honoured by every consumer, cleared by an empty list, and a
    /// selected environment that disappears is missing with its way back — never a fallback.
    #[test]
    fn stored_selection_is_honoured_and_its_disappearance_reported() {
        let root = scratch("stored-selection");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        select(&root, "", ".venv-py314");
        let envs = Python.environments(&root);
        assert_eq!(
            (chosen(&envs[0]), envs[0].source.clone()),
            (Some(".venv-py314"), Some(EnvSource::Selected))
        );
        assert_eq!(
            Python.detect(&root).unwrap().interpreter,
            Some(root.join(".venv-py314/bin/python"))
        );
        fs::remove_dir_all(root.join(".venv-py314")).unwrap();
        let env = &Python.environments(&root)[0];
        assert_eq!(chosen(env), None);
        assert_eq!(
            env.missing_next_step.as_deref(),
            Some(
                "environment .venv-py314 missing (selected) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        assert_eq!(chosen(&Python.environments(&root)[0]), Some(".venv"));
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("stored-selection-nested");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        select(&root, "packages/alpha", "gone");
        assert_eq!(
            Python.environments(&root)[0].missing_next_step.as_deref(),
            Some(
                "environment gone missing (selected) — recreate it or ide.start environment {\"python:packages/alpha\": \"auto\"}"
            )
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Accepts a label, `auto`, a relative or absolute path to an environment; refuses an
    /// unknown selector with the candidates, an unknown root, and any selection under a pin.
    #[test]
    fn check_selection_accepts_and_refuses() {
        let root = scratch("check-selection");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        let outside = scratch("check-selection-outside");
        venv(&outside, "env", "");
        let check = |selector: &str| Python.check_selection(&root, Path::new(""), selector);
        assert_eq!(check(".venv-py314"), Ok(()));
        assert_eq!(check("auto"), Ok(()));
        assert_eq!(check("./.venv-py314"), Ok(()));
        assert_eq!(check(&outside.join("env").display().to_string()), Ok(()));
        assert_eq!(
            check(".venv-py9"),
            Err("\".venv-py9\" not found; candidates .venv, .venv-py314".to_owned())
        );
        assert!(
            Python
                .check_selection(&root, Path::new("nope"), ".venv")
                .unwrap_err()
                .starts_with("nope is not a Python project root; roots .")
        );
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv"}"#,
        );
        assert_eq!(
            check(".venv-py314"),
            Err(
                "pyrightconfig.json pins venv \".venv\" — selection ignored; edit venv there or remove the pin"
                    .to_owned()
            )
        );
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    /// Accepted roots follow only the chosen interpreter and honor the current read-deny policy.
    #[test]
    fn accepted_roots_follow_the_interpreter_link() {
        let root = scratch("accepted-roots");
        let base = fs::canonicalize(scratch("accepted-roots-base")).unwrap();
        let base_alias = root.join("python-base-alias");
        let unrelated = scratch("accepted-roots-unrelated");
        put(&base, "bin/python3.14", "");
        put(&unrelated, "bin/python3.14", "");
        std::os::unix::fs::symlink(&base, &base_alias).unwrap();
        put(&root, "pyproject.toml", "");
        for name in [".venv", ".venv-alt"] {
            fs::create_dir_all(root.join(name).join("bin")).unwrap();
            put(&root, &format!("{name}/pyvenv.cfg"), "version = 3.14.0\n");
        }
        std::os::unix::fs::symlink(
            base_alias.join("bin/python3.14"),
            root.join(".venv/bin/python"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            unrelated.join("bin/python3.14"),
            root.join(".venv-alt/bin/python"),
        )
        .unwrap();
        select(&root, "", ".venv");
        let config = ModuleConfig::default();
        assert_eq!(
            accepted_roots(&root, &config),
            vec![root.join(".venv"), base.clone()]
        );
        let denied = ModuleConfig {
            read_denies: vec![ReadDeny::Path(base.clone())],
            ..ModuleConfig::default()
        };
        // A nonempty deny policy keeps symlinked parents fail-closed, matching the checker.
        assert!(accepted_roots(&root, &denied).is_empty());
        let unrelated_deny = ModuleConfig {
            read_denies: vec![ReadDeny::Path(fs::canonicalize(&unrelated).unwrap())],
            ..ModuleConfig::default()
        };
        assert!(accepted_roots(&root, &unrelated_deny).is_empty());

        fs::remove_file(root.join(".venv/bin/python")).unwrap();
        std::os::unix::fs::symlink(
            unrelated.join("bin/python3.14"),
            root.join(".venv/bin/python"),
        )
        .unwrap();
        let denied_changed_target = ModuleConfig {
            read_denies: vec![ReadDeny::Path(fs::canonicalize(&unrelated).unwrap())],
            ..ModuleConfig::default()
        };
        assert!(accepted_roots(&root, &denied_changed_target).is_empty());

        fs::remove_dir_all(root.join(".venv")).unwrap();
        // The explicit selection is missing; a different discovered environment is not a fallback.
        assert!(accepted_roots(&root, &config).is_empty());
        fs::remove_dir_all(root.join(".venv-alt")).unwrap();
        agent_ide_core::lang::environment::replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&base).unwrap();
        fs::remove_dir_all(&unrelated).unwrap();
    }

    /// Explicit commands run activated in the environment of the deepest root holding `cwd`,
    /// for the programs that environment owns only: `python` is replaced by the interpreter, an
    /// installed tool gets the environment's `bin` and `VIRTUAL_ENV`, and an uninstalled tool or
    /// another language's program (`cargo`) is left alone.
    #[test]
    fn command_env_activates_only_programs_the_environment_owns() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("command-env");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        let alpha = root.join("packages/alpha/.venv-a");
        let pytest = alpha.join("bin/pytest");
        put(&root, "packages/alpha/.venv-a/bin/pytest", "");
        fs::set_permissions(&pytest, fs::Permissions::from_mode(0o755)).unwrap();
        let activated = CommandEnv {
            argv_prefix: Vec::new(),
            path_prefix: Some(alpha.join("bin")),
            vars: vec![(
                OsString::from("VIRTUAL_ENV"),
                alpha.clone().into_os_string(),
            )],
        };

        let env = Python
            .command_env(&root, &root.join("packages/alpha/tests"), "python3")
            .unwrap();
        assert_eq!(
            env,
            CommandEnv {
                argv_prefix: vec![alpha.join("bin/python").into_os_string()],
                ..activated.clone()
            }
        );
        assert_eq!(
            Python.command_env(&root, Path::new("packages/alpha"), "pytest"),
            Some(activated.clone()),
            "an installed tool runs activated"
        );
        assert_eq!(
            Python.command_env(
                &root,
                Path::new("packages/alpha"),
                &pytest.display().to_string()
            ),
            Some(activated),
            "the language's own absolute command runs activated"
        );
        assert_eq!(
            Python.command_env(&root, Path::new("packages/alpha"), "ruff"),
            None,
            "a tool the environment lacks is not taken over"
        );
        assert_eq!(
            Python.command_env(&root, &root, "cargo"),
            None,
            "another language's program is never taken over"
        );
        assert_eq!(Python.command_env(&root, &root, "/usr/bin/make"), None);
        assert_eq!(
            Python
                .command_env(&root, &root, "python")
                .unwrap()
                .path_prefix,
            Some(root.join(".venv/bin"))
        );
        fs::remove_dir_all(root.join(".venv")).unwrap();
        assert_eq!(Python.command_env(&root, &root, "python"), None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Real Pyright: `.venv` lacks `stub_package`, `.venv-py314` provides it. Before the
    /// selection the project check reports the import unresolved; after selecting
    /// `.venv-py314` the same check resolves it. Needs `AGENT_IDE_PYRIGHT`, `AGENT_IDE_NODE` and
    /// `AGENT_IDE_PYTHON` like the product tests. Runs the public package `index.js` entry so its
    /// bootstrap initializes the bundled typeshed root.
    #[tokio::test]
    #[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
    async fn real_pyright_check_follows_the_selected_environment() {
        use agent_ide_core::checks::runner::{NestedSandboxFallbackRunner, SeatbeltRunner};

        let python = PathBuf::from(std::env::var_os("AGENT_IDE_PYTHON").unwrap());
        let node = PathBuf::from(std::env::var_os("AGENT_IDE_NODE").unwrap());
        let pyright = PathBuf::from(std::env::var_os("AGENT_IDE_PYRIGHT").unwrap());
        let cli = pyright
            .parent()
            .and_then(Path::parent)
            .map(|prefix| prefix.join("lib/node_modules/pyright/index.js"))
            .map(|cli| fs::canonicalize(&cli).unwrap_or(cli))
            .unwrap();
        let root = fs::canonicalize(scratch("real-selection")).unwrap();
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(&root, "src/app.py", "from stub_package import VALUE\n");
        for name in [".venv", ".venv-py314"] {
            let interpreter = root.join(name).join("bin/python");
            fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&python, &interpreter).unwrap();
            put(
                &root,
                &format!("{name}/pyvenv.cfg"),
                &format!(
                    "home = {}\ninclude-system-site-packages = false\nversion = 3.14.0\n",
                    python.parent().unwrap().display()
                ),
            );
        }
        put(
            &root,
            ".venv-py314/lib/python3.14/site-packages/stub_package/__init__.py",
            "VALUE: int = 42\n",
        );
        let runner: Arc<dyn ConfinedRunner> =
            Arc::new(NestedSandboxFallbackRunner::new(Arc::new(SeatbeltRunner)));
        let checker = PythonChecker::new(runner, node, cli, Duration::from_secs(120));
        let unresolved = |snapshot: &agent_ide_core::checks::ProblemSnapshot| {
            assert!(
                matches!(snapshot.state, CheckState::Ready | CheckState::Partial),
                "{snapshot:?}"
            );
            snapshot.problems.iter().any(|problem| {
                problem.path == "src/app.py"
                    && problem.code.as_deref() == Some("reportMissingImports")
            })
        };

        let before = checker.check(request(&root, &root.join("cache"))).await;
        assert!(unresolved(&before), "{before:?}");
        select(&root, "", ".venv-py314");
        let after = checker.check(request(&root, &root.join("cache"))).await;
        assert!(!unresolved(&after), "{after:?}");
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix A: a selected or pinned environment that is missing never lets another interpreter
    /// run: no card command, a test error with the way out, no formatter and no probe.
    #[test]
    fn demanded_missing_environment_never_runs_another_interpreter() {
        for (name, pinned) in [("demanded-selected", false), ("demanded-pinned", true)] {
            let root = scratch(name);
            put(&root, "pyproject.toml", "[tool.ruff]\nline-length = 100\n");
            put(&root, "uv.lock", "");
            venv(&root, ".venv", "");
            if pinned {
                put(
                    &root,
                    "pyrightconfig.json",
                    r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
                );
            } else {
                select(&root, "", ".venv-gone");
            }
            let project = Python.detect(&root).unwrap();
            assert_eq!(project.commands.test, None, "{name}");
            assert_eq!(project.commands.format, None, "{name}");
            let error = Python
                .test_selection(&project, &TestTarget::Pattern("x".to_owned()))
                .unwrap_err();
            let step = environments(&root, &[])[0]
                .missing_next_step
                .clone()
                .unwrap();
            assert_eq!(error, LangError::Unsupported(step), "{name}");
            assert_eq!(Python.format_command(&project, Path::new("a.py")), None);
            assert_eq!(
                Python.format_stdin_command(&project, Path::new("a.py")),
                None
            );
            assert_eq!(
                Python.syntax_probe_command(&project, &root, Path::new("a.py"), None),
                None,
                "{name}"
            );
            replace_selections(&root, crate::LANGUAGE, Vec::new());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// Fix B: the worktree root's missing selection is reported by the check and leaves the
    /// session without an interpreter, although a nested root's environment works.
    #[tokio::test]
    async fn missing_worktree_selection_is_not_hidden_by_a_nested_root() {
        let root = scratch("hidden-by-nested");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        select(&root, "", ".venv-gone");
        assert_eq!(crate::checks::session_interpreter(&root), None);
        let (checker, fake) = checker(&root, 2);
        let snapshot = checker.check(request(&root, &root.join("cache"))).await;
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::EnvMissing)
        );
        assert_eq!(
            snapshot.detail.as_deref(),
            Some(
                "environment .venv-gone missing (selected) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        assert!(fake.specs().is_empty());
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix C (probe): the syntax probe runs the interpreter of the root holding the file, a
    /// nested root's selection included.
    #[test]
    fn probe_uses_the_environment_of_the_files_root() {
        let root = scratch("probe-per-root");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        venv(&root, "packages/alpha/.venv-b", "");
        select(&root, "packages/alpha", ".venv-b");
        let project = Python.detect(&root).unwrap();
        let probe = |file: &str| {
            Python
                .syntax_probe_command(&project, &root, Path::new(file), None)
                .unwrap()
                .remove(0)
        };
        assert_eq!(
            probe("packages/alpha/a.py"),
            root.join("packages/alpha/.venv-b/bin/python")
                .display()
                .to_string()
        );
        assert_eq!(
            probe("src/x.py"),
            root.join(".venv/bin/python").display().to_string()
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix D: an existing `pyrightconfig.json` without `venv` keys pins nothing, even when
    /// `[tool.pyright]` names a venv (Pyright never reads it then), so a selection is allowed.
    #[test]
    fn pyrightconfig_without_venv_keys_overrides_the_pyproject_pin() {
        let root = scratch("json-wins");
        put(
            &root,
            "pyproject.toml",
            "[tool.pyright]\nvenvPath = \".\"\nvenv = \".venv\"\n",
        );
        put(&root, "pyrightconfig.json", r#"{"strict": ["src"]}"#);
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        assert_eq!(
            Python.check_selection(&root, Path::new(""), ".venv-py314"),
            Ok(())
        );
        let env = resolve(&root, &root, Some(".venv-py314"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-py314"), Some(EnvSource::Selected))
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix E: `.python-version` and `pyvenv.cfg` follow the host's read denies; denied files
    /// are unreadable and their contents never reach a warning or a version.
    #[test]
    fn read_denies_hide_python_version_and_pyvenv_cfg() {
        let root = fs::canonicalize(scratch("denied-files")).unwrap();
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "version = 3.12.7\n");
        put(&root, ".python-version", "3.99\n");
        assert_eq!(
            resolve(&root, &root, None).warnings,
            ["≠ .python-version 3.99"]
        );
        let env = resolve_with_denies(
            &root,
            &root,
            None,
            &[ReadDeny::Path(root.join(".python-version"))],
        )
        .env;
        assert_eq!(chosen(&env), Some(".venv"));
        assert!(env.warnings.is_empty(), "{:?}", env.warnings);
        assert_eq!(env.chosen.unwrap().version.as_deref(), Some("3.12.7"));
        let env = resolve_with_denies(
            &root,
            &root,
            None,
            &[ReadDeny::Path(root.join(".venv/pyvenv.cfg"))],
        )
        .env;
        assert_eq!(env.chosen.unwrap().version, None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix F: selecting the current winner keeps the identity; recreating the environment at
    /// the same path changes it.
    #[test]
    fn identity_ignores_source_and_tracks_recreation() {
        let root = scratch("identity");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        let discovered = resolve(&root, &root, None).identity;
        assert_eq!(resolve(&root, &root, Some(".venv")).identity, discovered);
        fs::remove_dir_all(root.join(".venv")).unwrap();
        venv(&root, ".venv", "");
        assert_ne!(resolve(&root, &root, None).identity, discovered);
        fs::remove_dir_all(&root).unwrap();
    }

    /// The native project inputs feed the identity: changing, adding or removing exactly one
    /// input — the manifest, either lockfile, the pin file, a requirements file or the version
    /// request — invalidates the resolution even though the chosen environment itself is
    /// byte-identical (a changed lock changes declared dependency inputs), so checks and
    /// sessions must not ride on the old answer. Each mutation is compared against the identity
    /// captured immediately before it, so only the named input can explain the change. A nested
    /// root also covers its worktree-level version request.
    #[test]
    fn native_input_changes_change_the_identity() {
        let root = scratch("native-identity");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv", "home = .\nversion = 3.12.7\n");
        let id = || resolve(&root, &root, None).identity;
        let before = id();
        put(&root, "pyproject.toml", "[project]\nname = \"svc2\"\n");
        assert_ne!(id(), before, "manifest content");

        let before = id();
        put(&root, "uv.lock", "lock v1\n");
        assert_ne!(id(), before, "added lockfile");
        let before = id();
        put(&root, "uv.lock", "lock v2\n");
        assert_ne!(id(), before, "lockfile content");
        let before = id();
        fs::remove_file(root.join("uv.lock")).unwrap();
        assert_ne!(id(), before, "removed lockfile");

        let before = id();
        put(&root, "poetry.lock", "poetry v1\n");
        assert_ne!(id(), before, "poetry lockfile");
        let before = id();
        fs::remove_file(root.join("poetry.lock")).unwrap();
        assert_ne!(id(), before, "removed poetry lockfile");

        // A pin file without venv keys pins nothing, so only its fingerprint can move.
        let before = id();
        put(&root, "pyrightconfig.json", "{\"extra\": true}\n");
        assert_ne!(id(), before, "added pin file");
        let before = id();
        put(&root, "pyrightconfig.json", "{\"extra\": false}\n");
        assert_ne!(id(), before, "pin file content");
        let before = id();
        fs::remove_file(root.join("pyrightconfig.json")).unwrap();
        assert_ne!(id(), before, "removed pin file");

        let before = id();
        put(&root, "requirements-dev.txt", "pytest\n");
        assert_ne!(id(), before, "requirements file");

        let before = id();
        put(&root, ".python-version", "3.13\n");
        assert_ne!(id(), before, "version request");

        // The worktree-level request of a nested root belongs to that root's identity too.
        let nested = root.join("packages/svc");
        put(&nested, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(&root, "packages/svc/app.py", "");
        let nested_before = resolve(&root, &nested, None).identity;
        put(&root, ".python-version", "3.12\n");
        assert_ne!(
            resolve(&root, &nested, None).identity,
            nested_before,
            "worktree-level version request"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A change deep inside a large lockfile invalidates the resolution: the fingerprint hashes
    /// the whole content in bounded chunks, not a prefix, so a dependency changed past any
    /// fixed window must not ride on the old answer. The two lockfiles are the same length and
    /// differ in exactly one byte beyond 256 KiB.
    #[test]
    fn lockfile_tail_change_changes_the_identity() {
        let root = scratch("native-lock-tail");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv", "");
        let body = "a".repeat(256 * 1024);
        put(&root, "uv.lock", &format!("{body}a\n"));
        let before = resolve(&root, &root, None).identity;
        put(&root, "uv.lock", &format!("{body}b\n"));
        assert_ne!(
            resolve(&root, &root, None).identity,
            before,
            "a changed tail byte is a changed lock"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// The supported standalone include spellings preserve a whole quoted path with spaces;
    /// remote/expanded/escaped paths, malformed quoting and combined directives stay explicit.
    #[test]
    fn requirement_reference_syntax_is_bounded_and_explicit() {
        for line in [
            "-r deps.in",
            "-rdeps.in",
            "-r=deps.in",
            "--requirement deps.in",
            "--requirement=deps.in",
            "-c deps.in",
            "-cdeps.in",
            "-c=deps.in",
            "--constraint deps.in",
            "--constraint=deps.in",
        ] {
            assert_eq!(requirement_reference(line), Ok(Some("deps.in")), "{line}");
        }
        for line in [
            "-r 'deps with spaces.in'",
            "-c\"deps with spaces.in\" # comment",
            "--requirement='deps with spaces.in'",
            "--constraint=\"deps with spaces.in\"\t# comment",
        ] {
            assert_eq!(
                requirement_reference(line),
                Ok(Some("deps with spaces.in")),
                "{line}"
            );
        }
        for line in ["", "# -r ignored", "pkg==1 # -c ignored", "python==3.99"] {
            assert_eq!(requirement_reference(line), Ok(None), "{line}");
        }
        for line in [
            "-r",
            "-c 'unclosed",
            "-r \"bad\"tail",
            "-r a -c b",
            "--requirementfoo",
            "--no-index -r a",
            "-r https://example.invalid/a",
            "-r file:///a",
            "-r ${INPUT}",
            "-c ~/private",
            "-r path\\ with\\ spaces",
            "-r a\\",
        ] {
            assert!(requirement_reference(line).is_err(), "{line}");
        }
    }

    /// A constraint-only content change invalidates the shared identity, including a request
    /// for a different Python package version, while the selected prepared interpreter,
    /// source and version remain unchanged and no interpreter-version warning is invented.
    #[test]
    fn referenced_constraint_change_preserves_the_selected_environment() {
        let root = scratch("constraint-identity");
        put(&root, "requirements.txt", "-c constraints.txt\n");
        put(&root, "constraints.txt", "python==3.99\n");
        venv(&root, ".venv", "version = 3.14.0\n");
        let before = resolve(&root, &root, Some(".venv"));
        put(&root, "constraints.txt", "python==3.98\n");
        let after = resolve(&root, &root, Some(".venv"));
        assert_ne!(before.identity, after.identity);
        assert_eq!(interpreter(&before), interpreter(&after));
        assert_eq!(after.source, Some(EnvSource::Selected));
        assert_eq!(after.chosen.unwrap().version.as_deref(), Some("3.14.0"));
        assert!(before.warnings.is_empty());
        assert!(after.warnings.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// References are relative to each containing file, may cross a nested project root
    /// within the authoritative worktree, and deduplicate canonical aliases/cycles. Quoted
    /// paths with spaces work through real file discovery, not just the line parser.
    #[test]
    fn nested_requirement_references_deduplicate_and_terminate_cycles() {
        let root = scratch("requirement-cycle");
        let project = root.join("svc");
        put(
            &project,
            "requirements.txt",
            "-r '../shared/dev deps.in'\n-r '../shared/./dev deps.in'\n",
        );
        put(
            &root,
            "shared/dev deps.in",
            "--constraint='limits/base pins.txt'\n-r ../svc/requirements.txt\n",
        );
        put(
            &root,
            "shared/limits/base pins.txt",
            "pkg==1\n-c '../dev deps.in'\n",
        );
        venv(&project, ".venv", "");
        let before = resolve(&root, &project, None);
        assert!(before.warnings.is_empty(), "{:?}", before.warnings);
        let mut parts = Vec::new();
        let mut warnings = Vec::new();
        requirement_inputs(&root, &project, &[], &mut parts, &mut warnings);
        assert_eq!(parts.len(), 3, "each canonical input is hashed once");
        assert_eq!(resolve(&root, &project, None).identity, before.identity);
        put(
            &root,
            "shared/limits/base pins.txt",
            "pkg==2\n-c '../dev deps.in'\n",
        );
        let after = resolve(&root, &project, None);
        assert_ne!(after.identity, before.identity);
        assert_eq!(interpreter(&after), interpreter(&before));
        assert!(after.warnings.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// A same-length changed tail beyond 256 KiB in a referenced constraint is fully hashed;
    /// the streaming result matches the full-content BLAKE3 and reference coverage is complete.
    #[test]
    fn referenced_constraint_tail_is_hashed_in_full() {
        let root = fs::canonicalize(scratch("constraint-tail")).unwrap();
        put(&root, "requirements.txt", "--constraint=deps/pins.txt\n");
        venv(&root, ".venv", "");
        let padding = "# padding\n".repeat(30_000);
        let body = format!("{padding}pkg==1\n");
        assert!(body.len() > 256 * 1024);
        put(&root, "deps/pins.txt", &body);
        let before = resolve(&root, &root, None);
        let mut remaining = REQUIREMENT_BYTES;
        let mut reasons = BTreeSet::new();
        let (hash, references) =
            hash_requirements(&root.join("deps/pins.txt"), &mut remaining, &mut reasons).unwrap();
        assert_eq!(hash, blake3::hash(body.as_bytes()));
        assert!(references.is_empty());
        assert!(reasons.is_empty());
        put(&root, "deps/pins.txt", &format!("{padding}pkg==2\n"));
        let after = resolve(&root, &root, None);
        assert_ne!(after.identity, before.identity);
        assert!(after.warnings.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Denied lexical/resolved paths, lexical escapes and file/directory symlinks contribute
    /// no foreign content. Mutating every hidden target leaves the entire resolution output
    /// unchanged, while coverage reasons remain explicit and the prepared environment wins.
    #[test]
    fn excluded_and_escaping_requirement_contents_never_enter_the_resolution() {
        let fixture = scratch("requirement-boundary");
        let root = fixture.join("worktree");
        venv(&root, ".venv", "");
        put(
            &root,
            "requirements.txt",
            "-c hidden/pins.txt\n-c ../foreign.txt\n-r link.txt\n-r linked/pins.txt\n",
        );
        put(&root, "hidden/pins.txt", "hidden==1\n");
        put(&fixture, "foreign.txt", "foreign==1\n");
        put(&fixture, "foreign/pins.txt", "foreign==1\n");
        std::os::unix::fs::symlink(fixture.join("foreign.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(
            fixture.join("foreign.txt"),
            root.join("requirements-escape.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(fixture.join("foreign"), root.join("linked")).unwrap();
        let denies = [ReadDeny::Path(root.join("hidden"))];
        let before = resolve_with_denies(&root, &root, None, &denies).env;
        assert_eq!(chosen(&before), Some(".venv"));
        let warnings = before.warnings.join(";");
        assert!(warnings.contains("excluded from reads"));
        assert!(warnings.contains("escapes worktree"));
        assert!(warnings.contains("symlinked local references"));
        put(&root, "hidden/pins.txt", "hidden==2\n");
        put(&fixture, "foreign.txt", "foreign==2\n");
        put(&fixture, "foreign/pins.txt", "foreign==2\n");
        let after = resolve_with_denies(&root, &root, None, &denies).env;
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
        // A canonical deny also applies when the authoritative worktree has a lexical alias.
        let alias = fixture.join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let resolved_deny = [ReadDeny::Path(
            fs::canonicalize(root.join("hidden")).unwrap(),
        )];
        assert_eq!(
            requirement_path(&alias, &alias.join("hidden/pins.txt"), &resolved_deny),
            Err("local reference excluded from reads")
        );
        // Replacing an already-validated parent with a link still cannot redirect the open.
        put(&root, "swap/pins.txt", "local==1\n");
        let checked = requirement_path(&root, &root.join("swap/pins.txt"), &[]).unwrap();
        fs::rename(root.join("swap"), root.join("retained")).unwrap();
        std::os::unix::fs::symlink(fixture.join("foreign"), root.join("swap")).unwrap();
        let mut remaining = REQUIREMENT_BYTES;
        assert!(hash_requirements(&checked, &mut remaining, &mut BTreeSet::new()).is_err());
        fs::remove_dir_all(&fixture).unwrap();
    }

    /// Missing, unsupported/remote/expanded and non-UTF-8 references are warnings rather than
    /// falsely complete coverage; overlong lines are fully hashed but explicitly unparsed.
    #[test]
    fn unsupported_requirement_coverage_is_visible_without_changing_selection() {
        let root = scratch("requirement-unsupported");
        venv(&root, ".venv", "version = 3.14.0\n");
        for text in [
            "-r absent.in\n",
            "-c https://example.invalid/pins\n",
            "-r ${INPUT}\n",
            "-r 'unclosed\n",
            "-r a -c b\n",
            "-r a\\\n b\n",
        ] {
            put(&root, "requirements.txt", text);
            let env = resolve(&root, &root, Some(".venv"));
            assert_eq!(chosen(&env), Some(".venv"));
            assert_eq!(env.source, Some(EnvSource::Selected));
            assert!(
                env.warnings
                    .iter()
                    .any(|warning| warning.contains("coverage incomplete")),
                "{text}"
            );
        }
        fs::write(root.join("requirements.txt"), [0xff, b'\n']).unwrap();
        assert!(resolve(&root, &root, None).warnings[0].contains("non-UTF-8"));
        put(&root, "requirements.txt", &"a".repeat(REQUIREMENT_LINE + 1));
        let before = resolve(&root, &root, None);
        assert!(before.warnings[0].contains("line exceeds 8 KiB"));
        put(
            &root,
            "requirements.txt",
            &format!("{}b", "a".repeat(REQUIREMENT_LINE)),
        );
        assert_ne!(resolve(&root, &root, None).identity, before.identity);
        fs::remove_dir_all(&root).unwrap();
    }

    /// File/edge/byte bounds stop discovery deterministically and expose incomplete coverage.
    /// The streaming reader returns no digest for an over-budget prefix, while an exact
    /// byte-budget file succeeds, including its final physical line without LF.
    #[test]
    fn requirement_discovery_limits_are_explicit() {
        let root = fs::canonicalize(scratch("requirement-limits")).unwrap();
        venv(&root, ".venv", "");
        put(&root, "requirements.txt", "-r deps/0.in\n");
        for index in 0..REQUIREMENT_FILES {
            put(
                &root,
                &format!("deps/{index}.in"),
                &format!("-r {}.in\n", index + 1),
            );
        }
        assert!(resolve(&root, &root, None).warnings[0].contains("file budget exceeded (64)"));
        put(
            &root,
            "requirements.txt",
            &"-r deps/0.in\n".repeat(REQUIREMENT_FILES + 1),
        );
        assert!(resolve(&root, &root, None).warnings[0].contains("reference budget exceeded (64)"));
        for index in 0..REQUIREMENT_FILES {
            put(&root, &format!("requirements-{index}.txt"), "pkg==1\n");
        }
        assert!(resolve(&root, &root, None).warnings[0].contains("file budget exceeded (64)"));
        for index in 0..REQUIREMENT_FILES {
            fs::remove_file(root.join(format!("requirements-{index}.txt"))).unwrap();
        }
        put(
            &root,
            "requirements.txt",
            &"#\n".repeat(REQUIREMENT_BYTES / 2 + 1),
        );
        assert!(resolve(&root, &root, None).warnings[0].contains("byte budget exceeded (8 MiB)"));
        put(&root, "small.in", "pkg==1");
        let mut reasons = BTreeSet::new();
        let mut remaining = 6;
        assert_eq!(
            hash_requirements(&root.join("small.in"), &mut remaining, &mut reasons)
                .unwrap()
                .0,
            blake3::hash(b"pkg==1")
        );
        let mut remaining = 5;
        assert_eq!(
            hash_requirements(&root.join("small.in"), &mut remaining, &mut reasons),
            Err("requirement byte budget exceeded (8 MiB)")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A host-denied native input is unreadable and says nothing: a fixed file's fingerprint
    /// entry equals the file's absence, a dynamically enumerated `requirements*.txt` gets no
    /// entry at all when denied (denied fingerprints exactly like absent), and the resolver's
    /// existing refusal of denied manifest/pin files is intact through the new fingerprint path.
    #[test]
    fn denied_native_inputs_say_nothing() {
        use agent_ide_core::execution::seatbelt::ReadDeny;
        let root = scratch("native-denied");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv", "");
        put(&root, "uv.lock", "lock v1\n");
        put(&root, "requirements-dev.txt", "pytest\n");
        let resolve = |denies: &[ReadDeny]| resolve_with_denies(&root, &root, None, denies).env;
        // A denied lockfile contributes the same `-` entry as no lockfile at all.
        let denied = [ReadDeny::Path(root.join("uv.lock"))];
        let with_denied_lock = resolve(&denied).identity;
        fs::remove_file(root.join("uv.lock")).unwrap();
        assert_eq!(
            resolve(&[]).identity,
            with_denied_lock,
            "a denied input is unreadable, not a different answer"
        );
        put(&root, "uv.lock", "lock v1\n");
        assert_eq!(chosen(&resolve(&denied)), Some(".venv"));
        // A denied enumerated requirements file is no entry, not a `-` entry: denied equals
        // absent for the dynamic part of the fingerprint too.
        let denied_requirements = [ReadDeny::Path(root.join("requirements-dev.txt"))];
        let with_denied_requirements = resolve(&denied_requirements).identity;
        fs::remove_file(root.join("requirements-dev.txt")).unwrap();
        assert_eq!(
            resolve(&[]).identity,
            with_denied_requirements,
            "a denied requirements file is unreadable, not a different answer"
        );
        put(&root, "requirements-dev.txt", "pytest\n");
        // The existing refusal of a denied manifest is unchanged.
        let denies = [ReadDeny::Path(root.join("pyproject.toml"))];
        let refused = resolve(&denies);
        assert_eq!(chosen(&refused), None);
        assert!(
            refused
                .missing_next_step
                .as_deref()
                .is_some_and(|step| step.contains("excluded from reads by the host"))
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A native input that exists but is not UTF-8 text is still content: it is hashed as the
    /// bytes it is, so its presence — and any change of it — invalidates the resolution. The
    /// fingerprint is about the file, not about its readability as text.
    #[test]
    fn non_utf8_native_input_is_content() {
        let root = scratch("native-non-utf8");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv", "");
        let without_lock = resolve(&root, &root, None).identity;
        fs::write(root.join("uv.lock"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let with_binary = resolve(&root, &root, None).identity;
        assert_ne!(
            with_binary, without_lock,
            "existing content counts, whatever its encoding"
        );
        assert_eq!(chosen(&resolve(&root, &root, None)), Some(".venv"));
        fs::write(root.join("uv.lock"), [0xff, 0xfe, 0x00, 0x02]).unwrap();
        assert_ne!(
            resolve(&root, &root, None).identity,
            with_binary,
            "a changed byte of binary content is a change"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix G: an environment whose `bin/python` is a dangling link stays a candidate marked
    /// broken; discovery passes over it, and selecting it reports it broken.
    #[test]
    fn dangling_interpreter_stays_a_broken_candidate() {
        let root = scratch("dangling");
        put(&root, "pyproject.toml", "");
        fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent-agent-ide/python3",
            root.join(".venv/bin/python"),
        )
        .unwrap();
        venv(&root, ".venv-ok", "");
        let env = resolve(&root, &root, None);
        let broken: Vec<(&str, bool)> = env
            .candidates
            .iter()
            .map(|candidate| (candidate.label.as_str(), candidate.broken))
            .collect();
        assert_eq!(broken, [(".venv", true), (".venv-ok", false)]);
        assert_eq!(chosen(&env), Some(".venv-ok"));
        let selected = resolve(&root, &root, Some(".venv"));
        assert_eq!(chosen(&selected), None);
        assert_eq!(
            selected.missing_next_step.as_deref(),
            Some(
                "environment .venv is broken (base interpreter gone) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A broken environment (dangling `bin/python`) is refused with its cause, since selecting
    /// it could only end in a missing environment.
    #[test]
    fn check_selection_refuses_a_broken_environment() {
        let root = scratch("select-broken");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        fs::create_dir_all(root.join(".venv-old/bin")).unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent-agent-ide/python3",
            root.join(".venv-old/bin/python"),
        )
        .unwrap();
        assert_eq!(
            Python.check_selection(&root, Path::new(""), ".venv-old"),
            Err(
                "\".venv-old\" is broken (base interpreter gone); candidates .venv, .venv-old"
                    .to_owned()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// The test command's error is the resolver's own way out: for a pin, editing or removing
    /// the pin (a selection cannot help); for a nested root's selection, its `python:<root>` key.
    #[test]
    fn missing_environment_error_is_the_resolvers_next_step() {
        let root = scratch("next-step-pin");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
        );
        let project = Python.detect(&root).unwrap();
        let Err(LangError::Unsupported(message)) =
            Python.test_selection(&project, &TestTarget::Pattern("x".to_owned()))
        else {
            panic!("a missing pinned environment refuses the test run");
        };
        assert!(
            message.contains("edit venv there or remove the pin") && !message.contains("auto"),
            "{message}"
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("next-step-nested");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        select(&root, "packages/alpha", ".venv-gone");
        let project = Python.detect(&root).unwrap();
        assert_eq!(
            Python.test_selection(&project, &TestTarget::Pattern("x".to_owned())),
            Err(LangError::Unsupported(
                "packages/alpha: environment .venv-gone missing (selected) — recreate it or ide.start environment {\"python:packages/alpha\": \"auto\"}"
                    .to_owned()
            ))
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }
}
