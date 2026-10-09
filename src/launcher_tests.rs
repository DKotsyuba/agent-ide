//! Launcher declarations across the bundled languages: provider settings and project-check
//! sections are decoded and validated by the registered language integrations.

use crate::assistance::launcher::{
    LauncherConfig, LauncherError, ProjectChecksConfig, ProviderLaunch,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Validates trusted mappings and limits and refuses ambiguous mappings or unknown settings.
#[test]
fn launcher_mapping_is_closed_bounded_and_restart_only() {
    use crate::intelligence::typescript_backend::{
        TYPESCRIPT_BRIDGE_BLAKE3_V1, TYPESCRIPT_BRIDGE_BYTES_V1, TYPESCRIPT_CLOSURE_V1,
        TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1, TYPESCRIPT_NODE_BLAKE3_V1,
        TYPESCRIPT_TSSERVER_BLAKE3_V1, TYPESCRIPT_TSSERVER_BYTES_V1, TypeScriptLaunch,
    };
    use serde_json::json;
    crate::languages::install();
    let executable = json!({"path":"/private/tmp/accepted-program","identity":"accepted-git","blake3":"0".repeat(64)});
    let target = json!({"attachment":"private-attachment","candidate":"/private/tmp/worktree","git":executable,"codex":executable,"providers":[],"profiles":[],"allow_disabled_host":true});
    let config = json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[target.clone()]});
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    assert_eq!(
        loaded.target("private-attachment").unwrap().candidate,
        PathBuf::from("/private/tmp/worktree")
    );
    assert!(loaded.target("different").is_none());
    assert!(!format!("{loaded:?}").contains("private-attachment"));
    let template_path = std::env::temp_dir().join(format!(
        "agent-ide-bind-one-template-{}.json",
        std::process::id()
    ));
    std::fs::write(&template_path, config.to_string()).unwrap();
    let (bound, bound_bytes) = LauncherConfig::bind_one_candidate(
        &template_path,
        "fresh-managed-attachment",
        Path::new("/private/tmp/captured-worktree"),
    )
    .unwrap();
    assert_eq!(
        bound.target("fresh-managed-attachment").unwrap().candidate,
        PathBuf::from("/private/tmp/captured-worktree")
    );
    let rebound: Value = serde_json::from_slice(&bound_bytes).unwrap();
    let mut expected = config.clone();
    expected["targets"][0]["attachment"] = Value::String("fresh-managed-attachment".into());
    expected["targets"][0]["candidate"] = Value::String("/private/tmp/captured-worktree".into());
    assert_eq!(rebound, expected);
    std::fs::remove_file(template_path).unwrap();
    for changed in [
        json!({"version":2,"limits":config["limits"],"targets":[]}),
        json!({"version":1,"limits":config["limits"],"targets":[target.clone(),target.clone()]}),
        json!({"version":1,"limits":config["limits"],"targets":[],"cwd":"forbidden"}),
    ] {
        assert!(LauncherConfig::parse(changed.to_string().as_bytes()).is_err());
    }
    let mut invalid = config.clone();
    invalid["limits"]["queued"] = json!(65);
    assert!(LauncherConfig::parse(invalid.to_string().as_bytes()).is_err());
    let mut legacy = config.clone();
    legacy["targets"][0]["profiles"] = json!([{"arbitrary":true}]);
    assert!(LauncherConfig::parse(legacy.to_string().as_bytes()).is_ok());

    let provider = |settings: &str| json!({"executable":executable,"settings":settings,"toolchain":"accepted-git","node":executable,"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"pyright-cache"});
    let mut python_target = target.clone();
    python_target["providers"] = json!([
        provider("pyright_defaults_v1"),
        json!({"executable":executable,"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"go-cache"}),
        json!({"executable":executable,"settings":"rust_cache_priming_disabled_v1","toolchain":"rust-test","cargo":executable,"cargo_version":"accepted-git","rustc":executable,"rustc_version":"accepted-git","trust":"accepted-local","cache_namespace":"rust-cache"}),
        json!({"executable":{"path":"/private/tmp/bridge.mjs","identity":"6.0.0","blake3":TYPESCRIPT_BRIDGE_BLAKE3_V1},"settings":"typescript_defaults_v1","toolchain":"24.4.0","node":{"path":"/private/tmp/node","identity":"24.4.0","blake3":TYPESCRIPT_NODE_BLAKE3_V1},"typescript":{"bridge_bytes":TYPESCRIPT_BRIDGE_BYTES_V1,"bridge_version":"6.0.0","tsserver":{"path":"/private/tmp/tsserver.js","blake3":TYPESCRIPT_TSSERVER_BLAKE3_V1,"bytes":TYPESCRIPT_TSSERVER_BYTES_V1},"typescript_version":"5.9.3","closure":[{"path":"/private/tmp/a/_tsserver.js","blake3":TYPESCRIPT_CLOSURE_V1[0].1,"bytes":TYPESCRIPT_CLOSURE_V1[0].2},{"path":"/private/tmp/a/typescript.js","blake3":TYPESCRIPT_CLOSURE_V1[1].1,"bytes":TYPESCRIPT_CLOSURE_V1[1].2},{"path":"/private/tmp/b/package.json","blake3":TYPESCRIPT_CLOSURE_V1[2].1,"bytes":TYPESCRIPT_CLOSURE_V1[2].2},{"path":"/private/tmp/c/package.json","blake3":TYPESCRIPT_CLOSURE_V1[3].1,"bytes":TYPESCRIPT_CLOSURE_V1[3].2}],"codex_macos_evidence":TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1,"claude_macos_evidence":null},"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"typescript-cache"})
    ]);
    let mut python_config = json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target.clone()]});
    let unbound: ProviderLaunch =
        serde_json::from_value(python_config["targets"][0]["providers"][3].clone()).unwrap();
    python_config["targets"][0]["providers"][3]["typescript"]["codex_macos_evidence"] =
        json!(unbound.expected_typescript_codex_macos_evidence().unwrap());
    assert!(LauncherConfig::parse(python_config.to_string().as_bytes()).is_ok());
    let loaded = LauncherConfig::parse(python_config.to_string().as_bytes()).unwrap();
    // The retired `gopls_defaults` entry (second in the file) is dropped before validation.
    assert_eq!(
        loaded.target("private-attachment").unwrap().providers.len(),
        3
    );
    assert_eq!(
        loaded
            .retired_settings()
            .iter()
            .map(|retired| retired.key)
            .collect::<Vec<_>>(),
        ["gopls_defaults"]
    );
    let typescript = &loaded.target("private-attachment").unwrap().providers[2];
    assert!(typescript.typescript_codex_accepted());
    assert!(!typescript.typescript_claude_accepted());
    let accepted_claude = typescript
        .expected_typescript_claude_macos_evidence()
        .unwrap();
    python_config["targets"][0]["providers"][3]["typescript"]["claude_macos_evidence"] =
        json!(accepted_claude);
    let loaded = LauncherConfig::parse(python_config.to_string().as_bytes()).unwrap();
    assert!(loaded.target("private-attachment").unwrap().providers[2].typescript_claude_accepted());
    let mut invented_claude = python_config.clone();
    invented_claude["targets"][0]["providers"][3]["typescript"]["claude_macos_evidence"] =
        json!("invented");
    assert!(LauncherConfig::parse(invented_claude.to_string().as_bytes()).is_err());
    for field in [
        "toolchain",
        "node.identity",
        "executable.identity",
        "executable.blake3",
        "typescript.typescript_version",
        "typescript.tsserver.blake3",
        "typescript.closure.blake3",
        "typescript.closure.path",
    ] {
        let mut copied = python_config.clone();
        let provider = &mut copied["targets"][0]["providers"][3];
        match field {
            "toolchain" => provider["toolchain"] = json!("24.4.1"),
            "node.identity" => provider["node"]["identity"] = json!("24.4.1"),
            "executable.identity" => provider["executable"]["identity"] = json!("6.0.1"),
            "executable.blake3" => provider["executable"]["blake3"] = json!("e".repeat(64)),
            "typescript.typescript_version" => {
                provider["typescript"]["typescript_version"] = json!("5.9.4")
            }
            "typescript.tsserver.blake3" => {
                provider["typescript"]["tsserver"]["blake3"] = json!("f".repeat(64))
            }
            "typescript.closure.blake3" => {
                provider["typescript"]["closure"][0]["blake3"] = json!("d".repeat(64))
            }
            "typescript.closure.path" => {
                provider["typescript"]["closure"][0]["path"] =
                    json!("/private/tmp/a/a/_tsserver.js")
            }
            _ => unreachable!("closed copied-evidence mutation"),
        }
        assert!(
            LauncherConfig::parse(copied.to_string().as_bytes()).is_err(),
            "copied evidence accepted changed {field}"
        );
    }
    let mut relative_node = python_config.clone();
    relative_node["targets"][0]["providers"][0]["toolchain"] = json!("node");
    assert!(LauncherConfig::parse(relative_node.to_string().as_bytes()).is_err());
    python_target["providers"][0]["cargo"] = executable;
    assert!(LauncherConfig::parse(json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target]}).to_string().as_bytes()).is_err());
}

