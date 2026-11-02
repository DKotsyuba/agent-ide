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

    /// Reuses validated vectors from an operator-selected index for current destination payloads.
    /// Call refresh first. Only exact content IDs in the requested model space are copied;
    /// root identity, published generations, read receipts and request journals stay local.
    /// A later normal indexing pass revalidates sources and publishes ready documents.
    /// Returns the number of newly cached vectors; partial copies are safe to retry offline.
    pub fn reuse_vectors(
        &self,
        source: &ContextIndex,
        space: &str,
        dimensions: usize,
        options: &DocumentOptions,
    ) -> io::Result<usize> {
        if space.is_empty() || space.len() > 128 || !(1..=8192).contains(&dimensions) {
            return Err(io::Error::other("invalid vector reuse model space"));
        }
        let batch = self.documents(options)?;
        let mut copied = 0;
        for document in batch.documents {
            if self.vector(space, &document.id, dimensions)?.is_some() {
                continue;
            }
            if let Some(vector) = source.vector(space, &document.id, dimensions)? {
                self.store_vector(space, &document.id, &vector)?;
                copied += 1;
            }
        }
        Ok(copied)
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
        let (mut hits, _) = self.rank_documents(batch, space, query)?;
        hits.truncate(limit);
        Ok(hits)
    }

    /// Every document of `batch` with a vector in `space`, ordered as [`Self::semantic_search`]
    /// orders them (similarity desc, document id asc), plus the ids of documents without one.
    fn rank_documents(
        &self,
        batch: &DocumentBatch,
        space: &str,
        query: &[f32],
    ) -> io::Result<(Vec<SemanticHit>, Vec<String>)> {
        let snapshot = self
            .snapshot()?
            .ok_or_else(|| io::Error::other("index has not been built"))?;
        if batch.root != self.root || batch.generation != snapshot.generation {
            return Err(io::Error::other("document generation is stale"));
        }
        let query = unit(query, query.len())?;
        let mut hits = Vec::new();
        let mut missing = Vec::new();
        for document in &batch.documents {
            if let Some(vector) = self.vector(space, &document.id, query.len())? {
                let similarity = query.iter().zip(vector).map(|(a, b)| a * b).sum::<f32>();
                hits.push(SemanticHit {
                    document: document.clone(),
                    similarity,
                });
            } else {
                missing.push(document.id.clone());
            }
        }
        hits.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then(a.document.id.cmp(&b.document.id))
        });
        Ok((hits, missing))
    }
}

/// Reciprocal-rank constant of the hybrid fusion: rank `r` (0-based) contributes `1 / (61 + r)`.
const RRF_OFFSET: usize = 61;

/// One fused file of a hybrid search.
struct Fused {
    /// Sum of its reciprocal-rank contributions.
    score: f64,
    candidate: super::Candidate,
    /// Its rank in the lexical shortlist (diagnostic builds only).
    #[cfg(any(feature = "jev-pilot", test))]
    lexical_rank: Option<usize>,
    /// Rank of its first eligible document in the semantic shortlist.
    first_semantic_rank: Option<usize>,
}

#[cfg(any(feature = "jev-pilot", test))]
/// Why a semantic document could or could not stand for its file in the fusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DocumentEligibility {
    /// Its file is a current record with the same content hash.
    Eligible,
    /// Its file id is not a current record.
    UnknownFile,
    /// Its file's current content hash differs from the document's.
    StaleHash,
}

#[cfg(any(feature = "jev-pilot", test))]
/// Where one file ended up in one hybrid search.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HybridOutcome {
    /// In the lexical top-`limit` or represented by an eligible document of the semantic
    /// top-`limit`: a member of the admitted union, whatever its fused rank.
    Selected,
    /// Scored lexically or semantically, but in neither top-`limit`.
    OutsideBothLimits,
    /// No matched term and no eligible scored document.
    Unscored,
}

#[cfg(any(feature = "jev-pilot", test))]
/// The fixed parameters one hybrid search ran with.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct HybridTraceConfig {
    /// Lexical file and semantic document shortlist sizes. Preparation admits their whole
    /// fused union (at most twice this many files), not only the top `limit` fused files.
    pub limit: usize,
    /// How fused files are admitted: `lexical_union_semantic`, every file of either shortlist.
    pub admission: &'static str,
    /// Reciprocal-rank constant.
    pub rrf_offset: usize,
}

#[cfg(any(feature = "jev-pilot", test))]
/// One file of the complete lexical ranking (matched terms desc, path asc).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct LexicalRank {
    pub rank: usize,
    pub file_id: String,
    pub path: PathBuf,
    pub matches: usize,
}

