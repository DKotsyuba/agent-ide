//! Bounded context over exact Workspace bytes, with explicit semantic and lexical evidence.

use super::freshness::{Freshness, SourceBinding, ViewGeneration};
use crate::workspace::observation::{MAX_SOURCE_BYTES, SourceBytes, SourceObservation};
use async_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, PositionEncodingKind, Url};

/// Maximum retained text in one context response, independent of provider availability.
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;
/// Maximum retained semantic locations or lexical matches in one response.
pub const MAX_CONTEXT_ITEMS: usize = 128;

/// Selects an exact file or the identifier at a zero-based UTF-8 byte offset.
#[derive(Clone, Copy, Debug)]
pub enum ContextQuery {
    /// Returns a bounded prefix of the exact observed file.
    File,
    /// Looks up the identifier containing this byte; offsets inside UTF-8 code points are rejected.
    Symbol {
        /// Zero-based UTF-8 byte offset in the exact observed file.
        byte_offset: usize,
    },
}

/// Distinguishes provider evidence from bounded exact-file lexical evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextMode {
    /// The provider completed every advertised requested semantic operation.
    Semantic,
    /// Semantic service was unavailable or did not advertise the requested operations.
    Lexical {
        /// Bounded explanation of why the result contains lexical evidence.
        reason: String,
    },
}

/// Returns bounded source and locations with enough provenance to reject superseded evidence.
#[derive(Clone, Debug)]
pub struct ContextResult {
    /// Exact Workspace binding; consumers must compare it to their current observation.
    pub source: SourceBinding,
    /// Provider incarnation fences, absent when no provider participated.
    pub generation: Option<ViewGeneration>,
    /// Positive synchronized LSP document version, absent for provider-free fallback.
    pub document_version: Option<i32>,
    /// Exact percent-encoded file URI constructed from raw Workspace paths.
    pub uri: Url,
    /// Position units used by all returned provider and lexical locations.
    pub position_encoding: PositionEncodingKind,
    /// Explicit provenance; lexical matches never masquerade as semantic references.
    pub mode: ContextMode,
    /// Freshness of this exact source observation, not a claim of workspace-wide readiness.
    pub freshness: Freshness,
    /// Exact UTF-8 prefix of the observed bytes, capped at `MAX_CONTEXT_BYTES`.
    pub text: String,
    /// Identifier selected by the query, absent for a file query or nonidentifier position.
    pub symbol: Option<String>,
    /// Provider definitions; `None` means unsupported/unavailable, `Some([])` is an empty reply.
    pub definitions: Option<Vec<Location>>,
    /// Provider references; `None` means unsupported/unavailable, `Some([])` is an empty reply.
    pub references: Option<Vec<Location>>,
    /// Exact-file lexical token matches, never a project-wide symbol identity claim.
    pub lexical_matches: Vec<Location>,
    /// At least one text or location ceiling omitted requested data.
    pub truncated: bool,
}