/// Accepts an optional TypeScript checker and rejects non-normal checker paths.
#[test]
fn project_checks_accept_optional_typescript_and_reject_non_normal_paths() {
    use crate::checks::typescript::ProjectTypeScriptChecksConfig;
    use serde_json::json;
    crate::languages::install();
    /// Parses a minimal launcher configuration carrying `checks` as its project checks.
    fn parse(checks: &Value) -> Result<LauncherConfig, LauncherError> {
        LauncherConfig::parse(
            json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[],"allowed_roots":["/private/tmp/worktree"],"project_checks":checks})
                .to_string()
                .as_bytes(),
        )
    }
    let typescript = crate::languages::TYPESCRIPT;
    let old: ProjectChecksConfig = serde_json::from_value(
        json!({"python": {"node": "/abs/node", "pyright_cli": "/abs/pyright"}}),
    )
    .unwrap();
    assert!(old.section(typescript).is_none());
    assert!(
        parse(&json!({"python": {"node": "/abs/node", "pyright_cli": "/abs/pyright"}})).is_ok()
    );
    let new: ProjectChecksConfig = serde_json::from_value(
        json!({"typescript": {"node": "/abs/node", "tsc_cli": "/abs/typescript/lib/tsc.js"}}),
    )
    .unwrap();
    assert_eq!(
        new.section(typescript)
            .and_then(|section| section.downcast_ref::<ProjectTypeScriptChecksConfig>())
            .unwrap()
            .tsc_cli(),
        Path::new("/abs/typescript/lib/tsc.js")
    );
    assert!(
        parse(
            &json!({"typescript": {"node": "/abs/node", "tsc_cli": "/abs/typescript/lib/tsc.js"}})
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<ProjectChecksConfig>(
            json!({"typescript": {"node": "/abs/../node", "tsc_cli": "/abs/tsc.js"}}),
        )
        .is_ok()
    );
    assert_eq!(
        parse(&json!({"typescript": {"node": "/abs/../node", "tsc_cli": "/abs/tsc.js"}})).err(),
        Some(LauncherError::Rejected)
    );
}

