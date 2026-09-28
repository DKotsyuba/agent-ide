//! Repository-level index of cross-language name facts (the language bridge).
//!
//! One [`NameIndex`] per worktree incarnation holds, per candidate file, the facts its language's
//! [`NameFacts`] provider extracted, plus an inverted map from [`NameKey`] to files. A sweep lists
//! candidates (`git ls-files`, or a bounded walk without Git), stats them, and re-reads and
//! re-extracts only files whose size or mtime changed; a digest confirms the change before facts
//! are replaced. Every read goes through [`read_authorized_source`]. Nothing is persisted: the
//! index is rebuilt lazily after a restart. Counts are indexed, not live; a displayed row is
//! proven with [`NameIndex::verify`] against bytes read at render time.
//!
//! [`NameIndex`]: crate::intelligence::names::NameIndex
//! [`NameIndex::verify`]: crate::intelligence::names::NameIndex::verify
//! [`NameFacts`]: crate::lang::names::NameFacts
//! [`NameKey`]: crate::lang::names::NameKey
//! [`read_authorized_source`]: crate::workspace::observation::read_authorized_source

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    lang::{
        Language, LineRange,
        names::{FactSink, FileVerdict, NameFact, NameKey, Namespace, Role},
    },
    workspace::{
        authority::WorktreeRef,
        observation::{
            MAX_SOURCE_BYTES, MAX_SOURCE_PATH_BYTES, ObservationError, SourceReadLimits,
            read_authorized_source,
        },
    },
};

/// Provider-backed candidate files one listing keeps; listing stops beyond it.
pub const MAX_LISTED_FILES: usize = 50_000;
/// Candidate files one sweep indexes, in path-byte order.
pub const MAX_INDEXED_FILES: usize = 20_000;
/// Largest file indexed; larger files are skipped as `large`.
pub const MAX_FILE_BYTES: usize = MAX_SOURCE_BYTES;
/// Facts one worktree holds; a file that would pass it is skipped as `facts cap`.
pub const MAX_FACTS_PER_WORKTREE: usize = 500_000;
/// Worktree indexes kept; the least recently used one is dropped.
pub const MAX_WORKTREES: usize = 4;
/// Sweep time after which an unfinished sweep reports `Partial` instead of `Building`; the next
/// refresh resumes where it stopped.
pub const BUILD_BUDGET: Duration = Duration::from_secs(20);
/// Design target for a refresh with nothing changed at 10 000 candidate files.
pub const WARM_REFRESH_TARGET: Duration = Duration::from_millis(200);
/// Extractions the daemon-level fact cache keeps (two worktrees' worth of indexed files).
pub const MAX_CACHED_ENTRIES: usize = 2 * MAX_INDEXED_FILES;
/// Facts the daemon-level fact cache keeps. Live indexes share the cached facts, so this bounds
/// the memory of every worktree index together (about 12 MB per 500 000 facts).
pub const MAX_CACHED_FACTS: usize = 2 * MAX_FACTS_PER_WORKTREE;
/// Directories the fallback walk visits at most.
const WALK_MAX_DIRECTORIES: usize = 10_000;
/// Skip reason of a file whose facts would pass [`MAX_FACTS_PER_WORKTREE`].
const FACTS_CAP: &str = "facts cap";

/// Directories no language walk enters: VCS internals, virtual environments, dependency installs
/// and build output.
pub const SKIPPED_DIRECTORIES: [&str; 7] = [
    ".git",
    ".hg",
    ".venv",
    "venv",
    "node_modules",
    "target",
    "dist",
];

/// How complete the index is after a refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexState {
    /// A sweep is unfinished and still inside [`BUILD_BUDGET`]; ask again shortly.
    Building,
    /// Every listed candidate was swept.
    Ready,
    /// A bound was hit (sweep budget, listed or indexed files, worktree facts); counts are lower
    /// bounds. An unfinished sweep resumes on the next refresh.
    Partial {
        /// Candidates swept (and not skipped for the worktree facts bound).
        indexed: usize,
        /// Provider-backed candidates listed.
        listed: usize,
    },
}

/// One fact with its file and language.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Site {
    /// Worktree-relative file.
    pub file: PathBuf,
    /// Language owning the file.
    pub language: Language,
    /// The fact.
    pub fact: NameFact,
}

/// A site with the text of its line, read and verified for display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShownSite {
    /// The site.
    pub site: Site,
    /// Its line, trimmed and clipped.
    pub text: String,
}

/// Displayable sites of one key; see [`NameIndex::proven_sites`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Proven {
    /// Sites whose files were read and verified, in site order.
    pub sites: Vec<ShownSite>,
    /// Rows dropped because their file could no longer be read.
    pub dropped: usize,
    /// Sites past the display limit, not read.
    pub more: usize,
}

/// Indexed totals of one key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeySummary {
    /// The key.
    pub key: NameKey,
    /// Define facts.
    pub defines: usize,
    /// Use facts.
    pub uses: usize,
    /// Files with at least one fact.
    pub files: usize,
}

/// What identifies a file's content: its Git blob (clean tracked files, no read needed) or the
/// blake3 digest of its bytes (everything else).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ContentKey {
    /// A Git blob id from the worktree's index, for a file whose working copy is clean.
    GitBlob(Box<str>),
    /// blake3 of the bytes read.
    Digest([u8; 32]),
}

/// Key of one cached extraction: the language, its extractor revision and the content.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    /// Language identifier.
    language: &'static str,
    /// [`NameFacts::revision`](crate::lang::names::NameFacts::revision) of its provider.
    revision: &'static str,
    /// The content.
    content: ContentKey,
}

impl CacheKey {
    /// The key of `content` in `language`'s current extractor.
    fn of(language: Language, content: ContentKey) -> Option<Self> {
        Some(Self {
            language: language.name(),
            revision: language.names()?.revision(),
            content,
        })
    }
}

/// One file's extraction, shared by every worktree whose file has the same content.
#[derive(Debug)]
struct Extracted {
    /// Facts sorted by line, column, namespace id and name; empty for a skipped file.
    facts: Box<[NameFact]>,
    /// Why the content has no facts (`large`, `non-utf8`, a provider reason), if skipped.
    skipped: Option<&'static str>,
    /// The per-file fact cap refused facts.
    capped: bool,
}

/// Daemon-level cache of extractions by content, shared by the worktree indexes of one daemon
/// (one repository). Bounded by [`MAX_CACHED_ENTRIES`] and [`MAX_CACHED_FACTS`]; extractions no
/// live index references leave first, least recently used first.
#[derive(Debug)]
pub struct FactCache {
    /// Extractions with their last-use tick.
    entries: HashMap<CacheKey, (Arc<Extracted>, u64)>,
    /// Facts held across entries.
    facts: usize,
    /// Monotonic use counter.
    tick: u64,
    /// Entry bound ([`MAX_CACHED_ENTRIES`]).
    max_entries: usize,
    /// Fact bound ([`MAX_CACHED_FACTS`]).
    max_facts: usize,
}

