//! Renders the bounded `<agent-ide>` problem block and tracks per-actor delivery state (EYES-r1 §6).
//!
//! The feed turns the latest completed [`ProblemSnapshot`](crate::checks::ProblemSnapshot)s of an actor's worktree into one compact
//! block: fixed language order, no paths, messages or codes (a failed check adds at most 80 bytes of sanitized detail), at most [`MAX_BLOCK_BYTES`](crate::feed::MAX_BLOCK_BYTES) bytes
//! including tags. [`FeedState`](crate::feed::FeedState) remembers the last block delivered per (actor binding, worktree)
//! so identical state is never re-emitted and changed counts render as `(+N)`/`(-N)` deltas. The
//! state is bounded and purely in-memory; hook and IPC wiring live elsewhere.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crate::checks::{CheckState, Language, ProblemSnapshot, Recheck, UnavailableReason};

/// Maximum UTF-8 byte length of one rendered `<agent-ide>` block, tags included (EYES-r1 §6).
pub const MAX_BLOCK_BYTES: usize = 256;

/// Maximum number of keys [`FeedState`] remembers; further keys evict the least recently used one.
pub const MAX_FEED_KEYS: usize = 1024;

/// Maximum UTF-8 byte length of the detail appended to a `check failed` item.
const MAX_DETAIL_BYTES: usize = 80;

/// Separator between rendered language items inside one block.
const ITEM_SEPARATOR: &str = " | ";

/// Identifies one block audience: an opaque actor binding plus the worktree it works in.
///
/// Emission dedup, deltas and the delivery record are all scoped to this key: the same problem
/// state delivered to two bindings (or two worktrees) emits once per key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FeedKey {
    /// Opaque stable actor-binding identifier supplied by the caller; never interpreted here.
    pub binding: String,
    /// Worktree path the actor's snapshots belong to.
    pub worktree: PathBuf,
}

/// One language's contribution to a block, before delta annotation.
#[derive(Clone, Debug)]
enum ItemState {
    /// Completed result with numeric counts, optionally partial coverage.
    Counts {
        /// Total deduplicated error count.
        errors: u32,
        /// Total deduplicated warning count.
        warnings: u32,
        /// `true` when coverage was incomplete (`Partial` state).
        partial: bool,
    },
    /// No result; renders the fixed unavailable phrase for the carried reason, plus the sanitized
    /// detail for a failed check.
    Unavailable(UnavailableReason, Option<String>),
    /// A check is due or running; the last known counts, if any, stay visible.
    Checking {
        /// `false` for the first check of the session, `true` when a check for changed files runs.
        files_changed: bool,
        /// Last known `(errors, warnings)` shown inside the checking text.
        last: Option<(u32, u32)>,
    },
}

/// One candidate block item: a language plus its current state.
#[derive(Clone, Debug)]
struct FeedItem {
    /// Language this item describes; fixes the item's position in the block.
    language: Language,
    /// Latest state of this language's check.
    state: ItemState,
}

/// Delivery state remembered for one [`FeedKey`].
#[derive(Clone, Debug)]
struct DeliveredFeed {
    /// Last delivered item set without deltas, joined with [`ITEM_SEPARATOR`]; the emission
    /// comparison value.
    content: String,
    /// Last delivered numeric counts per language; a language absent here has never delivered
    /// counts, so its first numeric item renders without deltas.
    counts: Vec<(Language, u32, u32)>,
}

/// Bounded emission state for `<agent-ide>` blocks (EYES-r1 §6).
///
/// For every [`FeedKey`] it remembers the last delivered block content and counts, so
/// [`FeedState::next_block`] can suppress identical re-delivery and annotate changed counts with
/// deltas. At most [`MAX_FEED_KEYS`] keys are retained; admitting a further key evicts the least
/// recently used one. Purely in-memory and not synchronized: callers sharing one state must wrap
/// it in a lock.
#[derive(Debug, Default)]
pub struct FeedState {
    /// Delivery state per key.
    delivered: HashMap<FeedKey, DeliveredFeed>,
    /// Delivered keys in recency order, least recently used first; always a permutation of
    /// `delivered`.
    recency: VecDeque<FeedKey>,
}

