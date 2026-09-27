//! Line, indentation and project-fact helpers shared by language support modules.
//!
//! Everything here is a pure function of its inputs except `read_text` and `entry_names`,
//! which read the filesystem and treat any failure as empty.

use std::{fs, path::Path};

use super::{LanguageProject, LineRange, TestId};

/// Most individually named tests one command addresses before it falls back to their files.
pub const MAX_NAMED_TESTS: usize = 12;

/// Character ceiling for the signature of a class attribute or interface field.
pub const MAX_ATTRIBUTE_CHARS: usize = 60;

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