impl Default for FactCache {
    /// An empty cache with the fixed bounds.
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            facts: 0,
            tick: 0,
            max_entries: MAX_CACHED_ENTRIES,
            max_facts: MAX_CACHED_FACTS,
        }
    }
}

impl FactCache {
    /// The cached extraction of `key`, marked used.
    fn get(&mut self, key: &CacheKey) -> Option<Arc<Extracted>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|(extracted, used)| {
            *used = tick;
            extracted.clone()
        })
    }

    /// Caches `extracted` under `key`, evicting when a bound is passed.
    fn insert(&mut self, key: CacheKey, extracted: Arc<Extracted>) {
        self.tick += 1;
        self.facts += extracted.facts.len();
        if let Some((old, _)) = self.entries.insert(key, (extracted, self.tick)) {
            self.facts -= old.facts.len();
        }
        if self.entries.len() > self.max_entries || self.facts > self.max_facts {
            self.evict();
        }
    }

    /// Evicts down to 90 % of both bounds: unreferenced extractions first (least recently used
    /// first), then referenced ones, which only stop being shared.
    ///
    /// ponytail: one sort of all entries per eviction batch; an intrusive LRU list if eviction
    /// ever shows up in profiles.
    fn evict(&mut self) {
        let mut victims: Vec<(bool, u64, CacheKey)> = self
            .entries
            .iter()
            .map(|(key, (extracted, used))| (Arc::strong_count(extracted) > 1, *used, key.clone()))
            .collect();
        victims.sort_unstable_by_key(|victim| (victim.0, victim.1));
        for (_, _, key) in victims {
            if self.entries.len() * 10 <= self.max_entries * 9
                && self.facts * 10 <= self.max_facts * 9
            {
                break;
            }
            if let Some((extracted, _)) = self.entries.remove(&key) {
                self.facts -= extracted.facts.len();
            }
        }
    }

    /// Extractions held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Indexed state of one candidate file.
struct FileEntry {
    /// Language owning the file.
    language: Language,
    /// `(length, mtime in ns)` when last read; `None` for a clean tracked file known by its blob.
    stamp: Option<(u64, i128)>,
    /// What the facts were extracted from.
    content: ContentKey,
    /// blake3 of the bytes, once they were read.
    digest: Option<[u8; 32]>,
    /// The (possibly shared) extraction.
    extracted: Arc<Extracted>,
    /// The worktree fact cap refused this file.
    over_cap: bool,
}

impl FileEntry {
    /// Facts the worktree holds for this file.
    fn facts(&self) -> &[NameFact] {
        if self.over_cap {
            &[]
        } else {
            &self.extracted.facts
        }
    }

    /// Why the file has no facts, if skipped.
    fn skipped(&self) -> Option<&'static str> {
        if self.over_cap {
            Some(FACTS_CAP)
        } else {
            self.extracted.skipped
        }
    }
}

/// One pass over the listed candidates, resumable across refreshes.
struct Sweep {
    /// Candidates in path-byte order, at most [`MAX_INDEXED_FILES`], with the Git blob id of a
    /// clean tracked file.
    candidates: Vec<(PathBuf, Language, Option<Box<str>>)>,
    /// Provider-backed candidates listed, at most [`MAX_LISTED_FILES`].
    listed: usize,
    /// Next candidate to visit.
    cursor: usize,
    /// Time spent sweeping so far.
    elapsed: Duration,
}

/// Name facts of one worktree incarnation.
pub struct NameIndex {
    /// The worktree every read is authorized against.
    worktree: WorktreeRef,
    /// Indexed files.
    files: HashMap<PathBuf, FileEntry>,
    /// Files holding at least one fact of each key.
    postings: BTreeMap<NameKey, BTreeSet<PathBuf>>,
    /// Facts held across all files.
    facts: usize,
    /// Registered languages seen by the last listing, with or without a provider.
    present: BTreeSet<Language>,
    /// The unfinished sweep, if any.
    sweep: Option<Sweep>,
    /// Files the last refresh read (0 when nothing changed or everything came from the cache).
    reread: usize,
    /// Files the last refresh took from the fact cache instead of extracting them.
    reused: usize,
    /// A sweep has completed at least once.
    built: bool,
    /// Extractions shared with the other worktrees of the daemon.
    cache: Arc<Mutex<FactCache>>,
}

impl NameIndex {
    /// An empty index of `worktree` with a cache of its own; nothing is read until
    /// [`NameIndex::refresh`].
    pub fn new(worktree: WorktreeRef) -> Self {
        Self::with_cache(worktree, Arc::default())
    }

    /// An empty index of `worktree` sharing `cache` with the daemon's other worktree indexes.
    pub fn with_cache(worktree: WorktreeRef, cache: Arc<Mutex<FactCache>>) -> Self {
        Self {
            worktree,
            files: HashMap::new(),
            postings: BTreeMap::new(),
            facts: 0,
            present: BTreeSet::new(),
            sweep: None,
            reread: 0,
            reused: 0,
            built: false,
            cache,
        }
    }

