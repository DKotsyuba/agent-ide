//! Replaceable fail-open telemetry boundary with no storage or export implementation.

use crate::changes::edit::EditOutcome;

/// Coarse diagnostic refresh state that cannot contain source or diagnostic messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditDiagnosticState {
    /// A post-effect source observation was durably refreshed; provider detail remains external.
    Refreshed,
    /// No current diagnostic refresh was established.
    Unknown,
    /// The edit made no write, so post-effect refresh was not required.
    NotApplicable,
}

/// Bounded serialized-result size class without retaining exact path- or identifier-dependent size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputSizeClass {
    /// Serialized closed result occupied at most 256 bytes.
    Small,
    /// Serialized closed result occupied 257 through 1024 bytes.
    Medium,
    /// Serialized closed result occupied more than 1024 bytes within the Assistance cap.
    Large,
}

/// Sanitized completion fact for `ide.edit`; it contains no content, path, ID, or free-form text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditTelemetryFact {
    /// Closed Changes outcome tag.
    pub outcome: EditOutcome,
    /// Queue-through-settlement elapsed milliseconds, saturated to a bounded unsigned value.
    pub duration_ms: u32,
    /// Whether post-effect source/diagnostic refresh was established.
    pub diagnostics: EditDiagnosticState,
    /// Coarse size of the already-bounded closed result.
    pub output_size: OutputSizeClass,
}

/// Closed reason why an observed native write path remained in use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFallbackReason {
    /// A host-native writer was observed independently of preferred edit availability.
    NativeSelected,
}

/// Receives only privacy-safe facts and must return immediately without affecting product work.
///
/// Implementations must not block, panic, retry, perform authority decisions, or retain caller
/// identifiers. Queue-full or storage failure drops only the fact. This release ships only the
/// no-op substitute and deliberately implements no telemetry persistence, queries, or export.
pub trait EditTelemetry: Send + Sync {
    /// Attempts to accept one sanitized edit-completion fact; failure is intentionally unobservable.
    fn edit_completed(&self, fact: EditTelemetryFact);

    /// Attempts to accept one sanitized native-fallback observation with no path or content.
    fn native_fallback(&self, reason: NativeFallbackReason);
}

/// Shipped fail-open substitute that drops every fact synchronously.
#[derive(Debug, Default)]
pub struct NoopEditTelemetry;

impl EditTelemetry for NoopEditTelemetry {
    /// Drops one sanitized edit fact without allocation, I/O, or product-visible failure.
    fn edit_completed(&self, _fact: EditTelemetryFact) {}

    /// Drops one sanitized fallback fact without allocation, I/O, or product-visible failure.
    fn native_fallback(&self, _reason: NativeFallbackReason) {}
}

/// Classifies one already-bounded serialized result without exposing its exact byte count.
pub const fn output_size(bytes: usize) -> OutputSizeClass {
    if bytes <= 256 {
        OutputSizeClass::Small
    } else if bytes <= 1024 {
        OutputSizeClass::Medium
    } else {
        OutputSizeClass::Large
    }
}
