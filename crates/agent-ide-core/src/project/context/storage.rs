//! Project-private SQLite index; generations publish transactionally and never index themselves.

use super::{Candidate, FileRecord, IndexSnapshot};
use crate::project::structure::{self, Detail, NodeKind, StructureNode, StructureOptions};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const APPLICATION_ID: i64 = 0x41494443;
const SCHEMA: i64 = 1;

/// Owns one project's rebuildable local index. This is an operator API, not an MCP grant.
pub struct ContextIndex {
    pub(super) root: PathBuf,
    pub(super) connection: Connection,
    /// Exclusive writer lease; read-only query handles do not acquire it.
    _writer_lock: Option<fs::File>,
}

/// Makes database failures ordinary recoverable I/O errors at the operator boundary.
fn db(error: rusqlite::Error) -> io::Error {
    io::Error::other(error.to_string())
}

/// Rejects links, nonregular files and foreign owners before opening writable state.
fn private_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid reads process identity without retaining pointers or changing state.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "context state must be an owner-only regular file",
        ));
    }
    Ok(())
}

/// Adds the explicit project rule without truncating existing ignore bytes or following links.
fn ensure_ignore(root: &Path) -> io::Result<()> {
    let path = root.join(".gitignore");
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o644)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("gitignore is not a regular file"));
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err(io::Error::other("gitignore exceeds initialization limit"));
    }
    let exists = bytes.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        matches!(
            line,
            b"/.agent-ide/" | b".agent-ide/" | b"/.agent-ide" | b".agent-ide"
        )
    });
    if !exists {
        let mut addition = Vec::new();
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            addition.push(b'\n');
        }
        addition.extend_from_slice(b"/.agent-ide/\n");
        file.write_all(&addition)?;
        file.sync_all()?;
    }
    Ok(())
}