    /// Continues (or starts) a sweep until it finishes or `deadline` passes: lists candidates at
    /// the start of a sweep, re-reads only files whose stamp changed (the digest confirms the
    /// change before facts are replaced) and, once finished, drops files no longer listed.
    ///
    /// ponytail: stamps are `(len, mtime)` like the check fingerprint, so a same-size rewrite in
    /// the same nanosecond keeps stale undisplayed counts until the next change; displayed rows
    /// are always proven by [`NameIndex::verify`].
    pub fn refresh(&mut self, deadline: Instant) -> IndexState {
        let started = Instant::now();
        self.reread = 0;
        self.reused = 0;
        let mut sweep = match self.sweep.take() {
            Some(sweep) => sweep,
            None => self.list(),
        };
        while sweep.cursor < sweep.candidates.len() && Instant::now() < deadline {
            let (path, language, blob) = sweep.candidates[sweep.cursor].clone();
            self.visit(&path, language, blob);
            sweep.cursor += 1;
        }
        sweep.elapsed += started.elapsed();
        if sweep.cursor < sweep.candidates.len() {
            let state = if sweep.elapsed < BUILD_BUDGET {
                IndexState::Building
            } else {
                IndexState::Partial {
                    indexed: sweep.cursor,
                    listed: sweep.listed,
                }
            };
            self.sweep = Some(sweep);
            return state;
        }
        let kept: HashSet<&PathBuf> = sweep.candidates.iter().map(|(path, ..)| path).collect();
        let vanished: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|path| !kept.contains(path))
            .cloned()
            .collect();
        for path in vanished {
            self.remove(&path);
        }
        self.built = true;
        let indexed =
            sweep.candidates.len() - self.files.values().filter(|entry| entry.over_cap).count();
        if indexed == sweep.listed {
            IndexState::Ready
        } else {
            IndexState::Partial {
                indexed,
                listed: sweep.listed,
            }
        }
    }

    /// Every fact of `key`, ordered by file path bytes, then line, column, namespace id and name.
    pub fn sites(&self, key: &NameKey) -> Vec<Site> {
        let mut files: Vec<&PathBuf> = self
            .postings
            .get(key)
            .map(|files| files.iter().collect())
            .unwrap_or_default();
        files.sort_by(|a, b| path_bytes(a).cmp(path_bytes(b)));
        files
            .into_iter()
            .flat_map(|file| {
                let entry = &self.files[file];
                entry
                    .facts()
                    .iter()
                    .filter(|fact| fact.key == *key)
                    .map(|fact| Site {
                        file: file.clone(),
                        language: entry.language,
                        fact: fact.clone(),
                    })
            })
            .collect()
    }

    /// Indexed keys spelled `name` in every domain, optionally in one namespace, in key order.
    ///
    /// ponytail: scans every distinct key; a name-to-keys map when key counts make this slow.
    pub fn keys_named(&self, name: &str, only: Option<Namespace>) -> Vec<KeySummary> {
        self.postings
            .iter()
            .filter(|(key, _)| &*key.name == name && only.is_none_or(|ns| key.namespace == ns))
            .map(|(key, files)| {
                let (mut defines, mut uses) = (0, 0);
                for fact in files
                    .iter()
                    .flat_map(|file| self.files[file].facts())
                    .filter(|fact| fact.key == *key)
                {
                    match fact.role {
                        Role::Define => defines += 1,
                        Role::Use => uses += 1,
                    }
                }
                KeySummary {
                    key: key.clone(),
                    defines,
                    uses,
                    files: files.len(),
                }
            })
            .collect()
    }

    /// Facts of `file` on `lines`, in line and column order.
    pub fn facts_in(&self, file: &Path, lines: LineRange) -> Vec<Site> {
        self.files.get(file).map_or_else(Vec::new, |entry| {
            entry
                .facts()
                .iter()
                .filter(|fact| lines.start <= fact.line && fact.line <= lines.end)
                .map(|fact| Site {
                    file: file.to_path_buf(),
                    language: entry.language,
                    fact: fact.clone(),
                })
                .collect()
        })
    }

    /// Render-time proof for a displayed row: whether `bytes`, just read from `file`, are the
    /// bytes its facts came from. On a mismatch the file is re-extracted from `bytes` and `false`
    /// is returned, so the caller queries again.
    pub fn verify(&mut self, file: &Path, bytes: &[u8]) -> bool {
        let digest = *blake3::hash(bytes).as_bytes();
        let Some(entry) = self.files.get_mut(file) else {
            self.reindex(file, bytes);
            return false;
        };
        if entry.digest == Some(digest) {
            return true;
        }
        // A file known only by its Git blob was never read here: its bytes prove the facts when
        // they extract to the same facts.
        if entry.digest.is_none()
            && let Some(extracted) = extract(entry.language, file, bytes)
            && extracted.facts == entry.extracted.facts
            && extracted.skipped == entry.extracted.skipped
        {
            entry.digest = Some(digest);
            return true;
        }
        self.reindex(file, bytes);
        false
    }

    /// Replaces `file`'s entry with the facts of `bytes` (a render-time read).
    fn reindex(&mut self, file: &Path, bytes: &[u8]) {
        if let Some(language) =
            Language::for_path(file).filter(|language| language.names().is_some())
            && bytes.len() <= MAX_FILE_BYTES
        {
            let stamp = std::fs::symlink_metadata(self.worktree.worktree_path().join(file))
                .ok()
                .map(|metadata| stamp_of(&metadata));
            self.ingest(file, language, stamp, bytes, None);
        }
    }

    /// Languages present in the worktree (by the last listing) whose provider covers neither
    /// role of `namespace`, or that have no provider, in registration order.
    pub fn uncovered(&self, namespace: Namespace) -> Vec<Language> {
        self.present
            .iter()
            .copied()
            .filter(|language| {
                !language.names().is_some_and(|names| {
                    names.coverage().iter().any(|coverage| {
                        coverage.namespace == namespace && (coverage.defines || coverage.uses)
                    })
                })
            })
            .collect()
    }

    /// Sites of `key` (of `role`, when given) proven against the current bytes, for display:
    /// every file among the first
    /// `limit` sites is read through [`read_authorized_source`] and [`NameIndex::verify`]ed; when
    /// one changed it is re-extracted and the sites are queried again, so no row outlives its
    /// bytes. Rows of files that can no longer be read are dropped and counted; sites past
    /// `limit` are only counted.
    pub fn proven_sites(&mut self, key: &NameKey, role: Option<Role>, limit: usize) -> Proven {
        let of_role = |sites: Vec<Site>| -> Vec<Site> {
            sites
                .into_iter()
                .filter(|site| role.is_none_or(|role| site.fact.role == role))
                .collect()
        };
        let mut sites = of_role(self.sites(key));
        let mut sources: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut changed = false;
        for site in sites.iter().take(limit) {
            if sources.contains_key(&site.file) {
                continue;
            }
            let text = self.read(&site.file).map(|bytes| {
                changed |= !self.verify(&site.file, &bytes);
                String::from_utf8(bytes).ok()
            });
            sources.insert(site.file.clone(), text.flatten());
        }
        if changed {
            sites = of_role(self.sites(key));
        }
        let more = sites.len().saturating_sub(limit);
        let mut proven = Proven {
            sites: Vec::new(),
            dropped: 0,
            more,
        };
        for site in sites.into_iter().take(limit) {
            match sources.get(&site.file) {
                Some(Some(text)) => proven.sites.push(ShownSite {
                    text: crate::lang::render::line_text(text, site.fact.line),
                    site,
                }),
                _ => proven.dropped += 1,
            }
        }
        proven
    }

    /// The current text of `file`, read through [`read_authorized_source`]; `None` when it is
    /// missing, unreadable, over the size cap or not UTF-8.
    pub fn source(&self, file: &Path) -> Option<String> {
        self.read(file)
            .and_then(|bytes| String::from_utf8(bytes).ok())
    }

    /// The current bytes of `file` through the authorized reader.
    fn read(&self, file: &Path) -> Option<Vec<u8>> {
        let limits =
            SourceReadLimits::new(MAX_SOURCE_PATH_BYTES, MAX_FILE_BYTES).expect("fixed limits");
        read_authorized_source(&self.worktree, file, limits)
            .ok()
            .map(|read| read.contents().to_vec())
    }

    /// `(indexed files, facts)` held.
    pub fn summary(&self) -> (usize, usize) {
        (self.files.len(), self.facts)
    }

    /// Files the last refresh read; 0 when nothing changed or every change came from the cache.
    pub fn last_reread(&self) -> usize {
        self.reread
    }

    /// Files the last refresh took from the fact cache instead of extracting them.
    pub fn last_reused(&self) -> usize {
        self.reused
    }

    /// Whether a sweep has completed at least once (the index answers without building).
    pub fn is_built(&self) -> bool {
        self.built
    }

    /// Skipped files counted by reason.
    pub fn skipped(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for reason in self.files.values().filter_map(FileEntry::skipped) {
            *counts.entry(reason).or_default() += 1;
        }
        counts
    }

    /// Files whose facts the per-file cap cut short.
    pub fn capped_files(&self) -> usize {
        self.files
            .values()
            .filter(|entry| !entry.over_cap && entry.extracted.capped)
            .count()
    }

    /// Starts a sweep: lists candidates, records present languages, keeps provider-backed files.
    ///
    /// In a Git worktree, tracked files come from the index with their blob ids
    /// (`git ls-files -s`), those whose working copy differs lose theirs (`git diff-files`, which
    /// answers from Git's stat cache without reading contents), and untracked non-ignored files
    /// follow (`git ls-files --others --exclude-standard`). Without Git the bounded walk lists
    /// files without blob ids.
    fn list(&mut self) -> Sweep {
        let root = self.worktree.worktree_path();
        let listed = git_candidates(root)
            .unwrap_or_else(|| walk(root).into_iter().map(|path| (path, None)).collect());
        self.present.clear();
        let mut candidates = Vec::new();
        for (path, blob) in listed {
            let Some(language) = Language::for_path(&path) else {
                continue;
            };
            self.present.insert(language);
            if language.names().is_none() {
                continue;
            }
            if candidates.len() == MAX_LISTED_FILES {
                break;
            }
            candidates.push((path, language, blob));
        }
        candidates.sort_by(|a, b| path_bytes(&a.0).cmp(path_bytes(&b.0)));
        let listed = candidates.len();
        candidates.truncate(MAX_INDEXED_FILES);
        Sweep {
            candidates,
            listed,
            cursor: 0,
            elapsed: Duration::ZERO,
        }
    }

    /// Brings one candidate up to date. A clean tracked file costs nothing when its blob is
    /// already indexed and no read when the fact cache has it; any other file costs one `lstat`
    /// when unchanged.
    fn visit(&mut self, path: &Path, language: Language, blob: Option<Box<str>>) {
        let content = blob.map(ContentKey::GitBlob);
        if let Some(content) = &content {
            if self
                .files
                .get(path)
                .is_some_and(|entry| entry.content == *content && entry.language == language)
            {
                return;
            }
            let key = CacheKey::of(language, content.clone());
            let cached = key.and_then(|key| self.cache.lock().ok()?.get(&key));
            if let Some(extracted) = cached {
                self.reused += 1;
                return self.store(path, language, None, content.clone(), None, extracted);
            }
        }
        let metadata = match std::fs::symlink_metadata(self.worktree.worktree_path().join(path)) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return self.remove(path),
        };
        let stamp = stamp_of(&metadata);
        if content.is_none()
            && self.files.get(path).is_some_and(|entry| {
                entry.stamp == Some(stamp)
                    && entry.language == language
                    && matches!(entry.content, ContentKey::Digest(_))
            })
        {
            return;
        }
        self.reread += 1;
        if metadata.len() > MAX_FILE_BYTES as u64 {
            let content = content.unwrap_or(ContentKey::Digest([0; 32]));
            return self.store(path, language, Some(stamp), content, None, skipped("large"));
        }
        let limits =
            SourceReadLimits::new(MAX_SOURCE_PATH_BYTES, MAX_FILE_BYTES).expect("fixed limits");
        match read_authorized_source(&self.worktree, path, limits) {
            Ok(read) => self.ingest(path, language, Some(stamp), read.contents(), content),
            Err(ObservationError::TooLarge { .. }) => {
                let content = content.unwrap_or(ContentKey::Digest([0; 32]));
                self.store(path, language, Some(stamp), content, None, skipped("large"))
            }
            Err(_) => self.remove(path),
        }
    }

    /// Indexes `path` from `bytes` just read: the extraction comes from the fact cache when the
    /// content (`content`, else the digest of `bytes`) was extracted before, else it is extracted
    /// and cached. An unchanged digest only refreshes the stamp.
    fn ingest(
        &mut self,
        path: &Path,
        language: Language,
        stamp: Option<(u64, i128)>,
        bytes: &[u8],
        content: Option<ContentKey>,
    ) {
        let digest = *blake3::hash(bytes).as_bytes();
        if let Some(entry) = self.files.get_mut(path)
            && entry.digest == Some(digest)
            && entry.language == language
        {
            entry.stamp = stamp;
            if let Some(content) = content {
                // Same bytes, newly known by their blob: other worktrees may reuse them.
                if let Some(key) = CacheKey::of(language, content.clone())
                    && let Ok(mut cache) = self.cache.lock()
                {
                    cache.insert(key, entry.extracted.clone());
                }
                entry.content = content;
            }
            return;
        }
        let content = content.unwrap_or(ContentKey::Digest(digest));
        let Some(key) = CacheKey::of(language, content.clone()) else {
            return self.remove(path);
        };
        let cached = self.cache.lock().ok().and_then(|mut cache| cache.get(&key));
        let extracted = match cached {
            Some(extracted) => {
                self.reused += 1;
                extracted
            }
            None => {
                let Some(extracted) = extract(language, path, bytes) else {
                    return self.remove(path);
                };
                let extracted = Arc::new(extracted);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(key, extracted.clone());
                }
                extracted
            }
        };
        self.store(path, language, stamp, content, Some(digest), extracted);
    }

    /// Replaces `path`'s entry atomically: its old facts leave the postings before new ones enter.
    /// A file whose facts would pass [`MAX_FACTS_PER_WORKTREE`] is kept without facts.
    fn store(
        &mut self,
        path: &Path,
        language: Language,
        stamp: Option<(u64, i128)>,
        content: ContentKey,
        digest: Option<[u8; 32]>,
        extracted: Arc<Extracted>,
    ) {
        self.remove(path);
        let entry = FileEntry {
            language,
            stamp,
            content,
            digest,
            over_cap: self.facts + extracted.facts.len() > MAX_FACTS_PER_WORKTREE,
            extracted,
        };
        for fact in entry.facts() {
            self.postings
                .entry(fact.key.clone())
                .or_default()
                .insert(path.to_path_buf());
        }
        self.facts += entry.facts().len();
        self.files.insert(path.to_path_buf(), entry);
    }

    /// Drops `path` and its facts.
    fn remove(&mut self, path: &Path) {
        let Some(entry) = self.files.remove(path) else {
            return;
        };
        self.facts -= entry.facts().len();
        for fact in entry.facts() {
            if let Some(files) = self.postings.get_mut(&fact.key) {
                files.remove(path);
                if files.is_empty() {
                    self.postings.remove(&fact.key);
                }
            }
        }
    }
}

