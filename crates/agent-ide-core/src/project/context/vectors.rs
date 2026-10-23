//! Source-bound embedding documents and exact cosine lookup in project-local cached vectors.
use super::{ContextIndex, packet::read_current};
use crate::project::structure::Detail;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, path::PathBuf};

/// A bounded document keeps its current source identity outside the stable embedding input hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedDocument {
    pub id: String,
    pub file_id: String,
    pub path: PathBuf,
    pub content_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
    pub partial: bool,
}

/// One generation's candidate documents; gaps never silently become negative semantic evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentBatch {
    pub root: PathBuf,
    pub generation: u64,
    pub documents: Vec<IndexedDocument>,
    pub gaps: Vec<String>,
}

/// Limits discovery-to-model expansion independently of render and model context limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentOptions {
    pub max_documents: usize,
    pub max_input_bytes: usize,
    pub max_file_bytes: usize,
    pub max_read_bytes: usize,
    pub max_document_bytes: usize,
}
impl Default for DocumentOptions {
    /// Defaults are operator limits, not provider context claims.
    fn default() -> Self {
        Self {
            max_documents: 100_000,
            max_input_bytes: 4096,
            max_file_bytes: 2 * 1024 * 1024,
            max_read_bytes: 128 * 1024 * 1024,
            max_document_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Similarity is only a retrieval rank; it is not probability that a file requires an edit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SemanticHit {
    pub document: IndexedDocument,
    pub similarity: f32,
}

/// Maps SQLite errors without source contents or model credentials.
fn db(error: rusqlite::Error) -> io::Error {
    io::Error::other(error.to_string())
}

/// Normalizes validated dimensions using f64 accumulation to avoid f32 overflow.
fn unit(vector: &[f32], dimensions: usize) -> io::Result<Vec<f32>> {
    if dimensions == 0
        || dimensions > 8192
        || vector.len() != dimensions
        || vector.iter().any(|n| !n.is_finite())
    {
        return Err(io::Error::other("invalid vector dimensions or values"));
    }
    let norm = vector
        .iter()
        .map(|n| (*n as f64) * (*n as f64))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return Err(io::Error::other("zero vector"));
    }
    Ok(vector.iter().map(|n| (*n as f64 / norm) as f32).collect())
}

impl ContextIndex {
    /// Builds file/source documents using current bytes, splitting large sources on UTF-8 boundaries.
    /// Every part repeats its path and is explicitly partial; no model call occurs here.
    pub fn documents(&self, options: &DocumentOptions) -> io::Result<DocumentBatch> {
        self.documents_cancellable(options, &|| false)
    }

    /// Stops fragment preparation between bounded files/chunks without publishing partial file caches.
    pub fn documents_cancellable(
        &self,
        options: &DocumentOptions,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<DocumentBatch> {
        self.require_writer()?;
        if !(1..=100_000).contains(&options.max_documents)
            || !(256..=32768).contains(&options.max_input_bytes)
            || options.max_document_bytes == 0
            || options.max_document_bytes > 128 * 1024 * 1024
            || options.max_file_bytes == 0
            || options.max_file_bytes > 16 * 1024 * 1024
            || options.max_read_bytes == 0
            || options.max_read_bytes > 128 * 1024 * 1024
        {
            return Err(io::Error::other("invalid document limits"));
        }
        let snapshot = self
            .snapshot()?
            .ok_or_else(|| io::Error::other("index has not been built"))?;
        let mut batch = DocumentBatch {
            root: self.root.clone(),
            generation: snapshot.generation,
            documents: Vec::new(),
            gaps: Vec::new(),
        };
        if !snapshot.discovery_complete {
            batch.gaps.push("index discovery is incomplete".into());
        }
        self.document_tables()?;
        let recipe = format!(
            "units-v2:{}",
            serde_json::to_string(options).map_err(io::Error::other)?
        );
        let mut read = 0;
        let mut document_bytes = 0;
        'files: for file in self.records()? {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "document preparation cancelled",
                ));
            }
            if batch.documents.len() >= options.max_documents {
                batch.gaps.push("document count limit reached".into());
                break;
            }
            if file.content_hash.is_none() {
                batch
                    .gaps
                    .push(format!("{}: path-only source", file.path.display()));
                continue;
            }
            let cached: Option<String> = self
                .connection
                .query_row(
                    "SELECT data FROM document_cache WHERE path=?1 AND hash=?2 AND recipe=?3",
                    params![file.path.to_string_lossy(), file.content_hash, recipe],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db)?;
            if let Some(cached) = cached {
                let docs: Vec<IndexedDocument> =
                    serde_json::from_str(&cached).map_err(io::Error::other)?;
                let bytes: usize = docs.iter().map(|d| d.text.len()).sum();
                if batch.documents.len() + docs.len() > options.max_documents
                    || bytes > options.max_document_bytes.saturating_sub(document_bytes)
                {
                    batch.gaps.push("cached document limit reached".into());
                    break;
                }
                document_bytes += bytes;
                batch.documents.extend(docs);
                continue;
            }
            let file_start = batch.documents.len();
            let remaining = options
                .max_read_bytes
                .saturating_sub(read)
                .min(options.max_file_bytes);
            let source = match read_current(&self.root, &file, remaining, &mut read) {
                Ok(source) => source,
                Err(error) => {
                    batch.gaps.push(format!("{}: {error}", file.path.display()));
                    continue;
                }
            };
            // Complete semantic units keep edits to one method from invalidating sibling methods.
            // Uncovered bytes (imports, container headers, closing braces) are indexed as well.
            let mut spans = BTreeSet::new();
            let units = super::packet::declaration_units(&file, &source);
            let mut end = 0;
            for unit in &units {
                if unit.start < end {
                    continue;
                }
                if unit.start > end {
                    spans.insert((end, unit.start));
                }
                spans.insert((unit.start, unit.end));
                end = unit.end;
            }
            if end < source.len() {
                spans.insert((end, source.len()));
            }
            let header = match &file.header {
                Detail::Available(header) => header.text.as_str(),
                _ => "",
            };
            let card = format!("path: {}\nheader: {}", file.path.display(), header);
            if card.len() <= options.max_input_bytes {
                if card.len() > options.max_document_bytes.saturating_sub(document_bytes) {
                    batch.gaps.push("document byte limit reached".into());
                    return Ok(batch);
                }
                document_bytes += card.len();
                batch.documents.push(IndexedDocument {
                    id: blake3::hash(card.as_bytes()).to_hex().to_string(),
                    file_id: file.id.clone(),
                    path: file.path.clone(),
                    content_hash: file.content_hash.clone().unwrap(),
                    start_byte: 0,
                    end_byte: 0,
                    text: card,
                    partial: true,
                });
            }
            if spans.is_empty() && !source.is_empty() {
                spans.insert((0, source.len()));
            }
            for (start, end) in spans {
                let prefix = format!("path: {}\nsource:\n", file.path.display());
                let Some(cap) = options
                    .max_input_bytes
                    .checked_sub(prefix.len())
                    .filter(|cap| *cap >= 4)
                else {
                    batch
                        .gaps
                        .push(format!("{}: document prefix limit", file.path.display()));
                    batch.documents.truncate(file_start);
                    document_bytes = batch.documents.iter().map(|d| d.text.len()).sum();
                    continue 'files;
                };
                let mut cursor = start;
                while cursor < end {
                    if cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "document preparation cancelled",
                        ));
                    }
                    if batch.documents.len() >= options.max_documents {
                        batch.documents.truncate(file_start);
                        batch.gaps.push(format!(
                            "{}: document count limit reached",
                            file.path.display()
                        ));
                        return Ok(batch);
                    }
                    let mut finish = cursor.saturating_add(cap).min(end);
                    while !source.is_char_boundary(finish) {
                        finish -= 1;
                    }
                    if finish < end
                        && let Some(newline) = source[cursor..finish].rfind('\n')
                    {
                        finish = cursor + newline + 1;
                    }
                    let text = format!("{prefix}{}", &source[cursor..finish]);
                    if text.len() > options.max_document_bytes.saturating_sub(document_bytes) {
                        batch.documents.truncate(file_start);
                        batch.gaps.push(format!(
                            "{}: document byte limit reached",
                            file.path.display()
                        ));
                        return Ok(batch);
                    }
                    document_bytes += text.len();
                    batch.documents.push(IndexedDocument {
                        id: blake3::hash(text.as_bytes()).to_hex().to_string(),
                        file_id: file.id.clone(),
                        path: file.path.clone(),
                        content_hash: file.content_hash.clone().unwrap(),
                        start_byte: cursor,
                        end_byte: finish,
                        text,
                        partial: cursor != start || finish != end,
                    });
                    cursor = finish;
                }
            }
            self.connection
                .execute(
                    "INSERT OR REPLACE INTO document_cache VALUES (?1,?2,?3,?4)",
                    params![
                        file.path.to_string_lossy(),
                        file.content_hash,
                        recipe,
                        serde_json::to_string(&batch.documents[file_start..])
                            .map_err(io::Error::other)?
                    ],
                )
                .map_err(db)?;
        }
        Ok(batch)
    }

    /// Creates rebuildable document caches only in the background writer, never in query paths.
    fn document_tables(&self) -> io::Result<()> {
        self.connection.execute_batch("CREATE TABLE IF NOT EXISTS document_cache(path TEXT PRIMARY KEY,hash TEXT NOT NULL,recipe TEXT NOT NULL,data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS ready_documents(space TEXT NOT NULL,path TEXT NOT NULL,hash TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(space,path)); CREATE TABLE IF NOT EXISTS index_reports(space TEXT PRIMARY KEY,data TEXT NOT NULL);").map_err(db)
    }

    /// Atomically publishes one complete file after all its vectors are durable; callers validate freshness.
    #[cfg(feature = "jev-pilot")]
    pub(super) fn publish_file(
        &self,
        space: &str,
        documents: &[IndexedDocument],
    ) -> io::Result<()> {
        let Some(first) = documents.first() else {
            return Ok(());
        };
        if documents
            .iter()
            .any(|d| d.path != first.path || d.content_hash != first.content_hash)
        {
            return Err(io::Error::other("mixed published file versions"));
        }
        self.document_tables()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO ready_documents VALUES (?1,?2,?3,?4)",
                params![
                    space,
                    first.path.to_string_lossy(),
                    first.content_hash,
                    serde_json::to_string(documents).map_err(io::Error::other)?
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Loads only complete published files matching the pinned source generation, without source reads.
    pub fn published_documents(&self, space: &str) -> io::Result<DocumentBatch> {
        let snapshot = self
            .snapshot()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "context_index_not_ready"))?;
        let exists:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ready_documents')",[],|r|r.get(0)).map_err(db)?;
        if !exists {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "context_index_not_ready",
            ));
        }
        let mut batch = DocumentBatch {
            root: self.root.clone(),
            generation: snapshot.generation,
            documents: Vec::new(),
            gaps: Vec::new(),
        };
        for file in self.records()? {
            if file.content_hash.is_none() {
                continue;
            }
            let data: Option<String> = self
                .connection
                .query_row(
                    "SELECT data FROM ready_documents WHERE space=?1 AND path=?2 AND hash=?3",
                    params![space, file.path.to_string_lossy(), file.content_hash],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db)?;
            if let Some(data) = data {
                batch.documents.extend(
                    serde_json::from_str::<Vec<IndexedDocument>>(&data)
                        .map_err(io::Error::other)?,
                );
            } else {
                batch
                    .gaps
                    .push(format!("{}: index update pending", file.path.display()));
            }
        }
        if !snapshot.discovery_complete {
            batch.gaps.push("index discovery is incomplete".into());
        }
        if batch.documents.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "context_index_not_ready",
            ));
        }
        Ok(batch)
    }

    /// Persists the latest operator-visible background outcome without touching source data.
    #[cfg(feature = "jev-pilot")]
    pub(super) fn save_index_report(
        &self,
        space: &str,
        report: &serde_json::Value,
    ) -> io::Result<()> {
        self.document_tables()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO index_reports VALUES (?1,?2)",
                params![space, report.to_string()],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Reads the last background report, including on read-only query connections.
    pub fn index_report(&self, space: &str) -> io::Result<Option<serde_json::Value>> {
        let exists: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='index_reports')",
                [],
                |r| r.get(0),
            )
            .map_err(db)?;
        if !exists {
            return Ok(None);
        }
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT data FROM index_reports WHERE space=?1",
                [space],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?;
        raw.map(|s| serde_json::from_str(&s).map_err(io::Error::other))
            .transpose()
    }

    /// Creates only a rebuildable optional cache table inside the verified index database.
    fn vector_table(&self) -> io::Result<()> {
        self.connection.execute_batch("CREATE TABLE IF NOT EXISTS vectors (space TEXT NOT NULL, document TEXT NOT NULL, dimensions INTEGER NOT NULL, value BLOB NOT NULL, PRIMARY KEY(space,document))").map_err(db)
    }

    /// Stores one validated normalized vector. Namespaces prevent mixing model spaces.
    pub fn store_vector(&self, space: &str, document: &str, vector: &[f32]) -> io::Result<()> {
        if space.is_empty() || space.len() > 128 || document.is_empty() || document.len() > 128 {
            return Err(io::Error::other("invalid vector identity"));
        }
        let vector = unit(vector, vector.len())?;
        self.vector_table()?;
        let bytes: Vec<u8> = vector.iter().flat_map(|n| n.to_le_bytes()).collect();
        self.connection
            .execute(
                "INSERT OR REPLACE INTO vectors VALUES (?1,?2,?3,?4)",
                params![space, document, vector.len(), bytes],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Returns only a validated vector from the exact requested model space and dimension.
    pub fn vector(
        &self,
        space: &str,
        document: &str,
        dimensions: usize,
    ) -> io::Result<Option<Vec<f32>>> {
        let exists: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='vectors')",
                [],
                |r| r.get(0),
            )
            .map_err(db)?;
        if !exists {
            return Ok(None);
        }
        let row: Option<(usize, Vec<u8>)> = self
            .connection
            .query_row(
                "SELECT dimensions,value FROM vectors WHERE space=?1 AND document=?2",
                params![space, document],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        row.map(|(stored, bytes)| {
            if stored != dimensions || bytes.len() != dimensions.saturating_mul(4) {
                return Err(io::Error::other("cached vector dimension mismatch"));
            }
            let values: Vec<_> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect();
            unit(&values, dimensions)
        })
        .transpose()
    }

    /// Performs exact cosine lookup over current documents; missing embeddings remain absent.
    pub fn semantic_search(
        &self,
        batch: &DocumentBatch,
        space: &str,
        query: &[f32],
        limit: usize,
    ) -> io::Result<Vec<SemanticHit>> {
        if limit == 0 || limit > 256 {
            return Err(io::Error::other("semantic limit must be 1..256"));
        }
        let snapshot = self
            .snapshot()?
            .ok_or_else(|| io::Error::other("index has not been built"))?;
        if batch.root != self.root || batch.generation != snapshot.generation {
            return Err(io::Error::other("document generation is stale"));
        }
        let query = unit(query, query.len())?;
        let mut hits = Vec::new();
        for document in &batch.documents {
            if let Some(vector) = self.vector(space, &document.id, query.len())? {
                let similarity = query.iter().zip(vector).map(|(a, b)| a * b).sum::<f32>();
                hits.push(SemanticHit {
                    document: document.clone(),
                    similarity,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then(a.document.id.cmp(&b.document.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

impl ContextIndex {
    /// Merges lexical and semantic ranks without comparing their incompatible raw scores.
    /// Several symbol documents of a file share one file candidate and retain source spans.
    pub fn hybrid_search(
        &self,
        task: &str,
        batch: &DocumentBatch,
        space: &str,
        query: &[f32],
        limit: usize,
    ) -> io::Result<Vec<super::Candidate>> {
        let lexical = self.search(task, limit)?;
        let semantic = self.semantic_search(batch, space, query, limit)?;
        let records: std::collections::BTreeMap<_, _> = self
            .records()?
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        let mut merged = std::collections::BTreeMap::new();
        for (rank, candidate) in lexical.into_iter().enumerate() {
            merged.insert(
                candidate.file.id.clone(),
                (1.0 / (61 + rank) as f64, candidate),
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for (rank, hit) in semantic.into_iter().enumerate() {
            let Some(file) = records.get(&hit.document.file_id) else {
                continue;
            };
            if file.content_hash.as_deref() != Some(hit.document.content_hash.as_str()) {
                continue;
            }
            let entry = merged.entry(file.id.clone()).or_insert_with(|| {
                (
                    0.0,
                    super::Candidate {
                        file: file.clone(),
                        lexical_score: 0,
                        reasons: Vec::new(),
                        preferred_spans: Vec::new(),
                        whole_file: false,
                    },
                )
            });
            if seen.insert(file.id.clone()) {
                entry.0 += 1.0 / (61 + rank) as f64;
            }
            entry
                .1
                .reasons
                .push(format!("semantic document {}", hit.document.id));
            if hit.document.end_byte > hit.document.start_byte {
                entry
                    .1
                    .preferred_spans
                    .push((hit.document.start_byte, hit.document.end_byte));
            }
        }
        let mut ranked: Vec<_> = merged.into_values().collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.file.path.cmp(&b.1.file.path)));
        ranked.truncate(limit);
        Ok(ranked.into_iter().map(|(_, candidate)| candidate).collect())
    }
}