impl ContextIndex {
    /// Initializes `.agent-ide/index.sqlite` and the root ignore rule. Never migrates global state.
    /// Refuses symlinks, foreign/nonprivate state and incompatible database ownership/schema.
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if root.to_str().is_none() {
            return Err(io::Error::other("context root must be UTF-8"));
        }
        if !root.is_dir() {
            return Err(io::Error::other("context root is not a directory"));
        }
        let directory = root.join(".agent-ide");
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        // SAFETY: geteuid only returns the current effective user identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                ".agent-ide must be an owner-only directory, not a symlink",
            ));
        }
        // Persist the newly created state-directory entry before any model reservation is durable.
        fs::File::open(&root)?.sync_all()?;
        let lock_path = directory.join("init.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)?;
        private_file(&lock_path)?;
        use std::os::fd::AsRawFd;
        // SAFETY: lock owns a live descriptor; flock does not retain borrowed pointers. Closing
        // the retained file releases exclusive writer ownership on drop and every error path.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let path = directory.join("index.sqlite");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        private_file(&path)?;
        let is_empty = fs::metadata(&path)?.len() == 0;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(db)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(db)?;
        let application: i64 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(db)?;
        let tables:i64=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0)).map_err(db)?;
        let initial_schema: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(db)?;
        let fresh = is_empty || (application == 0 && initial_schema == 0 && tables == 0);
        if !fresh && application != APPLICATION_ID {
            return Err(io::Error::other(
                "context database belongs to another application",
            ));
        }
        if fresh {
            connection.execute_batch(&format!("BEGIN IMMEDIATE; PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={SCHEMA}; CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE files (path TEXT PRIMARY KEY, record TEXT NOT NULL, generation INTEGER NOT NULL);")).map_err(db)?;
            connection
                .execute(
                    "INSERT INTO meta VALUES ('root',?1)",
                    params![root.to_string_lossy()],
                )
                .map_err(db)?;
            connection.execute_batch("COMMIT").map_err(db)?;
        }
        let schema: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db)?;
        if schema != SCHEMA {
            return Err(io::Error::other(
                "context index schema requires explicit rebuild",
            ));
        }
        let recorded: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='root'", [], |r| r.get(0))
            .optional()
            .map_err(db)?;
        let recorded = match recorded {
            Some(value) => value,
            None => {
                let count: i64 = connection
                    .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                    .map_err(db)?;
                let meta: i64 = connection
                    .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
                    .map_err(db)?;
                if count != 0 || meta != 0 {
                    return Err(io::Error::other("index root identity missing"));
                }
                connection
                    .execute(
                        "INSERT INTO meta VALUES ('root',?1)",
                        params![root.to_string_lossy()],
                    )
                    .map_err(db)?;
                root.to_string_lossy().into_owned()
            }
        };
        if recorded != root.to_string_lossy() {
            return Err(io::Error::other(
                "context index belongs to another root; rebuild it explicitly",
            ));
        }
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(db)?;
        ensure_ignore(&root)?;
        Ok(Self {
            root,
            connection,
            _writer_lock: Some(lock),
        })
    }

    /// Opens an existing project index without creating files, changing ignore rules or refreshing sources.
    /// The read transaction pins a coherent snapshot while a WAL writer publishes new generations.
    pub fn open_existing(root: &Path) -> io::Result<Option<Self>> {
        let root = root.canonicalize()?;
        let directory = root.join(".agent-ide");
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // SAFETY: geteuid only reads the current effective user identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other("context directory is not private"));
        }
        let path = directory.join("index.sqlite");
        match private_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        if fs::metadata(&path)?.len() == 0 {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(db)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(db)?;
        connection.execute_batch("BEGIN").map_err(db)?;
        let application: i64 = connection
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(db)?;
        let schema: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(db)?;
        if application == 0 && schema == 0 {
            let count:i64=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0)).map_err(db)?;
            if count == 0 {
                return Ok(None);
            }
        }
        if application != APPLICATION_ID || schema != SCHEMA {
            return Err(io::Error::other("context index identity mismatch"));
        }
        let recorded: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='root'", [], |r| r.get(0))
            .optional()
            .map_err(db)?;
        let Some(recorded) = recorded else {
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .map_err(db)?;
            if count == 0 {
                return Ok(None);
            }
            return Err(io::Error::other("index root identity missing"));
        };
        if recorded != root.to_string_lossy() {
            return Err(io::Error::other("context index root mismatch"));
        }
        Ok(Some(Self {
            root,
            connection,
            _writer_lock: None,
        }))
    }

    /// Rejects mutation APIs on a reader handle before journal or database side effects.
    pub(super) fn require_writer(&self) -> io::Result<()> {
        if self._writer_lock.is_some() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "index writer lease required",
            ))
        }
    }

    /// Builds an explicitly bounded snapshot; incomplete discovery never silently deletes old rows.
    /// All source paths are relative; symlinks remain unexpanded by the existing builder.
    pub fn refresh(&mut self, options: &StructureOptions) -> io::Result<IndexSnapshot> {
        self.refresh_cancellable(options, &|| false)
    }

    /// Stops between discovery/source/transaction units without publishing a partial generation.
    pub fn refresh_cancellable(
        &mut self,
        options: &StructureOptions,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<IndexSnapshot> {
        let check = || {
            if cancelled() {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "index refresh cancelled",
                ))
            } else {
                Ok(())
            }
        };
        check()?;
        let mut options = options.clone();
        options.include_files = true;
        options.include_symbols = true;
        options.include_header = true;
        options.include_imports = true;
        options.include_skeleton = false;
        options.include_line_count = false;
        let cache_key = format!(
            "{}:{}",
            extractor_identity()?,
            serde_json::to_string(&options).map_err(io::Error::other)?
        );
        let previous: Option<String> = self
            .connection
            .query_row("SELECT value FROM meta WHERE key='extractor'", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db)?;
        let mut cache = std::collections::BTreeMap::new();
        if previous.as_deref() == Some(cache_key.as_str()) {
            let mut statement = self
                .connection
                .prepare("SELECT record FROM files")
                .map_err(db)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db)?;
            for row in rows {
                check()?;
                let record: FileRecord =
                    serde_json::from_str(&row.map_err(db)?).map_err(io::Error::other)?;
                if record.content_hash.is_some()
                    && !record.truncated
                    && matches!(record.symbols, Some(Detail::Available(_)))
                    && !matches!(record.header, Detail::Unavailable { .. })
                    && !matches!(record.imports, Detail::Unavailable { .. })
                {
                    cache.insert(
                        record.path.clone(),
                        StructureNode {
                            path: record.path,
                            kind: NodeKind::File,
                            children: Vec::new(),
                            collapsed: false,
                            truncated: false,
                            size_bytes: None,
                            line_count: None,
                            language: record.language,
                            source_hash: record.content_hash,
                            header: record.header,
                            imports: record.imports,
                            symbols: record.symbols,
                            skeleton: Detail::Disabled,
                            diff: Detail::Disabled,
                        },
                    );
                }
            }
        }
        let structure =
            structure::build_structure_cancellable(&self.root, &options, &cache, cancelled)?;
        let mut records = Vec::new();
        // A failed import parse does not mean files were omitted from the directory walk.
        let mut discovery_complete = structure
            .warnings
            .iter()
            .all(|warning| warning.reason == "source import scan unavailable");
        collect(
            &structure.tree,
            &self.root,
            &mut records,
            &mut discovery_complete,
        );
        // Textual scripts, manifests and documentation remain useful without a language parser.
        let mut plain_remaining = options.max_total_read_bytes;
        for record in &mut records {
            check()?;
            if record.content_hash.is_none()
                && (eligible_plain_text(&record.path)
                    || extensionless_script(&self.root, &record.path))
            {
                let limit = options.max_file_bytes.min(plain_remaining);
                let result = read_plain_text(&self.root.join(&record.path), limit);
                match result {
                    Ok(bytes) => {
                        plain_remaining = plain_remaining.saturating_sub(bytes.len());
                        record.content_hash = Some(blake3::hash(&bytes).to_hex().to_string());
                    }
                    Err(_) => {
                        record.truncated = true;
                        discovery_complete = false;
                    }
                }
            }
        }
        let details_complete = records.iter().all(|record| {
            !record.truncated
                && record.content_hash.is_some()
                && !matches!(record.symbols, Some(Detail::Unavailable { .. }))
        });
        let transaction = self.connection.transaction().map_err(db)?;
        let old: Option<String> = transaction
            .query_row("SELECT value FROM meta WHERE key='generation'", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db)?;
        let generation = old
            .as_deref()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(io::Error::other)?
            .checked_add(1)
            .ok_or_else(|| io::Error::other("generation overflow"))?;
        // An incomplete generation retains older rows for recovery but never returns them
        // as observations of the new generation. Only a complete walk proves deletions.
        for record in &records {
            check()?;
            transaction
                .execute(
                    "INSERT OR REPLACE INTO files(path,record,generation) VALUES (?1,?2,?3)",
                    params![
                        record.path.to_string_lossy(),
                        serde_json::to_string(record).map_err(io::Error::other)?,
                        generation
                    ],
                )
                .map_err(db)?;
        }
        if discovery_complete {
            transaction
                .execute(
                    "DELETE FROM files WHERE generation != ?1",
                    params![generation],
                )
                .map_err(db)?;
        }
        let snapshot = IndexSnapshot {
            generation,
            root: self.root.clone(),
            files: records.len(),
            reused_files: structure.summary.source_cache_hits,
            scope: options,
            discovery_complete,
            details_complete,
            warnings: structure
                .warnings
                .iter()
                .map(|warning| {
                    format!(
                        "{}: {}",
                        warning.path.as_deref().unwrap_or(Path::new(".")).display(),
                        warning.reason
                    )
                })
                .collect(),
        };
        for (key, value) in [
            ("generation", generation.to_string()),
            (
                "snapshot",
                serde_json::to_string(&snapshot).map_err(io::Error::other)?,
            ),
        ] {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO meta VALUES (?1,?2)",
                    params![key, value],
                )
                .map_err(db)?;
        }
        transaction
            .execute(
                "INSERT OR REPLACE INTO meta VALUES ('extractor', ?1)",
                params![cache_key],
            )
            .map_err(db)?;
        check()?;
        transaction.commit().map_err(db)?;
        Ok(snapshot)
    }

    /// Reads one published generation; absent means indexing has not completed yet.
    pub fn snapshot(&self) -> io::Result<Option<IndexSnapshot>> {
        let text: Option<String> = self
            .connection
            .query_row("SELECT value FROM meta WHERE key='snapshot'", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db)?;
        text.map(|text| serde_json::from_str(&text).map_err(io::Error::other))
            .transpose()
    }

    /// Returns only records observed in the current published generation.
    pub(super) fn records(&self) -> io::Result<Vec<FileRecord>> {
        let mut statement=self.connection.prepare("SELECT record FROM files WHERE generation=CAST((SELECT value FROM meta WHERE key='generation') AS INTEGER) ORDER BY path").map_err(db)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db)?
            .map(|row| serde_json::from_str(&row.map_err(db)?).map_err(io::Error::other))
            .collect()
    }

    /// Returns a bounded lexical shortlist over path and source-backed declaration cards.
    /// Does not label lexical matches as resolved links or verify current source freshness.
    pub fn search(&self, task: &str, limit: usize) -> io::Result<Vec<Candidate>> {
        if task.trim().is_empty() || task.len() > 64 * 1024 || limit == 0 || limit > 256 {
            return Err(io::Error::other(
                "task and candidate limit 1..256 are required",
            ));
        }
        if self.snapshot()?.is_none() {
            return Err(io::Error::other(
                "context index has no published generation",
            ));
        }
        let terms: std::collections::BTreeSet<_> = task
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.chars().count() > 1)
            .map(str::to_lowercase)
            .collect();
        let mut statement=self.connection.prepare("SELECT record FROM files WHERE generation=CAST((SELECT value FROM meta WHERE key='generation') AS INTEGER) ORDER BY path").map_err(db)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db)?;
        let mut candidates = Vec::new();
        for row in rows {
            let record: FileRecord =
                serde_json::from_str(&row.map_err(db)?).map_err(io::Error::other)?;
            let card = search_text(&record).to_lowercase();
            let reasons: Vec<_> = terms
                .iter()
                .filter(|term| card.contains(term.as_str()))
                .map(|term| format!("literal term: {term}"))
                .collect();
            if !reasons.is_empty() {
                candidates.push(Candidate {
                    file: record,
                    lexical_score: reasons.len(),
                    reasons,
                    preferred_spans: Vec::new(),
                    whole_file: false,
                });
            }
        }
        candidates.sort_by(|a, b| {
            b.lexical_score
                .cmp(&a.lexical_score)
                .then(a.file.path.cmp(&b.file.path))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }
}
use rusqlite::OptionalExtension;