impl FeedState {
    /// Builds the status plate for `snapshots` and returns it, or `None` when nothing changed.
    ///
    /// The block is a status plate (T18B): it names each language's current state, process states
    /// included, and is (re)sent whenever the rendered status changes — never while it is
    /// unchanged. Items render in fixed [`Language`] order (rust, python), skipping languages
    /// absent from the worktree (T10B: `Unavailable(Disabled)`) and languages without any
    /// snapshot; the last snapshot of a language wins. A language in `rechecks` — or with a
    /// `Checking` snapshot — renders `checking (first check)` or `checking (files changed; last
    /// result: N errors, M warnings)` instead of stale counts. When every configured language is
    /// absent or `snapshots` is empty, no block is emitted. When the item set without deltas equals
    /// the last block delivered for `key`, returns `None` and changes nothing. Otherwise deltas
    /// `(+N)`/`(-N)` are inserted after each count that changed versus the last delivered counts
    /// of the same language (a language's first numeric delivery has no deltas). An oversized
    /// block shrinks in order: deltas are dropped, then details and last-result texts are
    /// dropped, and finally — only if it still exceeds the cap — every item is hard-truncated on a
    /// UTF-8 boundary; a language is never dropped, so [`MAX_BLOCK_BYTES`] is never exceeded. The
    /// result records as delivered for `key`, which also marks `key` most recently used.
    pub fn next_block(
        &mut self,
        key: &FeedKey,
        snapshots: &[ProblemSnapshot],
        rechecks: &[(Language, Recheck)],
    ) -> Option<String> {
        self.next_block_when(key, snapshots, rechecks, |_| true)
    }

    /// Like [`FeedState::next_block`], but records the block as delivered only when `fits`
    /// accepts the rendered text (T28B).
    ///
    /// The record happens atomically with the `fits` decision: a refused block is never marked
    /// delivered and stays due for the next call, so a carrier that could not hold the whole
    /// block loses nothing. Nothing changed means `None` either way.
    pub fn next_block_when(
        &mut self,
        key: &FeedKey,
        snapshots: &[ProblemSnapshot],
        rechecks: &[(Language, Recheck)],
        fits: impl FnOnce(&str) -> bool,
    ) -> Option<String> {
        let (block, delivered) = self.build_block(key, snapshots, rechecks)?;
        if !fits(&block) {
            return None;
        }
        self.record(key, delivered);
        Some(block)
    }

    /// Renders the due block for `key` without recording delivery, returning it with the
    /// delivery record a commit would store; `None` when nothing changed.
    ///
    /// This is [`FeedState::next_block`]'s exact body up to the record: the unchanged-state path
    /// still touches recency, and the returned record is what [`FeedState::record`] would store.
    fn build_block(
        &mut self,
        key: &FeedKey,
        snapshots: &[ProblemSnapshot],
        rechecks: &[(Language, Recheck)],
    ) -> Option<(String, DeliveredFeed)> {
        let items = build_items(snapshots, rechecks);
        if items.is_empty() {
            return None;
        }
        let content = items
            .iter()
            .map(|item| render_item(item, None, false))
            .collect::<Vec<_>>()
            .join(ITEM_SEPARATOR);
        if self
            .delivered
            .get(key)
            .is_some_and(|delivered| delivered.content == content)
        {
            self.touch(key);
            return None;
        }
        let last_counts = |language: Language| {
            self.delivered.get(key).and_then(|delivered| {
                delivered
                    .counts
                    .iter()
                    .find(|(delivered_language, _, _)| *delivered_language == language)
                    .map(|(_, errors, warnings)| (*errors, *warnings))
            })
        };
        let mut rendered: Vec<String> = items
            .iter()
            .map(|item| render_item(item, last_counts(item.language), false))
            .collect();
        let mut block = wrap_block(&rendered);
        if block.len() > MAX_BLOCK_BYTES {
            rendered = items
                .iter()
                .map(|item| render_item(item, None, true))
                .collect();
            block = wrap_block(&rendered);
            if block.len() > MAX_BLOCK_BYTES {
                // Compact items are short, so this only guards against implausibly large counts:
                // give every item an equal share, on UTF-8 boundaries, and keep every language.
                let overhead = wrap_block(&[]).len() + ITEM_SEPARATOR.len() * (rendered.len() - 1);
                let share = MAX_BLOCK_BYTES.saturating_sub(overhead) / rendered.len();
                for item in &mut rendered {
                    *item = truncate_to_byte_len(item, share);
                }
                block = wrap_block(&rendered);
            }
        }
        debug_assert!(
            block.len() <= MAX_BLOCK_BYTES,
            "rendered block must never exceed MAX_BLOCK_BYTES"
        );
        // A checking item keeps its language's last delivered counts as the delta baseline, so
        // the result that follows renders its change against them.
        let counts: Vec<(Language, u32, u32)> = items
            .iter()
            .filter_map(|item| match &item.state {
                ItemState::Counts {
                    errors, warnings, ..
                } => Some((item.language, *errors, *warnings)),
                ItemState::Checking { .. } => last_counts(item.language)
                    .map(|(errors, warnings)| (item.language, errors, warnings)),
                ItemState::Unavailable(..) => None,
            })
            .collect();
        Some((block, DeliveredFeed { content, counts }))
    }

