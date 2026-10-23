//! Bounded, version-checked operator packets; no delivery receipt or edit authority is created.
use super::{ContextIndex, IndexSnapshot};
use crate::project::{structure::Detail, token_estimate::estimate_tokens};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Component, PathBuf},
};

/// Caller-owned context budget and finite source limits; token counts remain estimates.
#[derive(Clone, Debug)]
pub struct PacketOptions {
    pub context_tokens: u64,
    pub reserved_tokens: u64,
    pub characters_per_token: f64,
    pub candidates: usize,
    pub max_file_bytes: usize,
    pub max_read_bytes: usize,
}
impl Default for PacketOptions {
    /// Keeps output small while allowing a few complete declarations from large files.
    fn default() -> Self {
        Self {
            context_tokens: 12_000,
            reserved_tokens: 2_000,
            characters_per_token: 4.0,
            candidates: 12,
            max_file_bytes: 2 * 1024 * 1024,
            max_read_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Exact UTF-8 source bytes selected from a freshly verified file version.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceWindow {
    pub path: PathBuf,
    pub content_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
    /// True only when this window contains every byte of the verified file.
    pub complete_file: bool,
    pub reasons: Vec<String>,
    /// Closed descriptive role: primary, dependency, test or support; never grants authority.
    #[serde(default)]
    pub role: String,
    /// Lower ranks are earlier retrieval choices.
    #[serde(default)]
    pub rank: u32,
    /// Dropping required evidence makes the delivered packet incomplete.
    #[serde(default)]
    pub required: bool,
    /// True for a whole file or a complete declaration from the source outline.
    #[serde(default)]
    pub unit_complete: bool,
    /// Source-backed declaration address, absent for whole files or unlocalized ranges.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// Compact declaration facts supplement bodies with their surrounding type/module context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeclarationFact {
    pub symbol: String,
    pub kind: String,
    pub signature: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Version-bound header, import and declaration facts for one emitted source file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: PathBuf,
    pub content_hash: String,
    pub header: Option<crate::project::structure::SourceExcerpt>,
    pub imports: Vec<crate::project::structure::SourceExcerpt>,
    pub declarations: Vec<DeclarationFact>,
    pub truncated: bool,
}

/// A nonexhaustive shortlist, not a guarantee that every required edit location was found.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextBundle {
    pub task: String,
    pub snapshot: IndexSnapshot,
    pub windows: Vec<SourceWindow>,
    /// Facts are supplementary; they do not imply delivery of unshown declaration bodies.
    #[serde(default)]
    pub file_facts: Vec<FileFacts>,
    pub gaps: Vec<String>,
    pub estimated_tokens: u64,
    pub accounting_is_estimated: bool,
}

impl ContextIndex {
    /// Prepares an initial lexical packet from the published snapshot. Changed/unreadable files
    /// become explicit gaps, never stale content. A too-small envelope budget is an error.
    pub fn prepare(&self, task: &str, options: &PacketOptions) -> io::Result<ContextBundle> {
        if options.max_file_bytes == 0
            || options.max_file_bytes > 16 * 1024 * 1024
            || options.max_read_bytes == 0
            || options.max_read_bytes > 128 * 1024 * 1024
        {
            return Err(io::Error::other("invalid packet read limits"));
        }
        let candidates = self.search(task, options.candidates)?;
        self.prepare_candidates(task, options, candidates)
    }

    /// Builds an operator packet from already ranked local candidates; source checks still apply.
    pub(crate) fn prepare_candidates(
        &self,
        task: &str,
        options: &PacketOptions,
        candidates: Vec<super::Candidate>,
    ) -> io::Result<ContextBundle> {
        if task.is_empty()
            || task.len() > 64 * 1024
            || candidates.len() > 256
            || options.max_file_bytes == 0
            || options.max_file_bytes > 16 * 1024 * 1024
            || options.max_read_bytes == 0
            || options.max_read_bytes > 128 * 1024 * 1024
        {
            return Err(io::Error::other("invalid packet task or limits"));
        }
        let snapshot = self
            .snapshot()?
            .ok_or_else(|| io::Error::other("index has not been built"))?;
        let mut packet = ContextBundle {
            task: task.into(),
            snapshot,
            windows: Vec::new(),
            file_facts: Vec::new(),
            gaps: Vec::new(),
            estimated_tokens: 0,
            accounting_is_estimated: true,
        };
        if !packet.snapshot.discovery_complete {
            packet.gaps.push("index discovery is incomplete".into());
        }
        if candidates.is_empty() {
            packet.gaps.push(
                "no lexical candidate found; semantic search or more task evidence is needed"
                    .into(),
            );
        }
        let terms: Vec<_> = task
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|term| term.len() > 1)
            .map(str::to_lowercase)
            .collect();
        let mut read = 0;
        for (rank, candidate) in candidates.into_iter().enumerate() {
            let record = candidate.file;
            let remaining = options
                .max_read_bytes
                .saturating_sub(read)
                .min(options.max_file_bytes);
            let content = read_current(&self.root, &record, remaining, &mut read);
            let content = match content {
                Ok(content) => content,
                Err(error) => {
                    packet
                        .gaps
                        .push(format!("{}: {error}", record.path.display()));
                    continue;
                }
            };
            let mut ranges: Vec<(usize, usize)> = candidate
                .preferred_spans
                .iter()
                .copied()
                .filter(|&(start, end)| {
                    start < end
                        && end <= content.len()
                        && content.is_char_boundary(start)
                        && content.is_char_boundary(end)
                })
                .collect();
            // Small complete files preserve imports and enclosing contracts with less repeated
            // window metadata than several fragments. The packet budget still bounds delivery.
            if candidate.whole_file || content.len() <= 8192 {
                ranges.clear();
                ranges.push((0, content.len()));
            } else if !ranges.is_empty() {
                ranges.sort_unstable();
                ranges.dedup();
            } else if let Some(Detail::Available(symbols)) = &record.symbols {
                let mut selected = Vec::new();
                for symbol in symbols {
                    symbol.walk(&mut |symbol| {
                        let text = format!(
                            "{} {} {}",
                            symbol.name,
                            symbol.signature,
                            symbol.doc.as_deref().unwrap_or_default()
                        )
                        .to_lowercase();
                        let score = terms
                            .iter()
                            .filter(|term| text.contains(term.as_str()))
                            .count();
                        if score > 0 {
                            selected.push((score, symbol.range));
                        }
                    });
                }
                selected.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                let mut lines = vec![0];
                lines.extend(content.match_indices('\n').map(|(at, _)| at + 1));
                for (_, range) in selected.into_iter().take(3) {
                    if range.start == 0
                        || range.end < range.start
                        || range.end as usize > lines.len()
                    {
                        continue;
                    }
                    let start = lines[range.start as usize - 1];
                    let end = lines
                        .get(range.end as usize)
                        .copied()
                        .unwrap_or(content.len());
                    if !ranges.iter().any(|&(a, b)| a <= start && b >= end) {
                        ranges.push((start, end));
                    }
                }
            }
            let units = declaration_units(&record, &content);
            if !candidate.whole_file && content.len() > 8192 && !ranges.is_empty() {
                let mut closed = Vec::new();
                for &(start, end) in &ranges {
                    let overlapping: Vec<_> = units
                        .iter()
                        .filter(|unit| unit.start < end && unit.end > start)
                        .collect();
                    if overlapping.is_empty() {
                        closed.push((start, end));
                    } else {
                        closed.extend(overlapping.into_iter().map(|unit| (unit.start, unit.end)));
                    }
                }
                let mut seen = std::collections::BTreeSet::new();
                closed.retain(|range| seen.insert(*range));
                ranges = closed;
            }
            if ranges.is_empty() {
                packet.gaps.push(format!(
                    "{}: declaration not localized; further selection needed",
                    record.path.display()
                ));
            }
            if !ranges.is_empty() {
                packet.file_facts.push(file_facts(&record, &content));
            }
            for (start, end) in ranges {
                let whole = start == 0 && end == content.len();
                let unit = units
                    .iter()
                    .find(|unit| unit.start == start && unit.end == end);
                let is_test = unit.is_some_and(|unit| unit.kind == crate::lang::SymbolKind::Test)
                    || (whole
                        && crate::lang::Language::for_path(&record.path)
                            .is_some_and(|language| language.support().is_test_file(&record.path)));
                let role = if is_test {
                    "test"
                } else if rank == 0 {
                    "primary"
                } else {
                    "dependency"
                };
                if !whole && unit.is_none() {
                    packet.gaps.push(format!(
                        "{}: bytes {start}..{end} are not a complete declaration",
                        record.path.display()
                    ));
                }
                packet.windows.push(SourceWindow {
                    path: record.path.clone(),
                    content_hash: record.content_hash.clone().unwrap(),
                    start_byte: start,
                    end_byte: end,
                    text: content[start..end].into(),
                    complete_file: whole,
                    role: role.into(),
                    rank: rank as u32,
                    required: true,
                    unit_complete: whole || unit.is_some(),
                    symbol: unit.map(|unit| unit.symbol.clone()),
                    reasons: candidate
                        .reasons
                        .iter()
                        .filter(|reason| informative_reason(reason))
                        .cloned()
                        .collect(),
                });
            }
        }
        // Count the complete wire envelope, including task and explicit omission notes.
        loop {
            let encoded = serde_json::to_string_pretty(&packet).map_err(io::Error::other)?;
            let estimate = estimate_tokens(
                &encoded,
                options.characters_per_token,
                options.context_tokens,
                options.reserved_tokens,
            )
            .map_err(io::Error::other)?;
            if estimate.estimated_fits {
                if packet.estimated_tokens == estimate.estimated_tokens {
                    return Ok(packet);
                }
                packet.estimated_tokens = estimate.estimated_tokens;
                continue;
            }
            let Some(window) = packet.windows.pop() else {
                return Err(io::Error::other(
                    "task and packet metadata exceed context budget",
                ));
            };
            packet.gaps.push(format!(
                "{}: source omitted by context budget",
                window.path.display()
            ));
            packet.file_facts.retain(|facts| {
                packet
                    .windows
                    .iter()
                    .any(|window| window.path == facts.path)
            });
        }
    }
}

/// Reads one current, bounded source version; the byte ledger includes failed freshness checks.
pub(super) fn read_current(
    root: &std::path::Path,
    record: &super::FileRecord,
    remaining: usize,
    read: &mut usize,
) -> io::Result<String> {
    if record.path.as_os_str().is_empty()
        || record
            .path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || record
            .path
            .components()
            .any(|part| part.as_os_str() == ".agent-ide" || part.as_os_str() == ".git")
    {
        return Err(io::Error::other("invalid source path"));
    }
    let expected = record
        .content_hash
        .as_ref()
        .ok_or_else(|| io::Error::other("path-only record; source extraction unavailable"))?;
    let path = root.join(&record.path);
    if path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .as_deref()
        != path.parent()
    {
        return Err(io::Error::other("source ancestor changed"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || remaining == 0 || metadata.len() > remaining as u64 {
        return Err(io::Error::other("source read limit or nonregular file"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(remaining.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    *read += bytes.len();
    if bytes.len() > remaining {
        return Err(io::Error::other("source grew beyond limit"));
    }
    if blake3::hash(&bytes).to_hex().as_str() != expected {
        return Err(io::Error::other(
            "source changed since indexing; refresh required",
        ));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

/// Complete callable/test units and type declarations, excluding implementation/module wrappers.
pub(super) struct DeclarationUnit {
    pub(super) start: usize,
    pub(super) end: usize,
    symbol: String,
    kind: crate::lang::SymbolKind,
}

/// Converts header-inclusive outline lines to original byte ranges without splitting nested functions.
pub(super) fn declaration_units(record: &super::FileRecord, source: &str) -> Vec<DeclarationUnit> {
    use crate::lang::{Symbol, SymbolKind};
    fn visit(symbol: &Symbol, lines: &[usize], length: usize, units: &mut Vec<DeclarationUnit>) {
        if symbol.kind.is_container()
            && !matches!(
                symbol.kind,
                SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Interface | SymbolKind::Trait
            )
            && !symbol.children.is_empty()
        {
            for child in &symbol.children {
                visit(child, lines, length, units);
            }
            return;
        }
        if symbol.range.start == 0
            || symbol.range.end < symbol.range.start
            || symbol.range.end as usize > lines.len()
        {
            return;
        }
        let start = lines[symbol.range.start as usize - 1];
        let end = lines
            .get(symbol.range.end as usize)
            .copied()
            .unwrap_or(length);
        if start < end {
            units.push(DeclarationUnit {
                start,
                end,
                symbol: symbol.path.to_string(),
                kind: symbol.kind,
            });
        }
    }
    let mut lines = vec![0];
    lines.extend(source.match_indices('\n').map(|(at, _)| at + 1));
    let mut units = Vec::new();
    if let Some(Detail::Available(symbols)) = &record.symbols {
        for symbol in symbols {
            visit(symbol, &lines, source.len(), &mut units);
        }
    }
    units
}

/// Carries bounded actual facts, marking omitted declarations and inherited extraction limits.
fn file_facts(record: &super::FileRecord, source: &str) -> FileFacts {
    let mut header = match &record.header {
        Detail::Available(value) => Some(value.clone()),
        _ => None,
    };
    let imports = match &record.imports {
        Detail::Available(value) => value.statements.clone(),
        _ => Vec::new(),
    };
    // Expand a clipped module header locally, without changing the embedding/model input.
    if header.as_ref().is_some_and(|header| header.truncated)
        && let Some(language) = crate::lang::Language::for_path(&record.path)
    {
        let lines: Vec<_> = source.lines().take(129).collect();
        if let Some(region) = crate::lang::regions::scan_gap(
            &lines,
            1,
            lines.len() as u32,
            true,
            language.support().region_rules(),
        )
        .into_iter()
        .find(|region| region.kind == crate::lang::regions::RawKind::Header)
        {
            let end = (region.span.end as usize).min(128);
            if lines[..end].iter().map(|line| line.len()).sum::<usize>() <= 8192 {
                header = Some(crate::project::structure::SourceExcerpt {
                    text: lines[..end]
                        .iter()
                        .map(|line| crate::project::structure::header_text(line))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim_end()
                        .into(),
                    start_line: 1,
                    end_line: end as u32,
                    truncated: region.span.end > 128,
                });
            }
        }
    }
    let mut truncated = header.as_ref().is_some_and(|header| header.truncated)
        || matches!(&record.imports, Detail::Available(imports) if imports.truncated)
        || matches!(&record.symbols, None | Some(Detail::Unavailable { .. }));
    let mut declarations = Vec::new();
    let mut bytes = 0;
    if let Some(Detail::Available(symbols)) = &record.symbols {
        for symbol in symbols {
            symbol.walk(&mut |symbol| {
                let size = symbol.signature.len() + symbol.path.to_string().len();
                if declarations.len() >= 64 || bytes + size > 4096 {
                    truncated = true;
                    return;
                }
                bytes += size;
                declarations.push(DeclarationFact {
                    symbol: symbol.path.to_string(),
                    kind: symbol.kind.name().into(),
                    signature: symbol.signature.clone(),
                    start_line: symbol.range.start,
                    end_line: symbol.range.end,
                });
            });
        }
    }
    FileFacts {
        path: record.path.clone(),
        content_hash: record.content_hash.clone().unwrap_or_default(),
        header,
        imports,
        declarations,
        truncated,
    }
}

/// Removes generic search scaffolding from agent-facing reasons; raw model traces stay exact.
fn informative_reason(reason: &str) -> bool {
    reason.strip_prefix("literal term: ").is_none_or(|term| {
        !matches!(
            term,
            "a" | "an"
                | "the"
                | "and"
                | "or"
                | "of"
                | "in"
                | "on"
                | "at"
                | "by"
                | "to"
                | "for"
                | "with"
                | "from"
                | "as"
                | "is"
                | "are"
                | "be"
                | "this"
                | "that"
                | "it"
                | "its"
                | "before"
                | "after"
                | "и"
                | "или"
                | "в"
                | "во"
                | "на"
                | "по"
                | "к"
                | "из"
                | "с"
                | "со"
                | "о"
                | "от"
                | "до"
                | "для"
                | "это"
                | "как"
                | "что"
                | "не"
        )
    })
}
