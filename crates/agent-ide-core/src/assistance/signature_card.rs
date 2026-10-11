//! The call-signature block that ends every `ide.start` reply.
//!
//! Agents that cannot see the tool schemas cheaply (the Codex code mode prints the whole tool
//! catalog, about 18 KiB, once or more per agent) get the exact batch call shapes here instead,
//! in at most 2048 bytes. The block follows the project card and the servers
//! line of the start reply (`docs/contracts/start-index-state-v1.md` §1) and shows one example per
//! batch form of `docs/contracts/batch-api-v1.md`, the pushed-results rule and the required
//! `ide.stop`. It is reply text read after a call, so it falls under the strict tier of
//! `docs/contracts/reply-hygiene-v1.md` §2.

/// Largest size of the signature block in bytes, a tenth of the catalog dump it replaces.
#[cfg(test)]
const MAX_SIGNATURE_BYTES: usize = 2048;

/// The signature block text, without a leading or trailing newline.
const SIGNATURE_BLOCK: &str = r#"call forms (JSON arguments; plan first, then read and edit in few calls):
ide.read {"items":[{"symbol":"src/a.rs#Store/get"},{"path":"src/b.rs","lines":"10-40"},{"path":"docs/api.md"}]}
ide.outline {"paths":["src/a.rs","src/b.rs"],"kinds":"fn,struct"} or {"glob":"crates/x/src/**/*.rs"}
ide.symbol {"symbols":["src/a.rs#Store/get","src/b.rs#Kind"],"usages":true,"callers":1}
ide.graph {"symbols":["src/a.rs#f","src/b.rs#g"],"direction":"callers","depth":1}
ide.test {"targets":[{"symbol":"src/a.rs#parse"},{"path":"tests/cli.rs"},{"pattern":"roles_"}]}
ide.edit {"operation_id":"add-field-1","source_ref":"<from ide.read>","files":[{"path":"src/a.rs","changes":[{"old":"x: 0,","new":"x: 0,\ny: 0,"}]},{"path":"src/new.rs","create":true,"content":"fn new() {}\n"}]}
ide.inspect {"detail_refs":["<ref>","<ref>"]} or {}
Read everything you need in one ide.read, change several files in one ide.edit (one project check). Results of long work arrive by themselves in an <agent-ide> block after your next tool call or when you end your turn; never wait for them. End the task with ide.stop."#;

/// Returns the call-signature block that follows the project card in an `ide.start` reply.
///
/// The text is constant, language-neutral and at most 2048 bytes; every example is a JSON object
/// in the argument shape of the named tool's batch form.
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
            "ide.edit {\"operation_id\"",
            "\"files\"",
            "ide.inspect {\"detail_refs\"",
        ] {
            assert!(block.contains(form), "{form}");
        }
        assert!(!block.contains("wait_ms"), "nothing teaches waiting");
        assert!(block.contains("arrive by themselves in an <agent-ide> block"));
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
        assert_eq!(examples, 9);
    }
}
