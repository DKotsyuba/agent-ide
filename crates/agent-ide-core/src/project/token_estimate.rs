//! Cheap model-independent token estimates for the exact rendered text, not its source files.

/// An approximate count and budget comparison; no model tokenizer or network is used.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenEstimate {
    /// UTF-8 size of the rendered input.
    pub bytes: usize,
    /// Number of Unicode scalar values, including whitespace and tree decoration.
    pub characters: usize,
    /// Ceiling of characters divided by the caller's positive characters-per-token estimate.
    pub estimated_tokens: u64,
    /// Context remaining after the caller's prompt/history/output reservation, floored at zero.
    pub available_tokens: u64,
    /// Heuristic fit only; tokenization differences can still make the real request too large.
    pub estimated_fits: bool,
}

/// Why a meaningful finite estimate could not be computed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EstimateError {
    /// Characters per token must be finite and strictly positive.
    InvalidRatio,
    /// The estimate does not fit the returned unsigned count.
    Overflow,
}

impl std::fmt::Display for EstimateError {
    /// Stable short errors suitable for operator callers.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRatio => "characters_per_token must be finite and positive",
            Self::Overflow => "token estimate exceeds u64",
        })
    }
}
impl std::error::Error for EstimateError {}

/// Conventional rough ratio for English/code; not a measured tokenizer guarantee.
pub const DEFAULT_CHARACTERS_PER_TOKEN: f64 = 4.0;

/// Estimates the exact supplied rendered string, with all decorations and optional sections.
/// Pass the model's context limit and reserve for instructions, history and desired response.
/// Unicode scripts, identifiers and punctuation can differ substantially from this heuristic;
/// use the actual model tokenizer when a guaranteed context-bound decision is required.
///
/// # Errors
/// Rejects nonpositive/nonfinite ratios and estimates overflowing `u64`.
pub fn estimate_tokens(
    rendered: &str,
    characters_per_token: f64,
    context_tokens: u64,
    reserved_tokens: u64,
) -> Result<TokenEstimate, EstimateError> {
    if !characters_per_token.is_finite() || characters_per_token <= 0.0 {
        return Err(EstimateError::InvalidRatio);
    }
    let characters = rendered.chars().count();
    let estimate = (characters as f64 / characters_per_token).ceil();
    if !estimate.is_finite() || estimate >= u64::MAX as f64 {
        return Err(EstimateError::Overflow);
    }
    let estimated_tokens = estimate as u64;
    let available_tokens = context_tokens.saturating_sub(reserved_tokens);
    Ok(TokenEstimate {
        bytes: rendered.len(),
        characters,
        estimated_tokens,
        available_tokens,
        estimated_fits: reserved_tokens <= context_tokens && estimated_tokens <= available_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTF-8 byte size is not silently treated as a character count.
    #[test]
    fn unicode_and_budget_boundaries_are_explicit() {
        let count = estimate_tokens("└ файл😀", 4.0, 12, 10).unwrap();
        assert_eq!(count.characters, 7);
        assert!(count.bytes > count.characters);
        assert_eq!(count.estimated_tokens, 2);
        assert!(count.estimated_fits);
        assert!(
            !estimate_tokens("12345", 4.0, 11, 10)
                .unwrap()
                .estimated_fits
        );
        assert!(estimate_tokens("", 4.0, 0, 0).unwrap().estimated_fits);
        assert!(!estimate_tokens("", 4.0, 0, 1).unwrap().estimated_fits);
    }

    /// Invalid ratios and overflow never produce a falsely small estimate.
    #[test]
    fn invalid_and_overflowing_ratios_refuse() {
        for ratio in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                estimate_tokens("abc", ratio, 100, 0),
                Err(EstimateError::InvalidRatio)
            );
        }
        assert_eq!(
            estimate_tokens("abc", f64::MIN_POSITIVE, 100, 0),
            Err(EstimateError::Overflow)
        );
    }
}