/// An old launcher file that still declares the removed Go provider keeps loading: the entry is
/// dropped before registered-provider decoding whatever else it holds, once per file, and does not
/// count against the provider ceiling. Every other entry stays closed.
#[test]
fn retired_provider_entries_are_ignored_and_every_other_entry_stays_closed() {
    use serde_json::json;
    crate::languages::install();
    let executable = json!({"path":"/private/tmp/accepted-program","identity":"accepted-git","blake3":"0".repeat(64)});
    let load = |providers: Value| {
        let target = json!({"attachment":"private-attachment","candidate":"/private/tmp/worktree","git":executable,"providers":providers});
        LauncherConfig::parse(
            json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[target]})
                .to_string()
                .as_bytes(),
        )
    };
    // A stale entry with no executable at all, and a second with null and foreign fields, still
    // load; the notice is registered once and names the removal.
    let loaded = load(json!([
        {"settings":"gopls_defaults"},
        {"executable":null,"settings":"gopls_defaults","extra":[1]}
    ]))
    .unwrap();
    assert!(
        loaded
            .target("private-attachment")
            .unwrap()
            .providers
            .is_empty()
    );
    let notices: Vec<_> = loaded.retired_settings().iter().map(|r| r.notice).collect();
    assert_eq!(
        notices,
        ["Go support was removed in 0.10.8; the gopls provider entry is ignored"]
    );
    // An unknown settings key is still invalid, as is a duplicate key (top level or nested) in a
    // non-retired entry: the entry is replayed through the exact provider decoder.
    let python = |executable: &str| {
        format!(
            r#"{{"version":1,"limits":{{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096}},"targets":[{{"attachment":"private-attachment","candidate":"/private/tmp/worktree","git":{git},"providers":[{{"executable":{executable},"settings":"pyright_defaults_v1","toolchain":"accepted-git","node":{git},"trust":"accepted-local","cache_namespace":"pyright-cache"}}]}}]}}"#,
            git = executable_json()
        )
    };
    /// The accepted-program object used for `git`, `node` and the provider executable.
    fn executable_json() -> String {
        format!(
            r#"{{"path":"/private/tmp/accepted-program","identity":"accepted-git","blake3":"{}"}}"#,
            "0".repeat(64)
        )
    }
    assert!(LauncherConfig::parse(python(&executable_json()).as_bytes()).is_ok());
    let nested = executable_json().replacen(
        r#""path":"/private/tmp/accepted-program""#,
        r#""path":"/private/tmp/a","path":"/private/tmp/accepted-program""#,
        1,
    );
    assert!(LauncherConfig::parse(python(&nested).as_bytes()).is_err());
    assert!(
        LauncherConfig::parse(
            python(&executable_json())
                .replacen(
                    r#""trust":"accepted-local""#,
                    r#""trust":"a","trust":"b""#,
                    1
                )
                .as_bytes()
        )
        .is_err()
    );
    assert!(load(json!([{"settings":"unknown_defaults","executable":executable,"toolchain":"t","trust":"t","cache_namespace":"c"}])).is_err());
}