/// Validates digest, size, presence and UTF-8 against the supplied immutable Workspace observation.
/// Missing-path observations accept only empty bytes. Mismatched, oversized or non-UTF-8 bytes fail.
pub(crate) fn observed_text<'a>(
    observation: &SourceObservation,
    bytes: &'a [u8],
) -> std::io::Result<&'a str> {
    if observation.bytes().is_none() && bytes.is_empty() {
        return Ok("");
    }
    if bytes.len() > MAX_SOURCE_BYTES
        || observation.bytes() != Some(&SourceBytes::from_bytes(bytes))
    {
        return Err(invalid(
            "bytes do not match the exact present Workspace observation",
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| invalid("LSP context requires UTF-8 source"))
}

/// Constructs the exact raw-path file URI without reading or canonicalizing mutable disk state.
pub(crate) fn observation_uri(observation: &SourceObservation) -> std::io::Result<Url> {
    Url::from_file_path(
        observation
            .worktree()
            .worktree_path()
            .join(observation.path()),
    )
    .map_err(|_| invalid("Workspace path cannot be represented as a file URI"))
}

/// Computes a zero-based LSP position for an exact byte boundary using the negotiated encoding.
/// Rejects out-of-range and split-code-point offsets and encodings other than UTF-8/16/32.
pub fn position(
    text: &str,
    offset: usize,
    encoding: &PositionEncodingKind,
) -> std::io::Result<Position> {
    let before = text
        .get(..offset)
        .ok_or_else(|| invalid("invalid UTF-8 byte offset"))?;
    let line = before.bytes().filter(|byte| *byte == b'\n').count();
    let tail = before.rsplit('\n').next().unwrap_or("");
    let character = if encoding == &PositionEncodingKind::UTF8 {
        tail.len()
    } else if encoding == &PositionEncodingKind::UTF16 {
        tail.encode_utf16().count()
    } else if encoding == &PositionEncodingKind::UTF32 {
        tail.chars().count()
    } else {
        return Err(invalid("unsupported LSP position encoding"));
    };
    Ok(Position::new(
        u32::try_from(line).map_err(|_| invalid("LSP line exceeds u32"))?,
        u32::try_from(character).map_err(|_| invalid("LSP character exceeds u32"))?,
    ))
}

/// Returns bounded lexical context from the exact observation; never opens files or starts a provider.
/// `reason` is a caller-supplied unavailability label capped at 256 bytes; malformed source/query fails.
pub fn lexical_context(
    observation: &SourceObservation,
    bytes: &[u8],
    query: ContextQuery,
    reason: &str,
) -> std::io::Result<ContextResult> {
    let text = observed_text(observation, bytes)?;
    let uri = observation_uri(observation)?;
    let encoding = PositionEncodingKind::UTF16;
    let symbol = match query {
        ContextQuery::File => None,
        ContextQuery::Symbol { byte_offset } => {
            position(text, byte_offset, &encoding)?;
            token_at(text, byte_offset)
        }
    };
    let mut matches = Vec::new();
    let mut truncated = text.len() > MAX_CONTEXT_BYTES;
    if let Some(symbol) = &symbol {
        for (start, candidate) in text.match_indices(symbol) {
            let end = start + candidate.len();
            if text[..start].chars().next_back().is_some_and(identifier)
                || text[end..].chars().next().is_some_and(identifier)
            {
                continue;
            }
            if matches.len() == MAX_CONTEXT_ITEMS {
                truncated = true;
                break;
            }
            matches.push(Location::new(
                uri.clone(),
                async_lsp::lsp_types::Range::new(
                    position(text, start, &encoding)?,
                    position(text, end, &encoding)?,
                ),
            ));
        }
    }
    Ok(ContextResult {
        source: SourceBinding::from_observation(observation),
        generation: None,
        document_version: None,
        uri,
        position_encoding: encoding,
        mode: ContextMode::Lexical {
            reason: prefix(reason, 256).to_owned(),
        },
        freshness: if observation.coverage().is_complete() {
            Freshness::Current
        } else {
            Freshness::Unknown
        },
        text: prefix(text, MAX_CONTEXT_BYTES).to_owned(),
        symbol,
        definitions: None,
        references: None,
        lexical_matches: matches,
        truncated,
    })
}

/// Returns the identifier containing the offset; punctuation, EOF and whitespace produce no symbol.
fn token_at(text: &str, offset: usize) -> Option<String> {
    if !text[offset..].chars().next().is_some_and(identifier) {
        return None;
    }
    let start = text[..offset]
        .char_indices()
        .rev()
        .find(|(_, ch)| !identifier(*ch))
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let end = text[offset..]
        .char_indices()
        .find(|(_, ch)| !identifier(*ch))
        .map_or(text.len(), |(index, _)| offset + index);
    Some(prefix(&text[start..end], MAX_CONTEXT_BYTES).to_owned())
}

/// Recognizes lexical identifier characters without claiming language-specific symbol semantics.
fn identifier(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// Returns the largest UTF-8 prefix at or below a byte ceiling.
pub(crate) fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Normalizes all standard LSP definition result shapes to locations and bounds their retained count.
pub(crate) fn definitions(result: Option<GotoDefinitionResponse>) -> (Vec<Location>, bool) {
    let mut locations = match result {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location::new(link.target_uri, link.target_selection_range))
            .collect(),
    };
    let truncated = locations.len() > MAX_CONTEXT_ITEMS;
    locations.truncate(MAX_CONTEXT_ITEMS);
    (locations, truncated)
}

/// Creates a stable invalid-input error for a violated observation or query boundary.
pub(crate) fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}
