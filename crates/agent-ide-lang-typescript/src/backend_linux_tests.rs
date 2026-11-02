//! Linux operator declaration and bundle remeasurement regression tests.

use super::*;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

/// Constructs a closed declaration using release-pinned JS identities and an explicit Node hash.
fn declaration() -> Value {
    json!({
        "executable":{"path":"/fixture/bridge.mjs","identity":TYPESCRIPT_BRIDGE_VERSION_V1,"blake3":TYPESCRIPT_BRIDGE_BLAKE3_V1},
        "settings":"typescript_defaults_v1", "toolchain":TYPESCRIPT_NODE_VERSION_V1,
        "node":{"path":"/fixture/node","identity":TYPESCRIPT_NODE_VERSION_V1,"blake3":"11".repeat(32)},
        "typescript":{
            "bridge_bytes":TYPESCRIPT_BRIDGE_BYTES_V1, "bridge_version":TYPESCRIPT_BRIDGE_VERSION_V1,
            "tsserver":{"path":"/fixture/tsserver.js","blake3":TYPESCRIPT_TSSERVER_BLAKE3_V1,"bytes":TYPESCRIPT_TSSERVER_BYTES_V1},
            "typescript_version":TYPESCRIPT_VERSION_V1,
            "closure": TYPESCRIPT_CLOSURE_V1.iter().enumerate().map(|(i,(name,hash,bytes))| json!({"path":format!("/fixture/{i}/{name}"),"blake3":hash,"bytes":bytes})).collect::<Vec<_>>()
        },
        "trust":"accepted-local", "cache_namespace":"typescript-cache"
    })
}

/// Parses using the real registered language provider decoder.
fn provider(value: &Value) -> ProviderLaunch {
    agent_ide_core::lang::install(&[crate::LANGUAGE]);
    serde_json::from_value(value.clone()).unwrap()
}

/// The explicit fingerprint binds the whole declaration and is not a macOS host record.
#[test]
fn operator_binding_rejects_node_and_closure_changes() {
    let mut value = declaration();
    let fingerprint = provider(&value)
        .expected_typescript_linux_operator_bundle()
        .unwrap();
    assert!(!TypeScriptServer.validate_launch(&provider(&value)));
    value["typescript"]["linux_operator_bundle"] = json!(fingerprint);
    assert!(TypeScriptServer.validate_launch(&provider(&value)));
    assert!(!provider(&value).typescript_codex_accepted());
    let mut changed_node = value.clone();
    changed_node["node"]["blake3"] = json!("22".repeat(32));
    assert!(!TypeScriptServer.validate_launch(&provider(&changed_node)));
    let mut changed_closure = value.clone();
    changed_closure["typescript"]["closure"][0]["blake3"] = json!("33".repeat(32));
    assert!(!TypeScriptServer.validate_launch(&provider(&changed_closure)));
    let mut fake_macos = value.clone();
    fake_macos["typescript"]["codex_macos_evidence"] = json!(TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1);
    assert!(!TypeScriptServer.validate_launch(&provider(&fake_macos)));
    // A syntactically accepted declaration never grants a missing bundle execution authority.
    assert!(
        TypeScriptServer
            .verify_launch(&provider(&value), &AtomicBool::new(false))
            .is_err()
    );
}

/// Path-sorted npm layouts may put either package.json before the TypeScript libraries, but each
/// pinned tuple must occur exactly once, even when the two package files share a basename.
#[test]
fn operator_binding_accepts_reordered_paths_and_rejects_duplicate_tuple() {
    let mut value = declaration();
    let original = value["typescript"]["closure"].as_array().unwrap().clone();
    let reordered: Vec<Value> = [3, 0, 1, 2]
        .into_iter()
        .enumerate()
        .map(|(position, index)| {
            let mut entry = original[index].clone();
            let name = Path::new(entry["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            entry["path"] = json!(format!("/fixture/{position}/{name}"));
            entry
        })
        .collect();
    value["typescript"]["closure"] = json!(reordered);
    let fingerprint = provider(&value)
        .expected_typescript_linux_operator_bundle()
        .unwrap();
    value["typescript"]["linux_operator_bundle"] = json!(fingerprint);
    assert!(TypeScriptServer.validate_launch(&provider(&value)));

    let mut duplicate = value.clone();
    duplicate["typescript"]["closure"][3]["blake3"] =
        duplicate["typescript"]["closure"][0]["blake3"].clone();
    duplicate["typescript"]["closure"][3]["bytes"] =
        duplicate["typescript"]["closure"][0]["bytes"].clone();
    assert!(
        provider(&duplicate)
            .expected_typescript_linux_operator_bundle()
            .is_none()
    );
    assert!(!TypeScriptServer.validate_launch(&provider(&duplicate)));

    let mut unsorted = value;
    unsorted["typescript"]["closure"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert!(
        provider(&unsorted)
            .expected_typescript_linux_operator_bundle()
            .is_none()
    );
}

/// The measured bundle refuses a replaced Node and each changed declared runtime member.
#[test]
fn measured_bundle_refuses_replaced_node_and_runtime_bytes() {
    let root = std::env::temp_dir().join(format!("agent-ide-ts-linux-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let node = root.join("node");
    std::fs::write(&node, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = |name: &str, bytes: &[u8]| {
        let path = root.join(name);
        std::fs::write(&path, bytes).unwrap();
        TypeScriptBundleFileV1 {
            path,
            blake3: blake3::hash(bytes),
            bytes: bytes.len() as u64,
        }
    };
    let bridge = file("bridge", b"bridge");
    let tsserver = file("tsserver.js", b"server");
    let closure = file("typescript.js", b"typescript");
    let bundle = TypeScriptProviderBundleV1::new(TypeScriptProviderBundleV1Identity {
        node: node.clone(),
        node_blake3: blake3::hash(b"#!/bin/sh\nexit 0\n"),
        node_version: "24.4.0".into(),
        bridge,
        bridge_version: "6.0.0".into(),
        tsserver,
        typescript_version: "5.9.3".into(),
        closure: vec![closure.clone()],
    })
    .unwrap();
    assert!(bundle.verify().is_ok());
    std::fs::write(&node, b"#!/bin/sh\nexit 1\n").unwrap();
    assert!(bundle.verify().is_err());
    std::fs::write(&node, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::write(&closure.path, b"modified!!").unwrap();
    assert!(bundle.verify().is_err());
}