/// Name indexes of the worktrees the daemon's bindings use, most recently used first.
#[derive(Default)]
pub struct NameIndexes {
    /// At most [`MAX_WORKTREES`] indexes, keyed by worktree id and incarnation.
    recent: Vec<(WorktreeRef, Arc<Mutex<NameIndex>>)>,
    /// Extractions shared by every index, keyed by content.
    cache: Arc<Mutex<FactCache>>,
}

impl NameIndexes {
    /// The index of `worktree` (this incarnation), if one is held; never creates one.
    pub fn get(&self, worktree: &WorktreeRef) -> Option<Arc<Mutex<NameIndex>>> {
        self.recent
            .iter()
            .find(|(held, _)| {
                held.id() == worktree.id() && held.incarnation() == worktree.incarnation()
            })
            .map(|(_, index)| index.clone())
    }

    /// Whether the shared fact cache holds any extraction (another worktree was indexed).
    pub fn has_cached_facts(&self) -> bool {
        self.cache.lock().is_ok_and(|cache| !cache.is_empty())
    }

    /// Whether an index of `worktree` (this incarnation) is held.
    pub fn contains(&self, worktree: &WorktreeRef) -> bool {
        self.recent.iter().any(|(held, _)| {
            held.id() == worktree.id() && held.incarnation() == worktree.incarnation()
        })
    }