/// Flattens the observed tree without inventing records for collapsed or inaccessible directories.
fn collect(node: &StructureNode, root: &Path, records: &mut Vec<FileRecord>, complete: &mut bool) {
    if node.kind == NodeKind::Directory && (node.collapsed || node.truncated) {
        *complete = false;
    }
    if node.kind == NodeKind::File {
        let mut identity = blake3::Hasher::new();
        identity.update(root.as_os_str().as_encoded_bytes());
        identity.update(&[0]);
        identity.update(node.path.as_os_str().as_encoded_bytes());
        records.push(FileRecord {
            id: identity.finalize().to_hex().to_string(),
            path: node.path.clone(),
            content_hash: node.source_hash.clone(),
            language: node.language.clone(),
            header: node.header.clone(),
            imports: node.imports.clone(),
            symbols: node.symbols.clone(),
            truncated: node.truncated,
        });
    }
    for child in &node.children {
        collect(child, root, records, complete);
    }
}

/// Indexes source values rather than serialization field names and unavailable messages.
fn search_text(record: &FileRecord) -> String {
    let mut text = record.path.to_string_lossy().into_owned();
    if let Detail::Available(header) = &record.header {
        text.push('\n');
        text.push_str(&header.text);
    }
    if let Detail::Available(imports) = &record.imports {
        for import in &imports.statements {
            text.push('\n');
            text.push_str(&import.text);
        }
    }
    if let Some(Detail::Available(symbols)) = &record.symbols {
        for symbol in symbols {
            symbol.walk(&mut |symbol| {
                text.push('\n');
                text.push_str(&symbol.name);
                text.push(' ');
                text.push_str(&symbol.signature);
                if let Some(doc) = &symbol.doc {
                    text.push(' ');
                    text.push_str(doc);
                }
            });
        }
    }
    text
}