    /// Drops the delivery state for `key`; called when the actor stops.
    ///
    /// The next [`FeedState::next_block`] for `key` behaves like a first delivery: it emits
    /// without deltas even for unchanged problem state. Forgetting an unknown key is a no-op.
    pub fn forget(&mut self, key: &FeedKey) {
        if self.delivered.remove(key).is_some() {
            self.recency.retain(|queued| queued != key);
        }
    }

    /// Records `delivered` for `key`, marks it most recently used, and evicts the least recently
    /// used key when the map outgrows [`MAX_FEED_KEYS`].
    fn record(&mut self, key: &FeedKey, delivered: DeliveredFeed) {
        self.recency.retain(|queued| queued != key);
        self.recency.push_back(key.clone());
        self.delivered.insert(key.clone(), delivered);
        while self.delivered.len() > MAX_FEED_KEYS {
            let Some(evicted) = self.recency.pop_front() else {
                break;
            };
            self.delivered.remove(&evicted);
        }
    }

    /// Marks an already delivered `key` most recently used without changing its state.
    fn touch(&mut self, key: &FeedKey) {
        if self.delivered.contains_key(key) {
            self.recency.retain(|queued| queued != key);
            self.recency.push_back(key.clone());
        }
    }
}

/// Builds the rendered items for `snapshots` in fixed [`Language`] order.
///
/// Several snapshots per language are allowed; the last one in `snapshots` wins, matching the
/// scheduler's latest-completed-wins rule. A language with no snapshot is skipped. A language
/// absent from the worktree (T10B: `Unavailable(Disabled)`) is skipped too — it renders as
/// nothing rather than a fixed unavailable phrase, so a project that only has one of the two
/// languages never mentions the other. A `Checking` snapshot or a [`Recheck::FirstCheck`] renders
/// as the first check of the session; a [`Recheck::FilesChanged`] keeps the last counts in view.
fn build_items(snapshots: &[ProblemSnapshot], rechecks: &[(Language, Recheck)]) -> Vec<FeedItem> {
    let mut items = Vec::new();
    for language in [Language::Rust, Language::Python] {
        let Some(snapshot) = snapshots.iter().rev().find(|s| s.language == language) else {
            continue;
        };
        let recheck = rechecks
            .iter()
            .rev()
            .find(|(recheck_language, _)| *recheck_language == language)
            .map(|(_, recheck)| *recheck);
        let first_check = ItemState::Checking {
            files_changed: false,
            last: None,
        };
        let state = match (&snapshot.state, recheck) {
            (CheckState::Unavailable(UnavailableReason::Disabled), _) => continue,
            (CheckState::Checking, _) | (_, Some(Recheck::FirstCheck)) => first_check,
            (CheckState::Ready | CheckState::Partial, Some(Recheck::FilesChanged)) => {
                ItemState::Checking {
                    files_changed: true,
                    last: Some((snapshot.errors, snapshot.warnings)),
                }
            }
            (CheckState::Unavailable(_), Some(Recheck::FilesChanged)) => ItemState::Checking {
                files_changed: true,
                last: None,
            },
            (CheckState::Ready | CheckState::Partial, None) => ItemState::Counts {
                errors: snapshot.errors,
                warnings: snapshot.warnings,
                partial: matches!(snapshot.state, CheckState::Partial),
            },
            (CheckState::Unavailable(reason), None) => ItemState::Unavailable(
                *reason,
                matches!(reason, UnavailableReason::Fatal)
                    .then(|| snapshot.detail.as_deref().and_then(feed_detail))
                    .flatten(),
            ),
        };
        items.push(FeedItem { language, state });
    }
    items
}

/// Reduces a checker detail to one plain line of at most [`MAX_DETAIL_BYTES`] bytes.
///
/// The detail is untrusted checker output: control characters become spaces and angle brackets
/// become `?`, so it can neither break the single-line block nor forge its framing tags. `None`
/// when nothing printable remains.
fn feed_detail(detail: &str) -> Option<String> {
    let plain: String = detail
        .chars()
        .map(|c| match c {
            c if c.is_control() => ' ',
            '<' | '>' => '?',
            c => c,
        })
        .collect();
    let cut = truncate_to_byte_len(plain.trim(), MAX_DETAIL_BYTES);
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_owned())
}

