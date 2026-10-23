//! Task-oriented source context. Operator callers authorize the root before initializing it.
//! Reading an index is not delivery of its sources to an agent and grants no edit authority.

mod packet;
mod storage;
mod vectors;
pub use packet::{ContextBundle, DeclarationFact, FileFacts, PacketOptions, SourceWindow};
pub use storage::ContextIndex;
pub use vectors::{DocumentBatch, DocumentOptions, IndexedDocument, SemanticHit};

use super::structure::{Detail, ImportSummary, SourceExcerpt};
use crate::lang::Symbol;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Current source-backed facts of one file; unsupported files remain path-searchable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub path: PathBuf,
    pub content_hash: Option<String>,
    pub language: Option<String>,
    pub header: Detail<SourceExcerpt>,
    pub imports: Detail<ImportSummary>,
    pub symbols: Option<Detail<Vec<Symbol>>>,
    pub truncated: bool,
}

/// A published index generation and honest scope/parse limitations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexSnapshot {
    pub generation: u64,
    pub root: PathBuf,
    pub files: usize,
    pub reused_files: usize,
    pub scope: super::structure::StructureOptions,
    pub discovery_complete: bool,
    pub details_complete: bool,
    pub warnings: Vec<String>,
}

/// A ranked source candidate; relevance is not proof that this file needs changing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub file: FileRecord,
    pub lexical_score: usize,
    pub reasons: Vec<String>,
    /// Version-bound source spans selected by retrieval; empty lets the packet localize lexically.
    #[serde(default)]
    pub preferred_spans: Vec<(usize, usize)>,
    /// Materialize all source bytes when the selector needs evidence beyond its bounded card.
    #[serde(default)]
    pub whole_file: bool,
}

/// API and loopback embedding adapters, available in the existing model-enabled build.
#[cfg(feature = "jev-pilot")]
pub mod embedding;

/// Bounded semantic assessment using the reused Jev transport.
#[cfg(feature = "jev-pilot")]
pub mod select;

/// Opt-in orchestration of embedding retrieval and Jev selection.
#[cfg(feature = "jev-pilot")]
pub mod service;

/// Background source embedding and offline cost preflight; never called by task queries.
#[cfg(feature = "jev-pilot")]
pub mod indexer;

/// Source-validated public check facts used to ground task retrieval.
pub mod evidence;
