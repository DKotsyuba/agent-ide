//! Line, indentation and project-fact helpers shared by language support modules.
//!
//! Everything here is a pure function of its inputs except `read_text` and `entry_names`,
//! which read the filesystem and treat any failure as empty.

use std::{fs, path::Path};

use super::{LanguageProject, LineRange, SyntaxVerdict, TestId};

/// Most individually named tests one command addresses before it falls back to their files.
pub const MAX_NAMED_TESTS: usize = 12;

/// Character ceiling for the signature of a class attribute or interface field.
pub const MAX_ATTRIBUTE_CHARS: usize = 60;

/// Directories [`has_files_with`] visits at most.
const PRESENCE_MAX_DIRECTORIES: usize = 64;
/// Directory levels below the root [`has_files_with`] enters.
const PRESENCE_MAX_DEPTH: usize = 3;

/// Whether a file with one of `extensions` lies within a bounded breadth-first walk of `root`:
/// three levels, 64 directories, skipping hidden directories and
/// [`SKIPPED_DIRECTORIES`](crate::intelligence::names::SKIPPED_DIRECTORIES). The presence rule of
/// languages without a manifest.
pub fn has_files_with(root: &Path, extensions: &[&str]) -> bool {
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = 0;
    while let Some((directory, depth)) = queue.pop_front() {
        visited += 1;
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extensions.contains(&extension))
            {
                return true;
            }
            if kind.is_dir()
                && depth < PRESENCE_MAX_DEPTH
                && !name.starts_with('.')
                && !crate::intelligence::names::SKIPPED_DIRECTORIES.contains(&name.as_ref())
            {
                queue.push_back((path, depth + 1));
            }
        }
        if visited >= PRESENCE_MAX_DIRECTORIES {
            break;
        }
    }
    false
}

/// Tests in first-seen order without duplicates.
pub fn distinct(tests: &[TestId]) -> Vec<TestId> {
    let mut unique: Vec<TestId> = Vec::new();
    for test in tests {
        if !unique.contains(test) {
            unique.push(test.clone());
        }
    }
    unique
}

/// Distinct files of `tests`, first-seen order, as display strings.
pub fn distinct_files(tests: &[TestId]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for test in tests {
        let file = test.file.display().to_string();
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

/// Lines of `source` without terminators; never empty so line lookups stay in bounds.
pub fn source_lines(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

/// The 1-based line `number`, clamped into the file.
pub fn line_at<'a>(lines: &[&'a str], number: u32) -> &'a str {
    lines[(number as usize).clamp(1, lines.len()) - 1]
}

/// Last non-blank 1-based line inside `range` (clamped to the file; `range.start` when all
/// lines are blank).
pub fn last_content_line(lines: &[&str], range: LineRange) -> u32 {
    let end = (range.end as usize).min(lines.len()) as u32;
    (range.start..=end)
        .rev()
        .find(|&number| !line_at(lines, number).trim().is_empty())
        .unwrap_or(range.start)
}

/// Reads `root/name` as text; a missing or unreadable file reads as empty.
pub fn read_text(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(name)).unwrap_or_default()
}

/// Sorted names of the entries directly under `dir`; empty when it cannot be listed.
pub fn entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Value of the first `name` fact in the project's environment.
pub fn env_value<'a>(project: &'a LanguageProject, name: &str) -> Option<&'a str> {
    project
        .environment
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Leading whitespace of `line`.
pub fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The file's indentation unit: the indentation step after the first line ending in `opener`
/// (the character that opens a block, such as `:` or `{`) — a tab when that step is
/// tab-indented — or `default` spaces when no such step exists.
pub fn indent_unit(lines: &[&str], opener: char, default: usize) -> String {
    for (index, line) in lines.iter().enumerate() {
        if !line.trim_end().ends_with(opener) {
            continue;
        }
        let Some(next) = lines[index + 1..]
            .iter()
            .find(|line| !line.trim().is_empty())
        else {
            break;
        };
        let (outer, inner) = (indent_of(line), indent_of(next));
        if inner.len() > outer.len() && inner.starts_with(outer) {
            let step = &inner[outer.len()..];
            return if step.starts_with('\t') {
                "\t".to_owned()
            } else {
                step.to_owned()
            };
        }
    }
    " ".repeat(default)
}

