//! Per-language project environments: what a language resolves for each project root, and the
//! choices agents make through `ide.start environment`.
//!
//! The core stays language-free: it stores selections as opaque selector text keyed by worktree,
//! language and project root, and renders a [`ResolvedEnv`] without knowing what an interpreter
//! or toolchain is. Each language's single resolver ([`super::LanguageSupport::environments`])
//! reads the selections through [`selections`] and decides what they mean.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use super::Language;

/// Size of one chunk a native input streams through while being hashed: hashing costs this
/// much memory per file, never the file's size.
const INPUT_CHUNK: usize = 64 * 1024;

/// Size of one chunk a native input streams through while being hashed: hashing costs this
/// much memory per file, never the file's size.
const INPUT_CHUNK: usize = 64 * 1024;

/// Opens `path` under the environment-input gate, or `None` when the file says nothing:
/// missing, host-denied, or a link that refuses to be followed. Without denies a regular file
/// is read plainly; under denies a denied or symlinked input is refused without being followed
/// (`O_NOFOLLOW`). The one gate every environment input read goes through.
fn open_input(path: &Path, denies: &[crate::execution::seatbelt::ReadDeny]) -> Option<fs::File> {
    if denies.is_empty() {
        if !path.is_file() {
            return None;
        }
        return fs::File::open(path).ok();
    }
    if denies.iter().any(|deny| deny.matches(path)) {
        return None;
    }
    // A final link is not proved readable: refuse without following.
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()
}

/// The guarded text of a native environment input, or `None` when it says nothing (see
/// [`open_input`]). Small declared inputs only — pins and manifests a language parses — never
/// a substitute for streaming a large file.
pub fn read_input(
    path: &Path,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> Option<String> {
    let mut text = String::new();
    open_input(path, denies)?
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Whether `path` is readable under the environment-input gate ([`open_input`]): a language
/// checks this before a content-dependent decision, so a denied input never influences one.
pub fn readable(path: &Path, denies: &[crate::execution::seatbelt::ReadDeny]) -> bool {
    open_input(path, denies).is_some()
}

/// Whole-content BLAKE3 of a native environment input, hashed with bounded memory, or `None`
/// when the file says nothing (see [`open_input`]). The file is streamed in `INPUT_CHUNK`
/// chunks straight into the hasher, so no input is ever held in memory whole, and any content
/// — UTF-8 or not — is hashed as the bytes it is. A language's resolver calls this for every
/// fingerprinted project file so all of them hash identically.
pub fn hash_input(
    path: &Path,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> Option<blake3::Hash> {
    let mut file = open_input(path, denies)?;
    let mut hasher = blake3::Hasher::new();
    let mut chunk = vec![0u8; INPUT_CHUNK];
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            return Some(hasher.finalize());
        }
        hasher.update(&chunk[..read]);
    }
}

/// One stored environment choice for one project root of a worktree.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvSelection {
    /// The project root, relative to the worktree; empty for the worktree itself.
    pub root: PathBuf,
    /// The selector exactly as the language accepted it: a candidate label, a path, a version.
    pub selector: String,
}

/// One environment a language found for a project root.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvCandidate {
    /// What the card shows and an agent may pass back as a selector: `.venv-py314`.
    pub label: String,
    /// Absolute environment directory (or toolchain directory).
    pub path: PathBuf,
    /// Version read from files (`3.14.0`), or `None` when the files do not say.
    pub version: Option<String>,
    /// The environment exists but cannot run (for example its base interpreter is gone).
    pub broken: bool,
}

/// Why a project root uses its chosen environment.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum EnvSource {
    /// An agent chose it through `ide.start environment`.
    Selected,
    /// A committed project file pins it; the text names the file (`pyrightconfig.json`).
    Pin(String),
    /// Found by the language's discovery order.
    Discovered,
    /// The operator's launcher configuration supplies it.
    Launcher,
}

/// The single answer of one language's resolver for one project root; every consumer (card,
/// project check, language server session, test and format commands) uses the same answer.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedEnv {
    /// The project root, relative to the worktree; empty for the worktree itself.
    pub root: PathBuf,
    /// The environment in use, or `None` when none resolves.
    pub chosen: Option<EnvCandidate>,
    /// Why `chosen` won; `None` exactly when `chosen` is `None`.
    pub source: Option<EnvSource>,
    /// Every environment found for the root, `chosen` included, in the language's order.
    pub candidates: Vec<EnvCandidate>,
    /// Short facts worth one clause on the card, such as a mismatch with a version-request file.
    pub warnings: Vec<String>,
    /// When nothing resolves (or the chosen one is missing): the cause and a working next step.
    pub missing_next_step: Option<String>,
    /// Opaque identity of the resolution (chosen path, version, pin-file stamps). Two answers with
    /// the same identity mean nothing a consumer depends on changed.
    pub identity: String,
}

/// How one command (a test or formatter run) must start inside a resolved environment.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnv {
    /// Replaces the command's program with an interpreter/module or pinned executable. The
    /// command's remaining arguments follow unchanged.
    pub argv_prefix: Vec<OsString>,
    /// Directory put first on the command's `PATH`.
    pub path_prefix: Option<PathBuf>,
    /// Variables set for the command: `VIRTUAL_ENV`.
    pub vars: Vec<(OsString, OsString)>,
}

/// Current choices partitioned by worktree and registered language.
type SelectionMap = HashMap<(PathBuf, Language), Vec<EnvSelection>>;

/// Selections in force, keyed by canonical worktree and language. The durable store is the
/// source of truth; the worker loads a worktree's rows into this map when it resolves the
/// worktree and rewrites them whenever `ide.start environment` changes them.
static SELECTIONS: LazyLock<Mutex<SelectionMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The selections in force for one language in one worktree, in no particular order.
///
/// The map is a cache of the durable store, so a lock poisoned by a panicking holder is recovered
/// (poisoned-lock policy, `docs/architecture.md`): the lookup answers what was last written
/// instead of panicking every later reader.
pub fn selections(worktree: &Path, language: Language) -> Vec<EnvSelection> {
    SELECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(worktree.to_path_buf(), language))
        .cloned()
        .unwrap_or_default()
}

/// Replaces every selection of one language in one worktree; an empty list clears them.
///
/// Recovers a poisoned lock like [`selections`]: the next load from the durable store rewrites the
/// worktree's rows whole.
pub fn replace_selections(worktree: &Path, language: Language, selections: Vec<EnvSelection>) {
    let mut map = SELECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (worktree.to_path_buf(), language);
    if selections.is_empty() {
        map.remove(&key);
    } else {
        map.insert(key, selections);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A holder that panicked with the process-wide selection map locked does not make every later
    /// read or write of the (rebuildable) cache panic.
    #[test]
    fn poisoned_selection_cache_is_recovered_not_propagated() {
        crate::lang::testing::install();
        let poisoned = std::thread::spawn(|| {
            let _map = SELECTIONS.lock().unwrap();
            panic!("poison the selection cache");
        })
        .join();
        assert!(poisoned.is_err() && SELECTIONS.is_poisoned());

        let worktree = Path::new("/nonexistent/poisoned-cache");
        let language = crate::lang::testing::ALPHA;
        assert!(selections(worktree, language).is_empty());
        replace_selections(worktree, language, Vec::new());
        assert!(selections(worktree, language).is_empty());
    }
}