#[cfg(any(feature = "jev-pilot", test))]
/// One scored document of the complete semantic ranking (similarity desc, id asc).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DocumentRank {
    pub rank: usize,
    pub document_id: String,
    pub file_id: String,
    pub path: PathBuf,
    pub content_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub similarity: f32,
    /// Within the semantic top-`limit` that the fusion consumes.
    pub within_limit: bool,
    pub eligibility: DocumentEligibility,
}

#[cfg(any(feature = "jev-pilot", test))]
/// One fused file of the admitted union, in fused order.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FusionRank {
    pub rank: usize,
    pub file_id: String,
    pub path: PathBuf,
    pub score: f64,
    pub lexical_rank: Option<usize>,
    /// Rank of the file's first eligible document within the semantic top-`limit`.
    pub first_semantic_rank: Option<usize>,
    /// Admitted to relevance selection; always true, since the union admits every fused file.
    pub selected: bool,
}

#[cfg(any(feature = "jev-pilot", test))]
/// One current record's standing across every stage.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct HybridFileTrace {
    pub file_id: String,
    pub path: PathBuf,
    pub lexical_rank: Option<usize>,
    pub lexical_matches: usize,
    /// Best rank and similarity among the file's eligible scored documents.
    pub semantic_best_rank: Option<usize>,
    pub semantic_best_similarity: Option<f32>,
    pub semantic_documents: usize,
    /// Scored documents of this file that were not eligible; recorded, never an outcome.
    pub ineligible_documents: usize,
    pub fusion_rank: Option<usize>,
    pub fusion_score: f64,
    pub outcome: HybridOutcome,
}

