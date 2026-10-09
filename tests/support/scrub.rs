//! Fixture-identity scrubbing for transcripts compared across two daemons on two fixtures: the
//! tokens that differ only because the fixture repositories were created apart. Everything else
//! stays byte for byte, so a real difference in a reply still fails the comparison.
#![allow(
    dead_code,
    reason = "each test binary uses a different part of the helpers"
)]

use std::path::Path;

use serde_json::{Value, json};

/// The transcript line of one call with only the generated identities normalized: the values of
/// the request's own `source_ref`/`detail_ref` (a proof echoed from an earlier reply), the exact
/// worktree and fixture prefixes (relative suffixes stay), and, for the activation card alone,
/// the short commit hash and the activation identifier. Reply text of every other call, source
/// literals included, stays byte for byte.
pub fn line(tool: &str, arguments: &Value, reply: &Value, root: &Path, base: &Path) -> String {
    let mut arguments = arguments.clone();
    for key in ["source_ref", "detail_ref"] {
        if let Some(value) = arguments.get_mut(key) {
            *value = json!("<ref>");
        }
    }
    let mut reply = reply.clone();
    if tool == "ide.start"
        && let Some(card) = reply["text"].as_str()
    {
        let card = mask_after(card, "git ", 7, 12);
        reply["text"] = json!(mask_after(&card, "activation ", 8, 64));
    }
    super::parity::line(tool, &arguments, &reply)
        .replace(root.to_string_lossy().as_ref(), "<root>")
        .replace(base.to_string_lossy().as_ref(), "<fixture>")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(tool: &str, arguments: Value, text: &str, tag: &str) -> String {
        let (root, base) = (format!("/tmp/{tag}/repo"), format!("/tmp/{tag}"));
        line(
            tool,
            &arguments,
            &json!({"state":"ok","kind":"k","code":null,"text":text.replace("@", &root)}),
            Path::new(&root),
            Path::new(&base),
        )
    }

    /// The four generated identities normalize; relative suffixes and everything else stay.
    #[test]
    fn identities_normalize() {
        let card = |hash: &str, id: &str| format!("git {hash} @/a.css activation {id}");
        assert_eq!(
            at("ide.start", json!({}), &card("5d7fd78", "25d98761ab"), "p1"),
            at("ide.start", json!({}), &card("9f00aa1", "ffe01234ab"), "p2"),
        );
        let edit = |proof: &str, tag| {
            at(
                "ide.edit",
                json!({"source_ref":proof,"lines":"1-1"}),
                "@/a.css",
                tag,
            )
        };
        assert_eq!(edit("one", "p1"), edit("two", "p2"));
        assert!(edit("one", "p1").contains("<root>/a.css"));
    }

    /// Content, operation, path and non-volatile names still differ, and source literals that look
    /// like identities are never masked outside the activation card.
    #[test]
    fn real_differences_survive() {
        let read = |text: &str| at("ide.read", json!({"symbol":"a"}), text, "p");
        assert_ne!(read("git abcdef1"), read("git abcdef2"));
        assert_ne!(read("activation deadbeef"), read("activation cafebabe"));
        let start = |id: &str| at("ide.start", json!({"activation_id":id}), "x", "p");
        assert_ne!(start("activation deadbeef"), start("activation cafebabe"));
        assert_ne!(
            read("{\"source_ref\":\"alpha\"}"),
            read("{\"source_ref\":\"beta\"}")
        );
        assert_ne!(read("x"), read("x "));
        assert_ne!(
            at("ide.read", json!({"symbol":"a"}), "x", "p"),
            at("ide.symbol", json!({"symbol":"a"}), "x", "p")
        );
        assert_ne!(
            at("ide.read", json!({"symbol":"a","name":"n1"}), "x", "p"),
            at("ide.read", json!({"symbol":"a","name":"n2"}), "x", "p")
        );
        assert_ne!(
            at(
                "ide.edit",
                json!({"source_ref":"s","lines":"1-1"}),
                "x",
                "p"
            ),
            at(
                "ide.edit",
                json!({"source_ref":"s","lines":"1-2"}),
                "x",
                "p"
            )
        );
    }
}