    /// The index of `worktree`, created empty on first use. A recreated worktree (same path, new
    /// incarnation) never reuses its predecessor's index; the least recently used index beyond
    /// [`MAX_WORKTREES`] is dropped.
    pub fn for_worktree(&mut self, worktree: &WorktreeRef) -> Arc<Mutex<NameIndex>> {
        let same = |held: &WorktreeRef| {
            held.id() == worktree.id() && held.incarnation() == worktree.incarnation()
        };
        if let Some(at) = self.recent.iter().position(|(held, _)| same(held)) {
            let entry = self.recent.remove(at);
            let index = entry.1.clone();
            self.recent.insert(0, entry);
            return index;
        }
        self.recent
            .retain(|(held, _)| held.worktree_path() != worktree.worktree_path());
        let index = Arc::new(Mutex::new(NameIndex::with_cache(
            worktree.clone(),
            self.cache.clone(),
        )));
        self.recent.insert(0, (worktree.clone(), index.clone()));
        self.recent.truncate(MAX_WORKTREES);
        index
    }
}

/// The extraction of `bytes` for `path` in `language`, facts sorted; `None` without a provider.
fn extract(language: Language, path: &Path, bytes: &[u8]) -> Option<Extracted> {
    let provider = language.names()?;
    let Ok(source) = std::str::from_utf8(bytes) else {
        return Some(Extracted {
            facts: Box::new([]),
            skipped: Some("non-utf8"),
            capped: false,
        });
    };
    let mut sink = FactSink::new();
    let verdict = provider.extract(path, source, &mut sink);
    let capped = sink.is_capped();
    let mut facts = sink.into_facts();
    Some(match verdict {
        FileVerdict::Skipped(reason) => Extracted {
            facts: Box::new([]),
            skipped: Some(reason),
            capped: false,
        },
        FileVerdict::Indexed => {
            facts.sort_by(|a, b| {
                (a.line, a.column, a.key.namespace, &a.key.name).cmp(&(
                    b.line,
                    b.column,
                    b.key.namespace,
                    &b.key.name,
                ))
            });
            Extracted {
                facts: facts.into_boxed_slice(),
                skipped: None,
                capped,
            }
        }
    })
}

/// A factless extraction skipped for `reason`.
fn skipped(reason: &'static str) -> Arc<Extracted> {
    Arc::new(Extracted {
        facts: Box::new([]),
        skipped: Some(reason),
        capped: false,
    })
}