/// Namespaces cached parse facts by the actual running build, including uncommitted builds.
/// Hashes the executable once per process using bounded memory; no file paths enter the ID.
fn extractor_identity() -> io::Result<&'static str> {
    static ID: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use std::io::Read;
        let compute = || -> io::Result<String> {
            let mut file = fs::File::open(std::env::current_exe()?)?;
            let mut hasher = blake3::Hasher::new();
            let mut bytes = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut bytes)?;
                if count == 0 {
                    break;
                }
                hasher.update(&bytes[..count]);
            }
            Ok(hasher.finalize().to_hex().to_string())
        };
        compute().map_err(|error| error.to_string())
    })
    .as_ref()
    .map(String::as_str)
    .map_err(|error| io::Error::other(error.clone()))
}

/// Explicit source/config/document extensions; secret-bearing dotfiles and key containers are excluded.
fn eligible_plain_text(path: &Path) -> bool {
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    if name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name,
            "credentials.json" | "credentials" | "id_rsa" | "id_ed25519"
        )
    {
        return false;
    }
    matches!(
        name,
        "Dockerfile"
            | "Makefile"
            | "Justfile"
            | "LICENSE"
            | ".gitignore"
            | "Cargo.lock"
            | "uv.lock"
            | "poetry.lock"
            | "yarn.lock"
            | "VERSION"
            | ".editorconfig"
            | ".gitattributes"
            | ".dockerignore"
            | ".gitmodules"
            | ".nvmrc"
            | ".tool-versions"
    ) || path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| {
            matches!(
                ext,
                "jinja"
                    | "jinja2"
                    | "j2"
                    | "jsonl"
                    | "hbs"
                    | "mustache"
                    | "md"
                    | "mdx"
                    | "txt"
                    | "toml"
                    | "json"
                    | "jsonc"
                    | "yaml"
                    | "yml"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "sql"
                    | "ini"
                    | "cfg"
                    | "xml"
                    | "graphql"
                    | "gql"
                    | "proto"
                    | "c"
                    | "h"
                    | "cpp"
                    | "hpp"
                    | "java"
                    | "kt"
                    | "go"
                    | "rb"
                    | "php"
                    | "swift"
                    | "vue"
                    | "svelte"
            )
        })
}

