//! Fixture-identity scrubbing for transcripts compared across two daemons on two fixtures: the
//! tokens that differ only because the fixture repositories were created apart. Everything else
//! stays byte for byte, so a real difference in a reply still fails the comparison.
#![allow(
    dead_code,
    reason = "each test binary uses a different part of the helpers"
)]

use std::path::Path;

/// `text` with the fixture's identity masked: its base directory, short commit hashes after
/// `git `, activation identifiers and the values of `source_ref`/`detail_ref` echoes.
pub fn scrub(text: &str, base: &Path) -> String {
    let text = text.replace(base.to_string_lossy().as_ref(), "<fixture>");
    let text = mask_after(&text, "git ", 7, 12);
    let text = mask_after(&text, "activation ", 8, 64);
    let text = mask_quoted(&text, "\"source_ref\":\"");
    mask_quoted(&text, "\"detail_ref\":\"")
}

/// Masks the hexadecimal run that directly follows each `marker` when it is `min..=max` long.
fn mask_after(text: &str, marker: &str, min: usize, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let (head, tail) = rest.split_at(at + marker.len());
        out.push_str(head);
        let run = tail
            .bytes()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        if (min..=max).contains(&run) && !tail[run..].starts_with(|c: char| c.is_alphanumeric()) {
            out.push_str("<id>");
            rest = &tail[run..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Masks the quoted value after each `opening` (`"source_ref":"` and the like).
fn mask_quoted(text: &str, opening: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(opening) {
        let (head, tail) = rest.split_at(at + opening.len());
        out.push_str(head);
        match tail.find('"') {
            Some(end) => {
                out.push_str("<ref>");
                rest = &tail[end..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only identities are masked; every other byte, number and word stays.
    #[test]
    fn scrubbing_masks_identities_and_nothing_else() {
        let text = "git 5d7fd78 at /tmp/p-1/repo existing activation 25d98761ab \
                    {\"source_ref\":\"1af9b318-8\",\"detail_ref\":\"x-2\",\"n\":7} git main 3 files";
        let scrubbed = scrub(text, Path::new("/tmp/p-1"));
        assert_eq!(
            scrubbed,
            "git <id> at <fixture>/repo existing activation <id> \
             {\"source_ref\":\"<ref>\",\"detail_ref\":\"<ref>\",\"n\":7} git main 3 files"
        );
    }
}