/// Renders one item, annotating numeric counts with deltas versus `last_counts`.
///
/// `last_counts` is the last delivered `(errors, warnings)` of the item's language, or `None` for
/// a first numeric delivery or a non-numeric item; both render without deltas. `compact` drops
/// the failure detail and the last-result text, the over-cap form.
fn render_item(item: &FeedItem, last_counts: Option<(u32, u32)>, compact: bool) -> String {
    let language = item.language.as_str();
    match &item.state {
        ItemState::Counts {
            errors,
            warnings,
            partial,
        } => {
            let (last_errors, last_warnings) = match last_counts {
                Some((last_errors, last_warnings)) => (Some(last_errors), Some(last_warnings)),
                None => (None, None),
            };
            let mut text = format!(
                "{language}: {}, {}",
                render_count(*errors, "error", "errors", last_errors),
                render_count(*warnings, "warning", "warnings", last_warnings),
            );
            if *partial {
                text.push_str(" (partial)");
            }
            text
        }
        ItemState::Checking {
            files_changed: false,
            ..
        } => format!(
            "{language}: checking{}",
            if compact { "" } else { " (first check)" }
        ),
        ItemState::Checking {
            files_changed: true,
            last,
        } => match last {
            _ if compact => format!("{language}: checking"),
            None => format!("{language}: checking (files changed)"),
            Some((errors, warnings)) => format!(
                "{language}: checking (files changed; last result: {}, {})",
                render_count(*errors, "error", "errors", None),
                render_count(*warnings, "warning", "warnings", None),
            ),
        },
        ItemState::Unavailable(reason, detail) => match detail {
            Some(detail) if !compact => {
                format!("{language}: {} ({detail})", unavailable_text(*reason))
            }
            _ => format!("{language}: {}", unavailable_text(*reason)),
        },
    }
}

/// Formats one count with singular/plural noun and its nonzero delta suffix, e.g. `3 errors (+2)`.
fn render_count(count: u32, singular: &str, plural: &str, last: Option<u32>) -> String {
    let delta = match last {
        Some(last) if last != count => format!(" ({:+})", count as i64 - last as i64),
        _ => String::new(),
    };
    format!(
        "{} {}{}",
        count,
        plural_noun(count, singular, plural),
        delta
    )
}

/// Picks the singular or plural noun form for `count` (`0` and `2+` are plural).
fn plural_noun<'a>(count: u32, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

/// Maps an unavailable reason to its fixed feed phrase (EYES-r1 §6).
fn unavailable_text(reason: UnavailableReason) -> &'static str {
    match reason {
        UnavailableReason::Disabled => "checks disabled",
        UnavailableReason::ReadRestricted => "unavailable: read_restricted",
        UnavailableReason::OutsideRoots => "outside allowed roots",
        UnavailableReason::ToolMissing => "tool not found",
        UnavailableReason::EnvMissing => "environment not found",
        UnavailableReason::NoFiles => "no files analyzed",
        UnavailableReason::Fatal => "check failed",
        UnavailableReason::Timeout => "check timed out",
    }
}

/// Joins rendered items and wraps them in the tagged `<agent-ide>` block.
fn wrap_block(rendered: &[String]) -> String {
    format!(
        "<agent-ide>\n{}\n</agent-ide>",
        rendered.join(ITEM_SEPARATOR)
    )
}