/// Reads bounded regular UTF-8 text without following a final symlink or opening devices/pipes.
fn read_plain_text(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(io::Error::other("plain text exceeds source limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit || bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Err(io::Error::other("not bounded UTF-8 text"));
    }
    Ok(bytes)
}

/// Recognizes extensionless executable scripts by their shebang, without reading arbitrary binaries.
fn extensionless_script(root: &Path, path: &Path) -> bool {
    use std::io::Read;
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    if path.extension().is_some()
        || matches!(name, "credentials" | "id_rsa" | "id_ed25519")
        || name.starts_with(".env")
    {
        return false;
    }
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(path))
    else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return false;
    }
    let mut prefix = [0u8; 2];
    file.read_exact(&mut prefix).is_ok() && prefix == *b"#!"
}

#[cfg(test)]
mod initialization_tests {
    use super::*;

    /// An interrupted legacy root insert is not ready to readers and is repaired only when empty.
    #[test]
    fn interrupted_initialization_recovers_and_writer_lease_excludes_another_writer() {
        let root = crate::scratch::ScratchDir::new("index-initialization");
        let index = ContextIndex::open(&root).unwrap();
        index
            .connection
            .execute("DELETE FROM meta WHERE key='root'", [])
            .unwrap();
        drop(index);
        assert!(ContextIndex::open_existing(&root).unwrap().is_none());
        let writer = ContextIndex::open(&root).unwrap();
        assert!(
            matches!(ContextIndex::open(&root),Err(error) if error.kind()==io::ErrorKind::WouldBlock)
        );
        let reader = ContextIndex::open_existing(&root).unwrap().unwrap();
        assert!(reader.snapshot().unwrap().is_none());
        drop(reader);
        drop(writer);
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(root.join(".agent-ide/index.sqlite"))
            .unwrap();
        assert!(ContextIndex::open_existing(&root).unwrap().is_none());
        assert!(ContextIndex::open(&root).is_ok());
    }
}