/// Candidate paths of a Git worktree with the blob id of each clean tracked file, or `None` when
/// `root` is not a Git worktree or a query fails.
fn git_candidates(root: &Path) -> Option<Vec<(PathBuf, Option<Box<str>>)>> {
    use crate::checks::fingerprint::git_output;
    let staged = git_output(root, &["ls-files", "-s", "-z", "--cached"])?;
    let modified = git_output(root, &["diff-files", "--name-only", "-z"])?;
    let others = git_output(root, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    let modified: HashSet<&[u8]> = modified
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    let mut found = Vec::new();
    for record in staged.split(|byte| *byte == 0) {
        // `<mode> <oid> <stage>\t<path>`; conflicted paths (stage > 0) carry no single blob.
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let (meta, path) = (&record[..tab], &record[tab + 1..]);
        let mut fields = meta.split(|byte| *byte == b' ');
        let (Some(_mode), Some(oid), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let blob = (stage == b"0" && !modified.contains(path))
            .then(|| String::from_utf8_lossy(oid).into());
        if found
            .last()
            .is_some_and(|(last, _): &(PathBuf, Option<Box<str>>)| {
                last.as_os_str().as_bytes() == path
            })
        {
            continue;
        }
        found.push((PathBuf::from(OsStr::from_bytes(path)), blob));
    }
    found.extend(
        others
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| (PathBuf::from(OsStr::from_bytes(path)), None)),
    );
    Some(found)
}

/// Raw path bytes, the index's sort key for files.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// `(length, mtime in ns)` of a stat result.
fn stamp_of(metadata: &std::fs::Metadata) -> (u64, i128) {
    (
        metadata.len(),
        i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
    )
}

/// Files of registered languages below `root` without Git: breadth-first, sorted, skipping hidden
/// and [`SKIPPED_DIRECTORIES`], never following symlinks, bounded by [`WALK_MAX_DIRECTORIES`] and
/// [`MAX_LISTED_FILES`].
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([PathBuf::new()]);
    let mut visited = 0;
    while let Some(directory) = queue.pop_front() {
        visited += 1;
        let Ok(entries) = std::fs::read_dir(root.join(&directory)) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let path = directory.join(&name);
            if kind.is_dir() {
                let name = name.to_string_lossy();
                if !name.starts_with('.') && !SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
                    queue.push_back(path);
                }
            } else if kind.is_file() && Language::for_path(&path).is_some() {
                found.push(path);
                if found.len() == MAX_LISTED_FILES {
                    return found;
                }
            }
        }
        if visited >= WALK_MAX_DIRECTORIES {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{
        names::{Certainty, MAX_FACTS_PER_FILE, MAX_NAME_BYTES, ns},
        testing::{self, ALPHA, BETA, DELTA, GAMMA},
    };

    /// A scratch worktree removed when dropped.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        /// Removes the tree, best effort.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for Scratch {
        type Target = Path;
        /// The canonical root.
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    /// A fresh canonical scratch worktree (no Git: candidates come from the bounded walk).
    fn scratch(tag: &str) -> Scratch {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agent-ide-names-{tag}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Scratch(std::fs::canonicalize(root).unwrap())
    }

    /// An unverified fixture worktree reference over `root`.
    fn worktree(root: &Path, incarnation: u64) -> WorktreeRef {
        WorktreeRef::from_discovery(root.into(), root.into(), ".git".into(), incarnation).unwrap()
    }

    /// Writes `files` below `root`.
    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    /// An index over `root` refreshed to completion.
    fn built(root: &Path) -> NameIndex {
        testing::install();
        let mut index = NameIndex::new(worktree(root, 1));
        assert_eq!(index.refresh(far()), IndexState::Ready);
        index
    }

    /// A deadline no test reaches.
    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// `(file, line, column, role)` of each site, for compact assertions.
    fn rows(sites: &[Site]) -> Vec<(String, u32, u32, Role)> {
        sites
            .iter()
            .map(|site| {
                (
                    site.file.display().to_string(),
                    site.fact.line,
                    site.fact.column,
                    site.fact.role,
                )
            })
            .collect()
    }

    /// A class name defined in one language joins its exact and heuristic uses in another.
    #[test]
    fn joins_across_languages_by_namespace_and_name() {
        let root = scratch("join");
        write(
            &root,
            &[
                ("styles/a.alpha", "@btn @card\n"),
                ("src/b.beta", "x use:btn\n~btn use:card\n"),
                ("src/c.delta", "use:btn\n"),
            ],
        );
        let index = built(&root);
        let sites = index.sites(&NameKey::global(ns::CLASS, "btn"));
        assert_eq!(
            rows(&sites),
            [
                ("src/b.beta".into(), 1, 3, Role::Use),
                ("src/b.beta".into(), 2, 1, Role::Use),
                ("styles/a.alpha".into(), 1, 1, Role::Define),
            ]
        );
        assert_eq!(sites[0].language, BETA);
        assert_eq!(sites[2].language, ALPHA);
        assert_eq!(sites[0].fact.certainty, Certainty::Exact);
        assert_eq!(sites[1].fact.certainty, Certainty::Heuristic("tilde"));
        assert_eq!(
            index.keys_named("btn", None),
            [KeySummary {
                key: NameKey::global(ns::CLASS, "btn"),
                defines: 1,
                uses: 2,
                files: 2,
            }]
        );
        assert_eq!(
            rows(&index.facts_in(Path::new("src/b.beta"), LineRange::new(2, 2))),
            [
                ("src/b.beta".into(), 2, 1, Role::Use),
                ("src/b.beta".into(), 2, 6, Role::Use),
            ]
        );
    }

    /// The same spelling in two namespaces makes two keys that never meet.
    #[test]
    fn namespaces_never_collide() {
        let root = scratch("namespaces");
        write(
            &root,
            &[("a.alpha", "@main #main\n"), ("b.gamma", "#main\n")],
        );
        let index = built(&root);
        let class = index.sites(&NameKey::global(ns::CLASS, "main"));
        let id = index.sites(&NameKey::global(ns::ELEMENT_ID, "main"));
        assert_eq!(rows(&class), [("a.alpha".into(), 1, 1, Role::Define)]);
        assert_eq!(
            rows(&id),
            [
                ("a.alpha".into(), 1, 7, Role::Define),
                ("b.gamma".into(), 1, 1, Role::Use),
            ]
        );
        let keys = index.keys_named("main", None);
        assert_eq!(keys.len(), 2);
        assert_eq!(index.keys_named("main", Some(ns::ELEMENT_ID)).len(), 1);
        assert!(
            index
                .keys_named("main", Some(ns::STYLE_VARIABLE))
                .is_empty()
        );
    }

    /// The same name in two domains makes two keys; the global key sees neither.
    #[test]
    fn domains_never_collide() {
        let root = scratch("domains");
        write(
            &root,
            &[
                ("a.alpha", "@btn/x.mod @btn\n"),
                ("b.beta", "use:btn/x.mod use:btn/y.mod\n"),
            ],
        );
        let index = built(&root);
        let scoped = |domain: &str| NameKey {
            domain: domain.into(),
            ..NameKey::global(ns::CLASS, "btn")
        };
        assert_eq!(
            rows(&index.sites(&scoped("x.mod"))),
            [
                ("a.alpha".into(), 1, 1, Role::Define),
                ("b.beta".into(), 1, 1, Role::Use),
            ]
        );
        assert_eq!(
            rows(&index.sites(&scoped("y.mod"))),
            [("b.beta".into(), 1, 15, Role::Use)]
        );
        assert_eq!(
            rows(&index.sites(&NameKey::global(ns::CLASS, "btn"))),
            [("a.alpha".into(), 1, 12, Role::Define)]
        );
        let domains: Vec<_> = index
            .keys_named("btn", Some(ns::CLASS))
            .into_iter()
            .map(|summary| summary.key.domain.to_string())
            .collect();
        assert_eq!(domains, ["", "x.mod", "y.mod"]);
    }

    /// Two indexes built from the same files in different creation orders answer identically, in
    /// path-byte order (`a-b` before `a/b`, unlike component order).
    #[test]
    fn order_is_independent_of_insertion_order() {
        let files = [
            ("a/b.beta", "use:k\n"),
            ("a-b.beta", "use:k\n"),
            ("z.alpha", "@k\n"),
            ("a.alpha", "use:k @k\n"),
        ];
        let first = scratch("order-1");
        write(&first, &files);
        let second = scratch("order-2");
        let mut reversed = files;
        reversed.reverse();
        write(&second, &reversed);
        let key = NameKey::global(ns::CLASS, "k");
        let (one, two) = (built(&first).sites(&key), built(&second).sites(&key));
        assert_eq!(rows(&one), rows(&two));
        let files: Vec<_> = rows(&one).into_iter().map(|row| row.0).collect();
        assert_eq!(files, ["a-b.beta", "a.alpha", "a/b.beta", "z.alpha"]);
    }

    /// A content change replaces every fact of the file, including keys that disappeared.
    #[test]
    fn digest_change_replaces_all_file_facts() {
        let root = scratch("digest");
        write(&root, &[("a.alpha", "@old @kept\n")]);
        let mut index = built(&root);
        std::thread::sleep(Duration::from_millis(5));
        write(&root, &[("a.alpha", "@kept\n@new\n")]);
        assert_eq!(index.refresh(far()), IndexState::Ready);
        assert!(index.sites(&NameKey::global(ns::CLASS, "old")).is_empty());
        assert!(index.keys_named("old", None).is_empty());
        assert_eq!(
            rows(&index.sites(&NameKey::global(ns::CLASS, "kept"))),
            [("a.alpha".into(), 1, 1, Role::Define)]
        );
        assert_eq!(
            rows(&index.sites(&NameKey::global(ns::CLASS, "new"))),
            [("a.alpha".into(), 2, 1, Role::Define)]
        );
    }

    /// A deleted file loses all its facts on the next refresh.
    #[test]
    fn deleted_file_drops_facts() {
        let root = scratch("deleted");
        write(&root, &[("a.alpha", "@btn\n"), ("b.beta", "use:btn\n")]);
        let mut index = built(&root);
        std::fs::remove_file(root.join("b.beta")).unwrap();
        assert_eq!(index.refresh(far()), IndexState::Ready);
        assert_eq!(
            rows(&index.sites(&NameKey::global(ns::CLASS, "btn"))),
            [("a.alpha".into(), 1, 1, Role::Define)]
        );
        std::fs::remove_file(root.join("a.alpha")).unwrap();
        index.refresh(far());
        assert!(index.keys_named("btn", None).is_empty());
    }

    /// Bytes that no longer match the indexed digest fail verification and re-extract the file,
    /// so the caller's re-query sees the current facts even before the next sweep.
    #[test]
    fn verify_rejects_stale_row() {
        let root = scratch("verify");
        write(&root, &[("b.beta", "use:btn\n")]);
        let mut index = built(&root);
        let path = Path::new("b.beta");
        assert!(index.verify(path, b"use:btn\n"));
        assert!(!index.verify(path, b"\nuse:card\n"));
        assert!(index.sites(&NameKey::global(ns::CLASS, "btn")).is_empty());
        assert_eq!(
            rows(&index.sites(&NameKey::global(ns::CLASS, "card"))),
            [("b.beta".into(), 2, 1, Role::Use)]
        );
        assert!(index.verify(path, b"\nuse:card\n"));
    }

    /// Displayed rows are proven against the files: an edited file is re-extracted before rows
    /// are shown, a deleted one's rows are dropped and counted, rows past the limit are counted.
    #[test]
    fn proven_sites_never_show_stale_rows() {
        let root = scratch("proven");
        write(
            &root,
            &[
                ("a.beta", "use:btn\n"),
                ("b.beta", "use:btn\n"),
                ("c.beta", "use:btn\n"),
            ],
        );
        let mut index = built(&root);
        let key = NameKey::global(ns::CLASS, "btn");
        // Same size, different line: only the digest can tell.
        write(&root, &[("a.beta", "\nuse:btn")]);
        std::fs::remove_file(root.join("b.beta")).unwrap();
        let proven = index.proven_sites(&key, None, 2);
        let shown: Vec<_> = proven
            .sites
            .iter()
            .map(|shown| {
                (
                    shown.site.file.display().to_string(),
                    shown.site.fact.line,
                    shown.text.as_str(),
                )
            })
            .collect();
        assert_eq!(shown, [("a.beta".to_owned(), 2, "use:btn")]);
        assert_eq!((proven.dropped, proven.more), (1, 1));
        assert_eq!(
            index.source(Path::new("c.beta")).as_deref(),
            Some("use:btn\n")
        );
        assert_eq!(index.source(Path::new("b.beta")), None);
    }

    /// Runs `git` with a fixed identity in `root`.
    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("/usr/bin/git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Two worktrees of one repository share facts by content: the second builds from the cache
    /// without reading a clean tracked file; an unstaged edit or a branch that changes a file
    /// gives that worktree its own facts, and the first is unaffected.
    #[test]
    fn worktrees_share_facts_by_content() {
        testing::install();
        let first = scratch("share-a");
        git(&first, &["init", "--quiet"]);
        write(&first, &[("a.alpha", "@btn\n"), ("b.beta", "use:btn\n")]);
        git(&first, &["add", "--", "."]);
        git(&first, &["commit", "--quiet", "-m", "one"]);
        let second = scratch("share-b");
        std::fs::remove_dir(&*second).unwrap();
        git(
            &first,
            &["worktree", "add", "--quiet", "-b", "other"]
                .into_iter()
                .chain([second.to_str().unwrap()])
                .collect::<Vec<_>>(),
        );
        let cache: Arc<Mutex<FactCache>> = Arc::default();
        let mut a = NameIndex::with_cache(worktree(&first, 1), cache.clone());
        assert_eq!(a.refresh(far()), IndexState::Ready);
        assert_eq!((a.last_reread(), a.last_reused()), (2, 0));
        let mut b = NameIndex::with_cache(worktree(&second, 1), cache.clone());
        assert_eq!(b.refresh(far()), IndexState::Ready);
        assert_eq!(
            (b.last_reread(), b.last_reused()),
            (0, 2),
            "no read for clean files"
        );
        let key = NameKey::global(ns::CLASS, "btn");
        assert_eq!(rows(&a.sites(&key)), rows(&b.sites(&key)));
        assert!(
            b.verify(Path::new("b.beta"), b"use:btn\n"),
            "the blob's bytes prove it"
        );

        // An unstaged edit in the second worktree only.
        std::thread::sleep(Duration::from_millis(5));
        write(&second, &[("b.beta", "\nuse:btn\n")]);
        assert_eq!(b.refresh(far()), IndexState::Ready);
        assert_eq!(b.last_reread(), 1);
        assert_eq!(a.refresh(far()), IndexState::Ready);
        assert_eq!(a.last_reread(), 0);
        let lines = |index: &NameIndex| -> Vec<u32> {
            index
                .sites(&key)
                .iter()
                .map(|site| site.fact.line)
                .collect()
        };
        assert_eq!((lines(&a), lines(&b)), (vec![1, 1], vec![1, 2]));

        // A branch that changes a file: a new blob, its own facts.
        write(&second, &[("a.alpha", "@btn @card\n")]);
        git(&second, &["commit", "--quiet", "-am", "two"]);
        assert_eq!(b.refresh(far()), IndexState::Ready);
        assert_eq!(b.keys_named("card", None).len(), 1);
        a.refresh(far());
        assert!(a.keys_named("card", None).is_empty());
        git(
            &first,
            &["worktree", "remove", "--force", second.to_str().unwrap()],
        );
    }

    /// Cached facts are keyed by the extractor revision, so a revision bump misses them; the
    /// cache evicts unreferenced extractions first when a bound is passed.
    #[test]
    fn fact_cache_keys_by_revision_and_evicts_unreferenced_first() {
        testing::install();
        let content = ContentKey::GitBlob("abc".into());
        let key = CacheKey::of(ALPHA, content.clone()).unwrap();
        assert_eq!(key.revision, "1");
        let mut cache = FactCache {
            max_entries: 2,
            ..FactCache::default()
        };
        let held = skipped("held");
        cache.insert(key.clone(), held.clone());
        let bumped = CacheKey {
            revision: "2",
            ..key.clone()
        };
        assert!(cache.get(&bumped).is_none());
        assert!(cache.get(&key).is_some());
        let other = |name: &str| CacheKey {
            content: ContentKey::GitBlob(name.into()),
            ..key.clone()
        };
        cache.insert(other("b"), skipped("b"));
        cache.insert(other("c"), skipped("c"));
        // Over the bound: the unreferenced, least recently used extraction goes first; the one a
        // live index still holds stays.
        assert!(cache.get(&key).is_some());
        assert!(cache.get(&other("b")).is_none());
        assert_eq!(cache.len(), 1);
        drop(held);
    }

    /// The sink drops invalid names and stops at the per-file cap; a capped file is counted.
    #[test]
    fn sink_caps_and_validates_names() {
        let mut sink = FactSink::new();
        let fact = |name: &str, domain: &str| NameFact {
            key: NameKey {
                domain: domain.into(),
                ..NameKey::global(ns::CLASS, name)
            },
            role: Role::Use,
            line: 1,
            column: 1,
            certainty: Certainty::Exact,
        };
        for invalid in [
            fact("", ""),
            fact("a b", ""),
            fact("a\u{7}", ""),
            fact("a", "x y"),
            fact(&"x".repeat(MAX_NAME_BYTES + 1), ""),
            NameFact {
                column: 0,
                ..fact("a", "")
            },
        ] {
            assert!(sink.push(invalid));
        }
        assert_eq!((sink.rejected(), sink.facts().len()), (6, 0));
        assert!(sink.push(fact(&"x".repeat(MAX_NAME_BYTES), "src/a.mod")));
        while sink.push(fact("k", "")) {}
        assert!(sink.is_full() && sink.is_capped());
        assert_eq!(sink.facts().len(), MAX_FACTS_PER_FILE);

        let root = scratch("cap");
        let many = (0..MAX_FACTS_PER_FILE + 10)
            .map(|index| format!("@n{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        write(&root, &[("a.alpha", &many), ("b.alpha", "@ok\n")]);
        let index = built(&root);
        assert_eq!(index.capped_files(), 1);
        assert_eq!(index.facts, MAX_FACTS_PER_FILE + 1);
        assert!(index.keys_named("n5009", None).is_empty());
        assert_eq!(index.keys_named("n0", None).len(), 1);
    }

    /// A sweep cut by its deadline reports `Building`, then `Partial` once the build budget is
    /// spent, and the next refresh resumes where it stopped instead of starting over.
    #[test]
    fn budget_exhaustion_is_partial_and_resumes() {
        let root = scratch("budget");
        write(
            &root,
            &[
                ("a.alpha", "@a\n"),
                ("b.alpha", "@b\n"),
                ("c.alpha", "@c\n"),
            ],
        );
        testing::install();
        let mut index = NameIndex::new(worktree(&root, 1));
        assert_eq!(index.refresh(Instant::now()), IndexState::Building);
        assert!(index.keys_named("a", None).is_empty());
        index.sweep.as_mut().unwrap().elapsed = BUILD_BUDGET;
        assert_eq!(
            index.refresh(Instant::now()),
            IndexState::Partial {
                indexed: 0,
                listed: 3
            }
        );
        // Visit exactly one file, then stop.
        let sweep = index.sweep.as_mut().unwrap();
        sweep.cursor = 1;
        let (path, language, blob) = sweep.candidates[0].clone();
        index.visit(&path, language, blob);
        assert_eq!(index.keys_named("a", None).len(), 1);
        assert_eq!(index.refresh(far()), IndexState::Ready);
        assert_eq!(index.keys_named("c", None).len(), 1);
        assert!(index.sweep.is_none());
    }

    /// Languages present without a provider, or whose provider covers other namespaces, are
    /// listed as uncovered; files of no registered language and skipped files are counted apart.
    #[test]
    fn uncovered_lists_present_language_without_provider() {
        let root = scratch("uncovered");
        write(
            &root,
            &[
                ("a.alpha", "@btn\n"),
                ("b.gamma", "#top\n"),
                ("c.delta", "use:btn\n"),
                ("d.beta", "#!skip\nuse:btn\n"),
                ("README.md", "@btn\n"),
            ],
        );
        std::fs::write(root.join("e.beta"), b"use:btn \xff\n").unwrap();
        let index = built(&root);
        assert_eq!(index.uncovered(ns::CLASS), [GAMMA, DELTA]);
        assert_eq!(index.uncovered(ns::ELEMENT_ID), [BETA, DELTA]);
        assert_eq!(
            index.uncovered(ns::STYLE_VARIABLE),
            [ALPHA, BETA, GAMMA, DELTA]
        );
        assert_eq!(
            index.skipped(),
            BTreeMap::from([("generated", 1), ("non-utf8", 1)])
        );
        assert_eq!(index.keys_named("btn", None)[0].uses, 0);
    }

    /// At most four worktree indexes are kept, the least recently used goes first, and a new
    /// incarnation at the same path replaces its predecessor.
    #[test]
    fn lru_keeps_at_most_four_worktrees() {
        let roots: Vec<Scratch> = (0..5).map(|n| scratch(&format!("lru-{n}"))).collect();
        let mut indexes = NameIndexes::default();
        let first = indexes.for_worktree(&worktree(&roots[0], 1));
        for root in &roots[1..4] {
            indexes.for_worktree(&worktree(root, 1));
        }
        assert!(Arc::ptr_eq(
            &first,
            &indexes.for_worktree(&worktree(&roots[0], 1))
        ));
        indexes.for_worktree(&worktree(&roots[4], 1));
        assert_eq!(indexes.recent.len(), MAX_WORKTREES);
        let kept: Vec<&Path> = indexes
            .recent
            .iter()
            .map(|(held, _)| held.worktree_path())
            .collect();
        assert!(!kept.contains(&&*roots[1]), "the LRU entry goes");
        assert!(kept.contains(&&*roots[0]));
        let recreated = indexes.for_worktree(&worktree(&roots[0], 2));
        assert!(!Arc::ptr_eq(&first, &recreated));
        assert_eq!(
            indexes
                .recent
                .iter()
                .filter(|(held, _)| held.worktree_path() == &*roots[0])
                .count(),
            1
        );
    }

    /// Git lists tracked and untracked non-ignored files; ignored directories never enter.
    #[test]
    fn git_listing_excludes_ignored_files() {
        let root = scratch("git");
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .arg("-C")
                    .arg(&*root)
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        write(
            &root,
            &[
                (".gitignore", "build/\n"),
                ("a.alpha", "@btn\n"),
                ("build/b.beta", "use:btn\n"),
                ("c.beta", "use:btn\n"),
            ],
        );
        let index = built(&root);
        let files: Vec<_> = rows(&index.sites(&NameKey::global(ns::CLASS, "btn")))
            .into_iter()
            .map(|row| row.0)
            .collect();
        assert_eq!(files, ["a.alpha", "c.beta"]);
    }

    /// A synthetic 10 000-file worktree builds inside the budget and a warm refresh with nothing
    /// changed meets [`WARM_REFRESH_TARGET`].
    #[test]
    fn ten_thousand_files_build_cold_and_refresh_warm() {
        let root = scratch("scale");
        for index in 0..10_000 {
            let (path, text) = if index % 2 == 0 {
                (
                    format!("d{}/f{index}.alpha", index % 100),
                    format!("@c{index} @shared\n"),
                )
            } else {
                (
                    format!("d{}/f{index}.beta", index % 100),
                    format!("use:c{} use:shared\n", index - 1),
                )
            };
            write(&root, &[(&path, &text)]);
        }
        testing::install();
        let mut index = NameIndex::new(worktree(&root, 1));
        let cold = Instant::now();
        assert_eq!(index.refresh(cold + BUILD_BUDGET), IndexState::Ready);
        let cold = cold.elapsed();
        let warm = Instant::now();
        assert_eq!(index.refresh(far()), IndexState::Ready);
        let warm = warm.elapsed();
        eprintln!("cold {cold:?}, warm {warm:?}");
        assert!(cold < Duration::from_secs(5), "cold build {cold:?}");
        assert!(warm <= WARM_REFRESH_TARGET, "warm refresh {warm:?}");
        assert_eq!(index.keys_named("shared", None)[0].files, 10_000);
    }
}