/// Truncates `text` to at most `max_bytes` UTF-8 bytes, cutting only on a whole character.
///
/// A byte offset that would split a multi-byte character is walked back to the nearest earlier
/// boundary, so the result is always valid UTF-8 and never longer than `max_bytes` bytes.
fn truncate_to_byte_len(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{Problem, Severity};

    /// Builds a ready snapshot with fixed counts for compact arrangements.
    fn ready(language: Language, errors: u32, warnings: u32) -> ProblemSnapshot {
        ProblemSnapshot {
            language,
            state: CheckState::Ready,
            errors,
            warnings,
            problems: Vec::new(),
            truncated: false,
            input_generation: 1,
            duration_ms: 1,
            detail: None,
        }
    }

    /// Builds a partial snapshot with fixed counts.
    fn partial(language: Language, errors: u32, warnings: u32) -> ProblemSnapshot {
        ProblemSnapshot {
            state: CheckState::Partial,
            ..ready(language, errors, warnings)
        }
    }

    /// Builds an unavailable snapshot for `reason`.
    fn unavailable(language: Language, reason: UnavailableReason) -> ProblemSnapshot {
        ProblemSnapshot::unavailable(language, reason, 1)
    }

    /// Builds a checking snapshot.
    fn checking(language: Language) -> ProblemSnapshot {
        ProblemSnapshot::checking(language, 1)
    }

    /// Builds a feed key with `binding` and a fixed worktree.
    fn key(binding: &str) -> FeedKey {
        FeedKey {
            binding: binding.to_string(),
            worktree: PathBuf::from("/wt"),
        }
    }

    #[test]
    fn first_ready_snapshot_emits_without_deltas() {
        let mut state = FeedState::default();
        let block = state.next_block(&key("hook"), &[ready(Language::Rust, 3, 5)], &[]);
        assert_eq!(
            block,
            Some("<agent-ide>\nrust: 3 errors, 5 warnings\n</agent-ide>".to_string())
        );
    }

    #[test]
    fn identical_state_is_never_emitted_again() {
        let mut state = FeedState::default();
        let hook = key("hook");
        assert!(
            state
                .next_block(&hook, &[ready(Language::Rust, 1, 0)], &[])
                .is_some()
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 1, 0)], &[]),
            None
        );
    }

    #[test]
    fn changed_error_count_emits_positive_delta() {
        let mut state = FeedState::default();
        let hook = key("hook");
        state
            .next_block(&hook, &[ready(Language::Rust, 1, 0)], &[])
            .expect("first delivery emits");
        let block = state
            .next_block(&hook, &[ready(Language::Rust, 3, 0)], &[])
            .expect("changed state emits");
        assert_eq!(
            block,
            "<agent-ide>\nrust: 3 errors (+2), 0 warnings\n</agent-ide>"
        );
    }

    #[test]
    fn restored_error_count_emits_negative_delta() {
        let mut state = FeedState::default();
        let hook = key("hook");
        state
            .next_block(&hook, &[ready(Language::Rust, 1, 0)], &[])
            .expect("first delivery emits");
        state
            .next_block(&hook, &[ready(Language::Rust, 3, 0)], &[])
            .expect("changed state emits");
        let block = state
            .next_block(&hook, &[ready(Language::Rust, 1, 0)], &[])
            .expect("restored state emits");
        assert_eq!(
            block,
            "<agent-ide>\nrust: 1 error (-2), 0 warnings\n</agent-ide>"
        );
    }

    #[test]
    fn partial_flag_change_emits_without_deltas() {
        let mut state = FeedState::default();
        let hook = key("hook");
        assert!(
            state
                .next_block(&hook, &[ready(Language::Rust, 2, 1)], &[])
                .is_some()
        );
        let block = state
            .next_block(&hook, &[partial(Language::Rust, 2, 1)], &[])
            .expect("partial change emits");
        assert_eq!(
            block,
            "<agent-ide>\nrust: 2 errors, 1 warning (partial)\n</agent-ide>"
        );
        assert_eq!(
            state.next_block(&hook, &[partial(Language::Rust, 2, 1)], &[]),
            None
        );
    }

    #[test]
    fn unavailable_reasons_render_fixed_phrases() {
        let phrases = [
            (UnavailableReason::OutsideRoots, "outside allowed roots"),
            (UnavailableReason::ToolMissing, "tool not found"),
            (UnavailableReason::EnvMissing, "environment not found"),
            (UnavailableReason::NoFiles, "no files analyzed"),
            (UnavailableReason::Fatal, "check failed"),
            (UnavailableReason::Timeout, "check timed out"),
        ];
        for (reason, phrase) in phrases {
            let mut state = FeedState::default();
            let block = state
                .next_block(&key("hook"), &[unavailable(Language::Python, reason)], &[])
                .expect("unavailable state emits");
            assert_eq!(
                block,
                format!("<agent-ide>\npython: {phrase}\n</agent-ide>")
            );
        }
    }

    /// A language still on its first check renders `checking (first check)` instead of being
    /// omitted, and the plate is re-sent once its result lands.
    #[test]
    fn first_check_renders_a_checking_plate_then_the_result() {
        let mut state = FeedState::default();
        let hook = key("hook");
        let checking_both = [checking(Language::Rust), checking(Language::Python)];
        assert_eq!(
            state.next_block(&hook, &checking_both, &[]),
            Some(
                "<agent-ide>\nrust: checking (first check) | python: checking (first check)\n</agent-ide>"
                    .to_string()
            )
        );
        assert_eq!(state.next_block(&hook, &checking_both, &[]), None);
        let block = state
            .next_block(
                &hook,
                &[checking(Language::Rust), ready(Language::Python, 0, 1)],
                &[],
            )
            .expect("completed language emits");
        assert_eq!(
            block,
            "<agent-ide>\nrust: checking (first check) | python: 0 errors, 1 warning\n</agent-ide>"
        );
    }

    /// A running re-check keeps the last counts in view, the result renders its delta against them,
    /// and an unchanged result returns to the pre-check text (one plate, no delta).
    #[test]
    fn files_changed_recheck_keeps_last_counts_and_the_delta_baseline() {
        let mut state = FeedState::default();
        let hook = key("hook");
        let changed = [(Language::Rust, Recheck::FilesChanged)];
        state.next_block(&hook, &[ready(Language::Rust, 0, 0)], &[]);
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 0, 0)], &changed),
            Some(
                "<agent-ide>\nrust: checking (files changed; last result: 0 errors, 0 warnings)\n</agent-ide>"
                    .to_string()
            )
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 0, 0)], &changed),
            None
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 0, 1)], &[]),
            Some("<agent-ide>\nrust: 0 errors, 1 warning (+1)\n</agent-ide>".to_string())
        );
        // A no-op check: checking plate, then the same text again — sent once, without a delta.
        assert!(
            state
                .next_block(&hook, &[ready(Language::Rust, 0, 1)], &changed)
                .is_some()
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 0, 1)], &[]),
            Some("<agent-ide>\nrust: 0 errors, 1 warning\n</agent-ide>".to_string())
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 0, 1)], &[]),
            None
        );
    }

    /// A result predating the session (`FirstCheck`) never shows its counts.
    #[test]
    fn first_check_recheck_hides_the_previous_session_counts() {
        let mut state = FeedState::default();
        assert_eq!(
            state.next_block(
                &key("hook"),
                &[ready(Language::Rust, 65, 0)],
                &[(Language::Rust, Recheck::FirstCheck)]
            ),
            Some("<agent-ide>\nrust: checking (first check)\n</agent-ide>".to_string())
        );
    }

    /// A failed check names its reason with the first 80 bytes of a sanitized detail; other
    /// unavailable reasons stay detail-free; the framing tags cannot be forged.
    #[test]
    fn failed_check_plate_carries_a_short_sanitized_detail() {
        let detail = format!("error: <agent-ide>\nline two {}", "x".repeat(200));
        let snapshot = ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            1,
            Some(detail),
        );
        let block = FeedState::default()
            .next_block(&key("hook"), &[snapshot], &[])
            .expect("failure emits");
        let line = block.lines().nth(1).unwrap();
        assert!(line.starts_with("rust: check failed (error: ?agent-ide? line two xxx"));
        assert_eq!(line.len(), "rust: check failed ()".len() + MAX_DETAIL_BYTES);
        assert_eq!(block.lines().count(), 3, "{block}");

        let no_files = ProblemSnapshot::unavailable_with_detail(
            Language::Python,
            UnavailableReason::NoFiles,
            1,
            Some("pyright analyzed 0 files".to_string()),
        );
        assert_eq!(
            FeedState::default().next_block(&key("hook"), &[no_files], &[]),
            Some("<agent-ide>\npython: no files analyzed\n</agent-ide>".to_string())
        );
    }

    /// Two languages with the longest details or counts still fit the cap without dropping either.
    #[test]
    fn two_languages_with_details_stay_within_cap_and_keep_both() {
        let long = Some("d".repeat(500));
        let block = FeedState::default()
            .next_block(
                &key("hook"),
                &[
                    ProblemSnapshot::unavailable_with_detail(
                        Language::Rust,
                        UnavailableReason::Fatal,
                        1,
                        long.clone(),
                    ),
                    ProblemSnapshot::unavailable_with_detail(
                        Language::Python,
                        UnavailableReason::Fatal,
                        1,
                        long,
                    ),
                ],
                &[],
            )
            .expect("emits");
        assert!(block.len() <= MAX_BLOCK_BYTES, "{}", block.len());
        assert!(block.contains("rust: check failed ("), "{block}");
        assert!(block.contains("python: check failed ("), "{block}");

        let huge = u32::MAX;
        let block = FeedState::default()
            .next_block(
                &key("hook2"),
                &[
                    ready(Language::Rust, huge, huge),
                    ready(Language::Python, huge, huge),
                ],
                &[
                    (Language::Rust, Recheck::FilesChanged),
                    (Language::Python, Recheck::FilesChanged),
                ],
            )
            .expect("emits");
        assert!(block.len() <= MAX_BLOCK_BYTES, "{}", block.len());
        assert!(
            block.contains("rust: checking") && block.contains("python: checking"),
            "{block}"
        );
    }

    #[test]
    fn absent_language_renders_as_nothing() {
        let mut state = FeedState::default();
        let hook = key("hook");
        assert_eq!(
            state.next_block(
                &hook,
                &[
                    unavailable(Language::Rust, UnavailableReason::Disabled),
                    unavailable(Language::Python, UnavailableReason::Disabled)
                ],
                &[]
            ),
            None,
            "no supported language present: no block at all"
        );
        let block = state
            .next_block(
                &hook,
                &[
                    unavailable(Language::Rust, UnavailableReason::Disabled),
                    ready(Language::Python, 2, 0),
                ],
                &[],
            )
            .expect("the present language still emits");
        assert_eq!(
            block, "<agent-ide>\npython: 2 errors, 0 warnings\n</agent-ide>",
            "the absent rust language is never mentioned"
        );
    }

    #[test]
    fn items_render_in_fixed_language_order_regardless_of_input() {
        let mut state = FeedState::default();
        let block = state
            .next_block(
                &key("hook"),
                &[
                    ready(Language::Python, 1, 0),
                    unavailable(Language::Rust, UnavailableReason::ToolMissing),
                ],
                &[],
            )
            .expect("mixed states emit");
        assert_eq!(
            block,
            "<agent-ide>\nrust: tool not found | python: 1 error, 0 warnings\n</agent-ide>"
        );
    }

    #[test]
    fn keys_track_delivery_independently() {
        let mut state = FeedState::default();
        let (a, b) = (key("a"), key("b"));
        let snapshots = [ready(Language::Rust, 3, 5)];
        assert!(state.next_block(&a, &snapshots, &[]).is_some());
        assert!(state.next_block(&b, &snapshots, &[]).is_some());
        assert_eq!(state.next_block(&a, &snapshots, &[]), None);
        assert_eq!(state.next_block(&b, &snapshots, &[]), None);
        let changed = [ready(Language::Rust, 4, 5)];
        assert_eq!(
            state.next_block(&a, &changed, &[]),
            Some("<agent-ide>\nrust: 4 errors (+1), 5 warnings\n</agent-ide>".to_string())
        );
        assert_eq!(state.next_block(&b, &snapshots, &[]), None);
    }

    #[test]
    fn worst_case_block_with_max_counts_and_deltas_stays_within_cap() {
        let mut state = FeedState::default();
        let hook = key("hook");
        let zeroes = [ready(Language::Rust, 0, 0), ready(Language::Python, 0, 0)];
        assert!(state.next_block(&hook, &zeroes, &[]).is_some());
        let worst = [
            partial(Language::Rust, u32::MAX, u32::MAX),
            partial(Language::Python, u32::MAX, u32::MAX),
        ];
        let block = state
            .next_block(&hook, &worst, &[])
            .expect("changed state emits");
        assert!(block.contains("(+4294967295)"));
        assert!(block.len() <= MAX_BLOCK_BYTES);
    }

    #[test]
    fn state_evicts_least_recently_used_key_at_capacity() {
        let mut state = FeedState::default();
        for index in 0..MAX_FEED_KEYS {
            assert!(
                state
                    .next_block(
                        &key(&format!("b{index}")),
                        &[ready(Language::Rust, 1, 0)],
                        &[]
                    )
                    .is_some()
            );
        }
        let oldest = key("b0");
        assert_eq!(
            state.next_block(&oldest, &[ready(Language::Rust, 2, 0)], &[]),
            Some("<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>".to_string())
        );
        // Admitting one more key evicts b1, now the least recently used one; b0 survives.
        assert!(
            state
                .next_block(
                    &key(&format!("b{MAX_FEED_KEYS}")),
                    &[ready(Language::Rust, 1, 0)],
                    &[]
                )
                .is_some()
        );
        assert_eq!(state.delivered.len(), MAX_FEED_KEYS);
        assert!(!state.delivered.contains_key(&key("b1")));
        assert!(state.delivered.contains_key(&oldest));
        // The evicted key re-delivers like a first delivery, without deltas.
        let block = state
            .next_block(&key("b1"), &[ready(Language::Rust, 1, 0)], &[])
            .expect("evicted key re-delivers");
        assert_eq!(
            block,
            "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
        );
    }

    #[test]
    fn forget_restarts_delivery_for_the_key() {
        let mut state = FeedState::default();
        let hook = key("hook");
        assert!(
            state
                .next_block(&hook, &[ready(Language::Rust, 1, 0)], &[])
                .is_some()
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 1, 0)], &[]),
            None
        );
        state.forget(&hook);
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 1, 0)], &[]),
            Some("<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>".to_string())
        );
        assert_eq!(
            state.next_block(&hook, &[ready(Language::Rust, 1, 0)], &[]),
            None
        );
        // Forgetting an unknown key is a no-op.
        state.forget(&key("unknown"));
    }

    /// Two languages present simultaneously with distinct starting counts: only python's count
    /// changes, proving each item's delta is matched by its own language rather than by position
    /// (a `last_counts` mixup would misattribute rust's unchanged counts to python or vice versa).
    #[test]
    fn per_language_last_counts_are_not_mixed_up_across_languages() {
        let mut state = FeedState::default();
        let hook = key("hook");
        let first = [ready(Language::Rust, 1, 0), ready(Language::Python, 5, 0)];
        assert!(state.next_block(&hook, &first, &[]).is_some());
        let second = [ready(Language::Rust, 1, 0), ready(Language::Python, 10, 0)];
        let block = state
            .next_block(&hook, &second, &[])
            .expect("changed python count emits");
        assert_eq!(
            block,
            "<agent-ide>\nrust: 1 error, 0 warnings | python: 10 errors (+5), 0 warnings\n</agent-ide>"
        );
    }

    /// The truncation helper cuts on a UTF-8 character boundary and never exceeds the requested
    /// byte budget.
    ///
    /// No reachable `next_block` input can grow a single item's rendered counts text past
    /// `MAX_BLOCK_BYTES` (see `worst_case_block_with_max_counts_and_deltas_stays_within_cap`), so
    /// the ladder's final hard-truncate step is exercised directly here instead.
    #[test]
    fn truncate_to_byte_len_cuts_only_on_a_char_boundary() {
        assert_eq!(truncate_to_byte_len("short", 10), "short");
        assert_eq!(truncate_to_byte_len("abcdef", 3), "abc");
        // Each 'é' is 2 bytes; a budget landing mid-character drops the whole character rather
        // than splitting it, so the result stays valid UTF-8.
        let truncated = truncate_to_byte_len("éé", 3);
        assert_eq!(truncated, "é");
        assert!(truncated.len() <= 3);
    }

    /// The block never contains raw path, message or code text from the underlying problems:
    /// only fixed phrases and numeric counts derived from the snapshot feed it.
    #[test]
    fn block_never_leaks_problem_path_message_or_code_text() {
        let mut state = FeedState::default();
        let problems = vec![Problem::new(
            "very/secret/path.rs".to_owned(),
            1,
            1,
            Severity::Error,
            Some("SECRET_CODE".to_owned()),
            "super secret message".to_owned(),
        )];
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, problems, 1, 5);
        let block = state
            .next_block(&key("hook"), &[snapshot], &[])
            .expect("ready snapshot emits");
        assert!(!block.contains("very/secret/path.rs"), "{block}");
        assert!(!block.contains("SECRET_CODE"), "{block}");
        assert!(!block.contains("super secret message"), "{block}");
    }

    /// Several snapshots of the same language in one call: the last one in the slice wins.
    #[test]
    fn several_snapshots_of_the_same_language_the_last_one_wins() {
        let mut state = FeedState::default();
        let snapshots = [
            ready(Language::Rust, 1, 0),
            ready(Language::Rust, 9, 9),
            unavailable(Language::Rust, UnavailableReason::ToolMissing),
        ];
        let block = state
            .next_block(&key("hook"), &snapshots, &[])
            .expect("last snapshot state emits");
        assert_eq!(block, "<agent-ide>\nrust: tool not found\n</agent-ide>");
    }

    /// A block `next_block_when` refuses (T28B) is never recorded as delivered: it stays due and
    /// the next accepted call emits it unchanged, exactly once.
    #[test]
    fn refused_blocks_stay_due_and_deliver_once_accepted() {
        let mut state = FeedState::default();
        let hook = key("hook");
        let snapshots = [ready(Language::Rust, 2, 0)];
        let block = "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>";
        assert_eq!(
            state.next_block_when(&hook, &snapshots, &[], |_| false),
            None,
            "a refused block is not delivered"
        );
        // Still due: refusing again returns the same block, not a suppression.
        assert_eq!(
            state.next_block_when(&hook, &snapshots, &[], |_| false),
            None
        );
        assert_eq!(
            state
                .next_block_when(&hook, &snapshots, &[], |_| true)
                .as_deref(),
            Some(block)
        );
        assert_eq!(state.next_block(&hook, &snapshots, &[]), None);
    }
}