/// Collapses a multi-line declaration into one line: whitespace runs become one space, and the
/// spaces and trailing commas that line breaks leave inside brackets are dropped.
pub fn one_line(text: &str) -> String {
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for (from, to) in [
        ("( ", "("),
        ("[ ", "["),
        (" )", ")"),
        (" ]", "]"),
        (",)", ")"),
        (",]", "]"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// How many lines each side of a bounded alignment window may hold; a larger window (or file)
/// skips the map, so the caller keeps its coarse movement note instead of paying an unbounded
/// longest-common-subsequence.
const ALIGN_WINDOW_LINES: usize = 2_000;
/// Files longer than this skip the line map entirely (the window bounds already refused to hold
/// them); kept as its own ceiling so the skip is explicit.
const ALIGN_MAX_LINES: usize = 5_000;

/// Where each line of a text landed after a formatter ran: pure line alignment of `before` onto
/// `after`, built by [`align_lines`]. An identity map when the texts are equal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineAlign {
    /// Leading lines present in both texts, 1-based inclusive (`0` when none).
    prefix: usize,
    /// Trailing lines present in both texts, counted from each text's end (`0` when none).
    suffix: usize,
    /// Total lines of the before text.
    before: usize,
    /// Total lines of the after text.
    after: usize,
    /// Longest-common-subsequence anchors inside the differing window, as `(before, after)`
    /// 0-based indices relative to the window's start, in order.
    anchors: Vec<(usize, usize)>,
}

impl LineAlign {
    /// The 1-based after-line the before-line `line` maps to: unchanged inside the shared prefix
    /// and suffix, an anchor's exact line at each matched line, and a linear interpolation between
    /// neighbouring anchors (the window's far edge included) inside the differing window. Clamped
    /// to the after text.
    pub fn map_line(&self, line: u32) -> u32 {
        let line = (line as usize).clamp(1, self.before.max(1));
        if line <= self.prefix {
            return line as u32;
        }
        if self.suffix > 0 && line > self.before - self.suffix {
            let from_end = self.before - line; // 0-based distance from the last before-line
            return (self.after - from_end) as u32;
        }
        // Inside the window: interpolate between the enclosing anchor points. The far edge of the
        // window acts as the last anchor, so an unmatched trailing run still maps monotonically.
        let offset = line - 1 - self.prefix; // 0-based within the window
        let window_before = self.before - self.prefix - self.suffix;
        let window_after = self.after - self.prefix - self.suffix;
        let anchors = self
            .anchors
            .iter()
            .copied()
            .chain([(window_before, window_after)]);
        let mut previous = (0usize, 0usize);
        for (before_index, after_index) in anchors {
            if before_index == offset {
                return (self.prefix + after_index + 1) as u32;
            }
            if before_index > offset {
                let span = before_index - previous.0;
                let moved = offset - previous.0;
                let shifted = previous.1 + moved * (after_index - previous.1) / span.max(1);
                return (self.prefix + shifted + 1) as u32;
            }
            previous = (before_index, after_index);
        }
        (self.prefix + window_after.min(offset) + 1) as u32
    }

    /// Maps one inclusive 1-based before range onto the after text, keeping its order.
    pub fn map_range(&self, range: LineRange) -> LineRange {
        let start = self.map_line(range.start);
        let end = self.map_line(range.end).max(start);
        LineRange::new(start, end)
    }
}

/// Aligns the lines of `before` onto `after`: `None` when either text is longer than
/// [`ALIGN_MAX_LINES`] or their differing window exceeds [`ALIGN_WINDOW_LINES`] per side (the
/// caller then keeps its coarse movement note). Equal texts map identically. Pure and
/// language-free; used to keep an edit reply's landing ranges exact across a formatter run.
// ponytail: bounded LCS window; files > 5,000 lines skip the map and keep the coarse note.
pub fn align_lines(before: &[&str], after: &[&str]) -> Option<LineAlign> {
    if before.len() > ALIGN_MAX_LINES || after.len() > ALIGN_MAX_LINES {
        return None;
    }
    let max_prefix = before.len().min(after.len());
    let mut prefix = 0;
    while prefix < max_prefix && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < max_prefix - prefix
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let window_before = &before[prefix..before.len() - suffix];
    let window_after = &after[prefix..after.len() - suffix];
    if window_before.len() > ALIGN_WINDOW_LINES || window_after.len() > ALIGN_WINDOW_LINES {
        return None;
    }
    // Longest-common-subsequence table over the window; both sides are bounded above, so the
    // table is at most 2,001 x 2,001 u32.
    let mut table = vec![vec![0u32; window_after.len() + 1]; window_before.len() + 1];
    for row in 1..=window_before.len() {
        for column in 1..=window_after.len() {
            table[row][column] = if window_before[row - 1] == window_after[column - 1] {
                table[row - 1][column - 1] + 1
            } else {
                table[row][column - 1].max(table[row - 1][column])
            };
        }
    }
    // Walk the table backwards to recover the matched anchors in forward order.
    let mut anchors = Vec::new();
    let (mut row, mut column) = (window_before.len(), window_after.len());
    while row > 0 && column > 0 {
        if window_before[row - 1] == window_after[column - 1] {
            anchors.push((row - 1, column - 1));
            row -= 1;
            column -= 1;
        } else if table[row][column - 1] >= table[row - 1][column] {
            column -= 1;
        } else {
            row -= 1;
        }
    }
    anchors.reverse();
    Some(LineAlign {
        prefix,
        suffix,
        before: before.len(),
        after: after.len(),
        anchors,
    })
}

/// Maps one probe run's outcome onto [`SyntaxVerdict`]: a zero exit is clean; a nonzero exit whose
/// first output line is `<line>: <message>` is that failure; anything else (no output, garbage, a
/// missing line number) means no checker was proven, so the edit proceeds as `Unchecked`.
impl SyntaxVerdict {
    pub fn from_probe(exit_ok: bool, first_output_line: &str) -> Self {
        if exit_ok {
            return Self::Clean;
        }
        match first_output_line.split_once(':') {
            Some((line, message)) => match (line.trim().parse::<u32>(), message.trim()) {
                (Ok(line), message) if !message.is_empty() => Self::Failed {
                    line,
                    message: message.chars().take(200).collect(),
                },
                _ => Self::Unchecked,
            },
            None => Self::Unchecked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The presence walk finds a nested file but never one inside a skipped or hidden directory.
    #[test]
    fn presence_walk_skips_dependency_and_hidden_directories() {
        let root = std::env::temp_dir().join(format!("agent-ide-presence-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for skipped in ["node_modules/pkg", ".cache"] {
            fs::create_dir_all(root.join(skipped)).unwrap();
            fs::write(root.join(skipped).join("a.x"), "").unwrap();
        }
        assert!(!has_files_with(&root, &["x"]));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.x"), "").unwrap();
        assert!(has_files_with(&root, &["y", "x"]));
        assert!(!has_files_with(&root, &["y"]));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Equal texts map identically; a pure insertion shifts exactly the lines after it.
    #[test]
    fn align_lines_maps_identity_and_insertion_shift() {
        let before = ["a", "b", "c"];
        let identity = align_lines(&before, &before).unwrap();
        assert_eq!(
            identity.map_range(LineRange::new(2, 3)),
            LineRange::new(2, 3)
        );
        let after = ["a", "b", "x1", "x2", "c"];
        let shifted = align_lines(&before, &after).unwrap();
        assert_eq!(
            shifted.map_range(LineRange::new(1, 2)),
            LineRange::new(1, 2)
        );
        assert_eq!(
            shifted.map_line(3),
            5,
            "the last before-line lands after the insert"
        );
        // A deletion pulls the remaining lines up.
        let deleted = align_lines(&after, &before).unwrap();
        assert_eq!(deleted.map_line(5), 3);
        assert_eq!(
            deleted.map_range(LineRange::new(3, 4)),
            LineRange::new(3, 3)
        );
    }

    /// Files past the alignment ceilings answer `None` so callers keep their coarse note.
    #[test]
    fn align_lines_refuses_oversized_texts() {
        let long: Vec<&str> = vec!["x"; ALIGN_MAX_LINES + 1];
        assert!(align_lines(&long, &long).is_none());
        let window: Vec<&str> = vec!["a"; ALIGN_WINDOW_LINES + 1];
        let changed: Vec<&str> = window
            .iter()
            .enumerate()
            .map(|(i, _)| if i % 2 == 0 { "b" } else { "a" })
            .collect();
        assert!(align_lines(&window, &changed).is_none());
    }

    /// A probe exit maps onto the verdict vocabulary; garbage stays `Unchecked`.
    #[test]
    fn syntax_verdict_from_probe_exit_output_and_garbage() {
        assert_eq!(SyntaxVerdict::from_probe(true, ""), SyntaxVerdict::Clean);
        assert_eq!(
            SyntaxVerdict::from_probe(false, "12: expected `}`"),
            SyntaxVerdict::Failed {
                line: 12,
                message: "expected `}`".to_owned()
            }
        );
        assert_eq!(
            SyntaxVerdict::from_probe(false, "node: cannot find module"),
            SyntaxVerdict::Unchecked
        );
        assert_eq!(
            SyntaxVerdict::from_probe(false, ""),
            SyntaxVerdict::Unchecked
        );
        assert_eq!(
            SyntaxVerdict::from_probe(false, ": no line"),
            SyntaxVerdict::Unchecked
        );
    }
}