#[cfg(any(feature = "jev-pilot", test))]
/// Every stage of one hybrid search, for diagnostics only: complete rankings before each
/// truncation, thresholds and per-file outcomes. Paths, ids and scores only; never source text
/// or vectors. It never feeds selection.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct HybridTrace {
    pub config: HybridTraceConfig,
    pub lexical_terms: usize,
    /// Matched terms of the `limit`-th lexical file, when the ranking is that long.
    pub lexical_cutoff_matches: Option<usize>,
    /// Similarity of the `limit`-th scored document, when the ranking is that long.
    pub semantic_cutoff_similarity: Option<f32>,
    pub documents_in_batch: usize,
    pub documents_scored: usize,
    /// Batch documents without a vector in the space.
    pub missing_vectors: Vec<String>,
    pub lexical: Vec<LexicalRank>,
    pub semantic: Vec<DocumentRank>,
    pub fusion: Vec<FusionRank>,
    /// Every current record, in path order.
    pub files: Vec<HybridFileTrace>,
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
        let mut fused = self.fuse(lexical, &semantic)?;
        fused.truncate(limit);
        Ok(fused.into_iter().map(|file| file.candidate).collect())
    }

    #[cfg(any(feature = "jev-pilot", test))]
    /// The UNION of the lexical top-`limit` files and the semantic top-`limit` documents' files,
    /// in [`Self::hybrid_search`]'s fused order, plus a [`HybridTrace`] of every stage.
    ///
    /// Unlike `hybrid_search` (at most `limit` files) no file of either shortlist is dropped at
    /// fusion: relevance selection decides, so a file one ranking places high cannot fall out
    /// only because the other ranking does not see it. The complete rankings are computed
    /// beside the selection for the trace only.
    pub(crate) fn hybrid_union_search_traced(
        &self,
        task: &str,
        batch: &DocumentBatch,
        space: &str,
        query: &[f32],
        limit: usize,
    ) -> io::Result<(Vec<super::Candidate>, HybridTrace)> {
        let lexical = self.search(task, limit)?;
        let (all_hits, missing_vectors) = self.rank_documents(batch, space, query)?;
        let semantic: Vec<_> = all_hits.iter().take(limit).cloned().collect();
        let fused = self.fuse(lexical, &semantic)?;
        let records = self.records()?;
        let current: std::collections::BTreeMap<_, _> = records
            .iter()
            .map(|record| (record.id.as_str(), record))
            .collect();
        // The complete lexical ranking, ordered exactly as `search` orders its shortlist.
        let terms = super::storage::lexical_terms(task);
        let mut lexical_all: Vec<_> = records
            .iter()
            .map(|record| {
                (
                    super::storage::lexical_reasons(record, &terms).len(),
                    record,
                )
            })
            .filter(|(matches, _)| *matches > 0)
            .collect();
        lexical_all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
        let lexical_ranks: Vec<_> = lexical_all
            .iter()
            .enumerate()
            .map(|(rank, (matches, record))| LexicalRank {
                rank,
                file_id: record.id.clone(),
                path: record.path.clone(),
                matches: *matches,
            })
            .collect();
        let semantic_ranks: Vec<_> = all_hits
            .iter()
            .enumerate()
            .map(|(rank, hit)| DocumentRank {
                rank,
                document_id: hit.document.id.clone(),
                file_id: hit.document.file_id.clone(),
                path: hit.document.path.clone(),
                content_hash: hit.document.content_hash.clone(),
                start_byte: hit.document.start_byte,
                end_byte: hit.document.end_byte,
                similarity: hit.similarity,
                within_limit: rank < limit,
                eligibility: match current.get(hit.document.file_id.as_str()) {
                    None => DocumentEligibility::UnknownFile,
                    Some(file)
                        if file.content_hash.as_deref()
                            != Some(hit.document.content_hash.as_str()) =>
                    {
                        DocumentEligibility::StaleHash
                    }
                    Some(_) => DocumentEligibility::Eligible,
                },
            })
            .collect();
        let fusion_ranks: Vec<_> = fused
            .iter()
            .enumerate()
            .map(|(rank, file)| FusionRank {
                rank,
                file_id: file.candidate.file.id.clone(),
                path: file.candidate.file.path.clone(),
                score: file.score,
                lexical_rank: file.lexical_rank,
                first_semantic_rank: file.first_semantic_rank,
                selected: true,
            })
            .collect();
        let lexical_by_file: std::collections::BTreeMap<_, _> = lexical_ranks
            .iter()
            .map(|rank| (rank.file_id.as_str(), rank))
            .collect();
        let mut documents_by_file: std::collections::BTreeMap<_, Vec<_>> =
            std::collections::BTreeMap::new();
        for rank in &semantic_ranks {
            documents_by_file
                .entry(rank.file_id.as_str())
                .or_default()
                .push(rank);
        }
        let fused_by_file: std::collections::BTreeMap<_, _> = fusion_ranks
            .iter()
            .map(|rank| (rank.file_id.as_str(), rank))
            .collect();
        let files = records
            .iter()
            .map(|record| {
                let lexical = lexical_by_file.get(record.id.as_str()).copied();
                let documents = documents_by_file
                    .get(record.id.as_str())
                    .map_or(&[][..], Vec::as_slice);
                let eligible: Vec<_> = documents
                    .iter()
                    .filter(|rank| rank.eligibility == DocumentEligibility::Eligible)
                    .collect();
                let fused = fused_by_file.get(record.id.as_str()).copied();
                let outcome = match fused {
                    Some(_) => HybridOutcome::Selected,
                    None if lexical.is_some() || !eligible.is_empty() => {
                        HybridOutcome::OutsideBothLimits
                    }
                    None => HybridOutcome::Unscored,
                };
                HybridFileTrace {
                    file_id: record.id.clone(),
                    path: record.path.clone(),
                    lexical_rank: lexical.map(|rank| rank.rank),
                    lexical_matches: lexical.map_or(0, |rank| rank.matches),
                    semantic_best_rank: eligible.first().map(|rank| rank.rank),
                    semantic_best_similarity: eligible.first().map(|rank| rank.similarity),
                    semantic_documents: eligible.len(),
                    ineligible_documents: documents.len() - eligible.len(),
                    fusion_rank: fused.map(|rank| rank.rank),
                    fusion_score: fused.map_or(0.0, |rank| rank.score),
                    outcome,
                }
            })
            .collect();
        let trace = HybridTrace {
            config: HybridTraceConfig {
                limit,
                admission: "lexical_union_semantic",
                rrf_offset: RRF_OFFSET,
            },
            lexical_terms: terms.len(),
            lexical_cutoff_matches: lexical_ranks.get(limit - 1).map(|rank| rank.matches),
            semantic_cutoff_similarity: semantic_ranks.get(limit - 1).map(|rank| rank.similarity),
            documents_in_batch: batch.documents.len(),
            documents_scored: semantic_ranks.len(),
            missing_vectors,
            lexical: lexical_ranks,
            semantic: semantic_ranks,
            fusion: fusion_ranks,
            files,
        };
        let selected = fused.into_iter().map(|file| file.candidate).collect();
        Ok((selected, trace))
    }

    /// The hybrid fusion shared by [`Self::hybrid_search`] and [`Self::hybrid_union_search_traced`]:
    /// lexical rank and the first eligible document rank of each file add `1 / (61 + rank)`;
    /// files are ordered by score desc, path asc, untruncated. An unknown or stale document is
    /// skipped but keeps its rank slot.
    fn fuse(
        &self,
        lexical: Vec<super::Candidate>,
        semantic: &[SemanticHit],
    ) -> io::Result<Vec<Fused>> {
        let records: std::collections::BTreeMap<_, _> = self
            .records()?
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        let mut merged = std::collections::BTreeMap::new();
        for (rank, candidate) in lexical.into_iter().enumerate() {
            merged.insert(
                candidate.file.id.clone(),
                Fused {
                    score: 1.0 / (RRF_OFFSET + rank) as f64,
                    candidate,
                    #[cfg(any(feature = "jev-pilot", test))]
                    lexical_rank: Some(rank),
                    first_semantic_rank: None,
                },
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for (rank, hit) in semantic.iter().enumerate() {
            let Some(file) = records.get(&hit.document.file_id) else {
                continue;
            };
            if file.content_hash.as_deref() != Some(hit.document.content_hash.as_str()) {
                continue;
            }
            let entry = merged.entry(file.id.clone()).or_insert_with(|| Fused {
                score: 0.0,
                candidate: super::Candidate {
                    file: file.clone(),
                    lexical_score: 0,
                    reasons: Vec::new(),
                    preferred_spans: Vec::new(),
                    whole_file: false,
                },
                #[cfg(any(feature = "jev-pilot", test))]
                lexical_rank: None,
                first_semantic_rank: None,
            });
            if seen.insert(file.id.clone()) {
                entry.score += 1.0 / (RRF_OFFSET + rank) as f64;
                entry.first_semantic_rank = Some(rank);
            }
            entry
                .candidate
                .reasons
                .push(format!("semantic document {}", hit.document.id));
            if hit.document.end_byte > hit.document.start_byte {
                entry
                    .candidate
                    .preferred_spans
                    .push((hit.document.start_byte, hit.document.end_byte));
            }
        }
        let mut ranked: Vec<_> = merged.into_values().collect();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.candidate.file.path.cmp(&b.candidate.file.path))
        });
        Ok(ranked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture whose lexical and semantic rankings are chosen by hand, limit 3.
    ///
    /// Lexical ("needle" in the path, one match each): needle-a, needle-b, needle-c, needle-d.
    /// Semantic (query [1, 0]) over each file's source document: sem-x 1.0, a stale copy of
    /// needle-a's document 0.95, needle-a 0.9, sem-y 0.8, the rest 0.0. Path cards and
    /// novec.alpha's source document have no vector.
    #[test]
    fn traced_union_search_keeps_both_shortlists_and_explains_every_file() {
        crate::lang::testing::install();
        let root = crate::scratch::ScratchDir::new("context-hybrid-trace");
        for name in [
            "needle-a.alpha",
            "needle-b.alpha",
            "needle-c.alpha",
            "needle-d.alpha",
            "sem-x.alpha",
            "sem-y.alpha",
            "plain.alpha",
            "novec.alpha",
        ] {
            std::fs::write(root.join(name), format!("{name}\n")).unwrap();
        }
        let mut index = ContextIndex::open(&root).unwrap();
        index
            .refresh(&crate::project::structure::StructureOptions::default())
            .unwrap();
        let mut batch = index.documents(&DocumentOptions::default()).unwrap();
        let space = "test-space";
        let vector = |path: &str| -> Option<[f32; 2]> {
            match path {
                "sem-x.alpha" => Some([1.0, 0.0]),
                "needle-a.alpha" => Some([0.9, 0.435_889_9]),
                "sem-y.alpha" => Some([0.8, 0.6]),
                "needle-b.alpha" | "needle-c.alpha" | "needle-d.alpha" | "plain.alpha" => {
                    Some([0.0, 1.0])
                }
                // novec.alpha and any file the fixture did not write (e.g. an index ignore
                // file) stay without vectors.
                _ => None,
            }
        };
        let source = |document: &&IndexedDocument| document.end_byte > document.start_byte;
        let mut stored = BTreeSet::new();
        for document in batch.documents.iter().filter(source) {
            if let Some(value) = vector(document.path.to_str().unwrap()) {
                index.store_vector(space, &document.id, &value).unwrap();
                stored.insert(document.id.clone());
            }
        }
        let expected_missing: BTreeSet<_> = batch
            .documents
            .iter()
            .filter(|document| !stored.contains(&document.id))
            .map(|document| document.id.clone())
            .collect();
        let novec_source = batch
            .documents
            .iter()
            .filter(source)
            .find(|document| document.path == std::path::Path::new("novec.alpha"))
            .unwrap()
            .id
            .clone();
        let mut stale = batch
            .documents
            .iter()
            .filter(source)
            .find(|document| document.path == std::path::Path::new("needle-a.alpha"))
            .unwrap()
            .clone();
        stale.id = "stale-document".into();
        stale.content_hash = "stale".into();
        index
            .store_vector(space, &stale.id, &[0.95, 0.312_249_9])
            .unwrap();
        stored.insert(stale.id.clone());
        batch.documents.push(stale);
        let query = [1.0, 0.0];
        let paths = |candidates: &[super::super::Candidate]| -> Vec<String> {
            candidates
                .iter()
                .map(|candidate| candidate.file.path.display().to_string())
                .collect()
        };
        let plain = index
            .hybrid_search("needle", &batch, space, &query, 3)
            .unwrap();
        assert_eq!(
            paths(&plain),
            ["needle-a.alpha", "sem-x.alpha", "needle-b.alpha"]
        );
        let (traced, trace) = index
            .hybrid_union_search_traced("needle", &batch, space, &query, 3)
            .unwrap();
        // The capped selection first, in the same order and with the same evidence, then the
        // lexical shortlist's needle-c that the capped fusion cut.
        assert_eq!(
            paths(&traced),
            [
                "needle-a.alpha",
                "sem-x.alpha",
                "needle-b.alpha",
                "needle-c.alpha"
            ]
        );
        assert_eq!(
            traced[..3]
                .iter()
                .map(|candidate| (&candidate.reasons, &candidate.preferred_spans))
                .collect::<Vec<_>>(),
            plain
                .iter()
                .map(|candidate| (&candidate.reasons, &candidate.preferred_spans))
                .collect::<Vec<_>>()
        );
        assert_eq!(trace.config.admission, "lexical_union_semantic");
        let outcome = |path: &str| {
            trace
                .files
                .iter()
                .find(|file| file.path == std::path::Path::new(path))
                .unwrap()
        };
        assert_eq!(outcome("needle-a.alpha").outcome, HybridOutcome::Selected);
        assert_eq!(outcome("needle-a.alpha").ineligible_documents, 1);
        assert_eq!(outcome("sem-x.alpha").outcome, HybridOutcome::Selected);
        assert_eq!(outcome("needle-b.alpha").outcome, HybridOutcome::Selected);
        assert_eq!(outcome("needle-c.alpha").outcome, HybridOutcome::Selected);
        assert_eq!(outcome("needle-c.alpha").fusion_rank, Some(3));
        for path in ["needle-d.alpha", "sem-y.alpha", "plain.alpha"] {
            assert_eq!(
                outcome(path).outcome,
                HybridOutcome::OutsideBothLimits,
                "{path}"
            );
        }
        assert_eq!(outcome("sem-y.alpha").semantic_best_rank, Some(3));
        assert_eq!(outcome("needle-d.alpha").lexical_rank, Some(3));
        assert_eq!(outcome("novec.alpha").outcome, HybridOutcome::Unscored);
        // Exactly the documents this fixture gave no vector: every path card, novec.alpha's
        // source document and any document of a file the fixture did not write.
        assert_eq!(
            trace
                .missing_vectors
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_missing,
            "batch paths: {:?}",
            batch
                .documents
                .iter()
                .map(|document| (&document.path, document.start_byte, document.end_byte))
                .collect::<Vec<_>>()
        );
        assert!(trace.missing_vectors.contains(&novec_source));
        assert_eq!(trace.lexical.len(), 4);
        assert_eq!(trace.lexical_cutoff_matches, Some(1));
        assert_eq!(trace.documents_scored, stored.len());
        // Seven written files' source documents plus the stale copy.
        assert_eq!(trace.documents_scored, 8);
        assert_eq!(
            trace
                .semantic
                .iter()
                .take(3)
                .map(|rank| (rank.path.display().to_string(), rank.eligibility))
                .collect::<Vec<_>>(),
            [
                ("sem-x.alpha".to_owned(), DocumentEligibility::Eligible),
                ("needle-a.alpha".to_owned(), DocumentEligibility::StaleHash),
                ("needle-a.alpha".to_owned(), DocumentEligibility::Eligible),
            ]
        );
        assert_eq!(
            trace
                .fusion
                .iter()
                .map(|rank| rank.selected)
                .collect::<Vec<_>>(),
            [true, true, true, true]
        );
        let json = serde_json::to_string(&trace).unwrap();
        assert!(!json.contains("\"outcome\":\"cut_at_fusion\""));
        assert!(!json.contains("needle-a.alpha\\n"), "no source text");
    }
}
