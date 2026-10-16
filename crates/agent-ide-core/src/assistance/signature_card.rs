//! The call-signature block that ends every `ide.start` reply.
//!
//! Agents that cannot see the tool schemas cheaply (the Codex code mode prints the whole tool
//! catalog, at most 14 KiB, once or more per agent) get the exact batch call shapes here instead,
//! in at most 700 bytes. The block follows the project card and the servers
//! line of the start reply (`docs/contracts/start-index-state-v1.md` §1) and shows one example per
//! batch form of `docs/contracts/batch-api-v1.md`, the pushed-results rule and the required
//! `ide.stop`. It is reply text read after a call, so it falls under the strict tier of
//! `docs/contracts/reply-hygiene-v1.md` §2.

/// Signature-block ceiling of M030/T006, within harness-reply-v1 r5's broader budget.
#[cfg(test)]
const MAX_SIGNATURE_BYTES: usize = 700;

/// The signature block text, without a leading or trailing newline.
const SIGNATURE_BLOCK: &str = r#"Batch call forms (JSON): symbol = card/direct neighbors; graph = call chains
ide.outline {"paths":["a.rs","b.rs"]} or {"glob":"src/*.rs"}
ide.symbol {"symbols":["a.rs#f","b.rs#g"]}
ide.graph {"symbols":["a.rs#f","b.rs#g"],"direction":"callers","depth":2}
ide.read {"items":[{"symbol":"a.rs#f:1-3"},{"symbol":"a.rs#@imports"}]}
Use ide.edit schema: one multi-file patch (Codex) or files (Claude).
ide.test {"targets":[{"path":"tests/f.rs"},{"command":["cargo","check"]}]}
ide.diff {"paths":["a.rs","b.rs"]}
ide.inspect {"detail_refs":["<ref1>","<ref2>"]}
Bridge mode: results arrive through Bridge; end idle turns. End the task with ide.stop."#;
/// Returns the call-signature block that follows the project card in an `ide.start` reply.
///
/// The text is constant, language-neutral and at most 700 bytes; every example is a JSON object
/// accepted by the named tool; batch forms and a depth-2 call-chain example are included.
pub(crate) fn signature_block() -> &'static str {
    SIGNATURE_BLOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block stays within its budget and names every batch form, the pushed-results rule and
    /// the required stop.
    #[test]
    fn block_is_small_and_lists_every_batch_form() {
        let block = signature_block();
        assert!(block.len() <= MAX_SIGNATURE_BYTES, "{} bytes", block.len());
        for form in [
            "ide.read {\"items\"",
            "ide.outline {\"paths\"",
            "{\"glob\"",
            "ide.symbol {\"symbols\"",
            "ide.graph {\"symbols\"",
            "ide.test {\"targets\"",
            "ide.edit schema",
            "#@imports",
        ] {
            assert!(block.contains(form), "{form}");
        }
        assert!(!block.contains("wait_ms"), "nothing teaches waiting");
        assert!(block.contains("card/direct neighbors; graph = call chains"));
        assert!(block.contains("\"direction\":\"callers\",\"depth\":2"));
        assert!(block.contains("results arrive through Bridge"));
        assert!(block.contains("End the task with ide.stop."));
        assert!(!block.starts_with('\n') && !block.ends_with('\n'));
    }

    /// Every example in the block is a JSON object that the facade accepts as the argument of
    /// the tool named on its line: the same validation a live call goes through, over the schemas
    /// that `schemas/tools.json` snapshots.
    #[test]
    fn every_example_validates_against_the_tool_schemas() {
        let schemas = crate::assistance::facade::tool_schemas();
        let mut examples = 0;
        for line in signature_block()
            .lines()
            .filter(|line| line.starts_with("ide."))
        {
            let (name, rest) = line.split_once(' ').unwrap();
            let schema = schemas
                .iter()
                .find(|schema| schema.name == name)
                .unwrap_or_else(|| panic!("unknown tool {name}"));
            for example in rest.split(" or ") {
                let value: serde_json::Value = serde_json::from_str(example)
                    .unwrap_or_else(|error| panic!("{example}: {error}"));
                assert!(value.is_object(), "{example}");
                assert!(
                    crate::assistance::facade::validate_call(schema.tool, value).is_ok(),
                    "{name} {example}"
                );
                examples += 1;
            }
        }
        assert_eq!(examples, 8);
    }
}
