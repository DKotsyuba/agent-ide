//! Launcher configuration and worktree root admission contract checks for confined project checks.

use agent_ide::assistance::launcher::{LauncherConfig, RootAdmissionError, admit_worktree};
use agent_ide::execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// Creates a fresh empty scratch root under the system temporary directory.
///
/// Any leftover directory from an earlier run of the same binary is removed first, so symlink
/// and admission fixtures never observe stale content. Tests run single-threaded by contract,
/// and each test removes its own scratch root before returning.
fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agent-ide-roots-config-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Builds the disabled-host sandbox state JSON of one trusted launcher target.
fn disabled_state() -> Value {
    let state = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": "/private/tmp",
        "useLegacyLandlock": false
    })))
    .unwrap();
    serde_json::from_str(state.sandbox_state_json()).unwrap()
}

/// Builds the accepted Execution profile record JSON for one trusted disabled-host target.
fn accepted_record() -> Value {
    let state = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": "/private/tmp",
        "useLegacyLandlock": false
    })))
    .unwrap();
    let record = PersistedProfileRecord::from_execution_evidence(
        "accepted-disabled",
        1,
        D03ProfileEvidence {
            provider_binary: "accepted-git".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "direct".into(),
            d03_evidence: "accepted-d03".into(),
        },
        &state,
    )
    .unwrap();
    serde_json::from_str(&record.to_json()).unwrap()
}

/// Builds a valid v0.2 launcher configuration with exactly one trusted target and no v0.3 fields.
fn v02_config() -> Value {
    let executable = json!({
        "path": "/private/tmp/accepted-program",
        "identity": "accepted-git",
        "blake3": "0".repeat(64)
    });
    let target = json!({
        "attachment": "private-attachment",
        "candidate": "/private/tmp/worktree",
        "git": executable.clone(),
        "codex": executable,
        "providers": [],
        "profiles": [{"record": accepted_record(), "sandbox_state": disabled_state()}],
        "allow_disabled_host": true
    });
    json!({
        "version": 1,
        "limits": {"queued": 4, "details": 8, "operation_ms": 1000, "output_bytes": 4096},
        "targets": [target]
    })
}

/// A v0.2 configuration without the new fields parses unchanged and reports project checks off.
#[test]
fn config_v02_configuration_parses_without_new_fields() {
    let loaded = LauncherConfig::parse(v02_config().to_string().as_bytes()).unwrap();
    assert!(loaded.allowed_roots().is_empty());
    assert!(loaded.project_checks().is_none());
    assert_eq!(
        loaded.target("private-attachment").unwrap().candidate,
        PathBuf::from("/private/tmp/worktree")
    );
    assert!(!format!("{loaded:?}").contains("private-attachment"));
}

/// A complete v0.3 configuration parses and exposes allowed roots and typed check timings.
#[test]
fn config_full_v03_configuration_parses_and_exposes_accessors() {
    let root = scratch("full");
    let mut config = v02_config();
    config["allowed_roots"] = json!([root.to_string_lossy()]);
    config["project_checks"] = json!({
        "debounce_ms": 100,
        "idle_timeout_s": 30,
        "check_timeout_s": 900,
        "rust": {"toolchain_dir": "/private/tmp/toolchain"},
        "python": {"node": "/private/tmp/node", "pyright_cli": "/private/tmp/pyright"}
    });
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    assert_eq!(loaded.allowed_roots(), std::slice::from_ref(&root));
    let checks = loaded.project_checks().unwrap();
    assert_eq!(checks.debounce(), Duration::from_millis(100));
    assert_eq!(checks.idle_timeout(), Duration::from_secs(30));
    assert_eq!(checks.check_timeout(), Duration::from_secs(900));
    assert_eq!(
        checks.rust().unwrap().toolchain_dir(),
        std::path::Path::new("/private/tmp/toolchain")
    );
    let python = checks.python().unwrap();
    assert_eq!(python.node(), std::path::Path::new("/private/tmp/node"));
    assert_eq!(
        python.pyright_cli(),
        std::path::Path::new("/private/tmp/pyright")
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// An explicit `rust.cargo_home` parses into the accessor, and an absent one stays `None`
/// (EYES-r2 §1: optional, defaulting to `$HOME/.cargo`).
#[test]
fn config_rust_cargo_home_parses_when_present_and_stays_none_when_absent() {
    let mut config = v02_config();
    config["allowed_roots"] = json!(["/private/tmp/worktree"]);
    config["project_checks"] = json!({
        "rust": {"toolchain_dir": "/private/tmp/toolchain", "cargo_home": "/private/tmp/cargo"}
    });
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    let rust = loaded.project_checks().unwrap().rust().unwrap();
    assert_eq!(
        rust.cargo_home(),
        Some(std::path::Path::new("/private/tmp/cargo"))
    );

    config["project_checks"] = json!({"rust": {"toolchain_dir": "/private/tmp/toolchain"}});
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    let rust = loaded.project_checks().unwrap().rust().unwrap();
    assert_eq!(rust.cargo_home(), None);
}

/// Relative or `..`-escaping `rust.cargo_home` declarations are rejected at parse time.
#[test]
fn config_rejects_relative_rust_cargo_home() {
    let mut config = v02_config();
    config["allowed_roots"] = json!(["/private/tmp/worktree"]);
    for rust in [
        json!({"toolchain_dir": "/private/tmp/toolchain", "cargo_home": "relative/cargo"}),
        json!({"toolchain_dir": "/private/tmp/toolchain", "cargo_home": "/private/tmp/../cargo"}),
    ] {
        config["project_checks"] = json!({"rust": rust});
        assert!(
            LauncherConfig::parse(config.to_string().as_bytes()).is_err(),
            "rust={:?} must be rejected",
            config["project_checks"]
        );
    }
}

/// Absent project-check timing and language fields fall back to the contract defaults.
#[test]
fn config_project_check_defaults_apply_when_optional_fields_absent() {
    let mut config = v02_config();
    config["allowed_roots"] = json!(["/private/tmp/worktree"]);
    config["project_checks"] = json!({});
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    let checks = loaded.project_checks().unwrap();
    assert_eq!(checks.debounce(), Duration::from_millis(1500));
    assert_eq!(checks.idle_timeout(), Duration::from_secs(300));
    assert_eq!(checks.check_timeout(), Duration::from_secs(300));
    // A language subsection absent means that language is never checked.
    assert!(checks.rust().is_none());
    assert!(checks.python().is_none());
}

/// Each out-of-range project-check timing is rejected with the launcher configuration error.
#[test]
fn config_rejects_out_of_range_project_check_values() {
    for (field, values) in [
        ("debounce_ms", [99, 10_001]),
        ("idle_timeout_s", [29, 3601]),
        ("check_timeout_s", [9, 901]),
    ] {
        for value in values {
            let mut config = v02_config();
            config["allowed_roots"] = json!(["/private/tmp/worktree"]);
            config["project_checks"] = json!({});
            config["project_checks"][field] = json!(value);
            assert!(
                LauncherConfig::parse(config.to_string().as_bytes()).is_err(),
                "{field}={value} must be rejected"
            );
        }
    }
}

/// Relative, escaping, trailing-slash, and oversized allowed-root declarations are rejected
/// at parse time, as are relative or `..`-escaping project-check tool paths.
#[test]
fn config_rejects_malformed_allowed_roots_and_paths() {
    let mut config = v02_config();
    for roots in [
        json!(["relative/path"]),
        json!(["/private/tmp/../worktree"]),
        json!(["/private/tmp/worktree/"]),
        // The filesystem root itself carries a trailing separator and stays undeclarable.
        json!(["/"]),
        json!(
            (0..17)
                .map(|index| format!("/private/tmp/r{index}"))
                .collect::<Vec<_>>()
        ),
    ] {
        config["allowed_roots"] = roots;
        assert!(
            LauncherConfig::parse(config.to_string().as_bytes()).is_err(),
            "allowed_roots={:?} must be rejected",
            config["allowed_roots"]
        );
    }
    config["allowed_roots"] = json!(["/private/tmp/worktree"]);
    for checks in [
        json!({"rust": {"toolchain_dir": "relative/toolchain"}}),
        json!({"rust": {"toolchain_dir": "/private/tmp/../toolchain"}}),
        json!({"python": {"node": "/abs/node", "pyright_cli": "relative/cli"}}),
        json!({"python": {"node": "/private/tmp/../node", "pyright_cli": "/abs/cli"}}),
        json!({"unknown_field": 1}),
    ] {
        config["project_checks"] = checks;
        assert!(
            LauncherConfig::parse(config.to_string().as_bytes()).is_err(),
            "project_checks={:?} must be rejected",
            config["project_checks"]
        );
    }
}

/// Exactly the maximum of 16 allowed roots parses successfully; one more is rejected (covered
/// above by the 17-root case in [`config_rejects_malformed_allowed_roots_and_paths`]).
#[test]
fn config_accepts_exactly_sixteen_allowed_roots() {
    let mut config = v02_config();
    let roots: Vec<String> = (0..16)
        .map(|index| format!("/private/tmp/r{index}"))
        .collect();
    config["allowed_roots"] = json!(roots);
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    assert_eq!(loaded.allowed_roots().len(), 16);
}

/// Each project-check timing field is accepted at both ends of its contract range.
#[test]
fn config_accepts_boundary_project_check_values() {
    let mut config = v02_config();
    config["allowed_roots"] = json!(["/private/tmp/worktree"]);
    for (field, min, max) in [
        ("debounce_ms", 100u64, 10_000u64),
        ("idle_timeout_s", 30u64, 3600u64),
        ("check_timeout_s", 10u64, 900u64),
    ] {
        for value in [min, max] {
            config["project_checks"] = json!({});
            config["project_checks"][field] = json!(value);
            assert!(
                LauncherConfig::parse(config.to_string().as_bytes()).is_ok(),
                "{field}={value} must be accepted"
            );
        }
    }
}

/// Worktrees equal to or below one allowed root are admitted and returned in canonical form.
#[test]
fn config_admission_accepts_equal_and_nested_worktrees() {
    let root = scratch("admit");
    let nested = root.join("repo");
    std::fs::create_dir_all(&nested).unwrap();
    let roots = vec![root.clone()];
    assert_eq!(
        admit_worktree(&roots, &nested).unwrap(),
        std::fs::canonicalize(&nested).unwrap()
    );
    assert_eq!(
        admit_worktree(&roots, &root).unwrap(),
        std::fs::canonicalize(&root).unwrap()
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// A worktree outside every allowed root is rejected without returning a path.
#[test]
fn config_admission_rejects_outside_worktrees() {
    let root = scratch("outside-root");
    let other = scratch("outside-other");
    assert_eq!(
        admit_worktree(std::slice::from_ref(&root), &other),
        Err(RootAdmissionError::OutsideRoots)
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(other).unwrap();
}

/// A sibling path sharing only a string prefix with a root is not admitted.
#[test]
fn config_admission_rejects_prefix_trap_siblings() {
    let parent = scratch("prefix-trap");
    let root = parent.join("b");
    let sibling = parent.join("bc");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    assert_eq!(
        admit_worktree(&[root], &sibling),
        Err(RootAdmissionError::OutsideRoots)
    );
    std::fs::remove_dir_all(parent).unwrap();
}

/// A symlink inside a root whose target resolves outside every root is rejected.
#[test]
fn config_admission_rejects_symlink_escape() {
    let root = scratch("symlink-root");
    let outside = scratch("symlink-outside");
    let link = root.join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_eq!(
        admit_worktree(std::slice::from_ref(&root), &link),
        Err(RootAdmissionError::OutsideRoots)
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(outside).unwrap();
}

/// Admission fails closed with no configured roots or an unresolvable worktree.
#[test]
fn config_admission_fails_closed_without_roots_or_unresolvable_worktree() {
    let root = scratch("fail-closed");
    assert_eq!(admit_worktree(&[], &root), Err(RootAdmissionError::NoRoots));
    assert_eq!(
        admit_worktree(std::slice::from_ref(&root), &root.join("missing-worktree")),
        Err(RootAdmissionError::Unresolvable)
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// A root that fails to canonicalize is skipped, not treated as an admission failure, regardless
/// of where it sits in the list; a resolvable root elsewhere still admits the worktree.
#[test]
fn config_admission_skips_unresolvable_roots_regardless_of_order() {
    let missing = scratch("skip-missing");
    let missing_root = missing.join("missing-root");
    std::fs::remove_dir_all(&missing).unwrap();
    let valid_root = scratch("skip-valid");
    let worktree = valid_root.join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let canonical = std::fs::canonicalize(&worktree).unwrap();

    assert_eq!(
        admit_worktree(&[missing_root.clone(), valid_root.clone()], &worktree),
        Ok(canonical.clone())
    );
    assert_eq!(
        admit_worktree(&[valid_root.clone(), missing_root.clone()], &worktree),
        Ok(canonical)
    );
    std::fs::remove_dir_all(valid_root).unwrap();
}

/// When every configured root is unresolvable, a resolvable worktree elsewhere is rejected as
/// outside the (empty, once unresolvable roots are skipped) set of admitted roots, not as
/// `Unresolvable`.
#[test]
fn config_admission_rejects_outside_roots_when_only_root_is_unresolvable() {
    let root = scratch("only-root-unresolvable");
    let missing_root = root.join("missing-root");
    assert_eq!(
        admit_worktree(&[missing_root], &root),
        Err(RootAdmissionError::OutsideRoots)
    );
    std::fs::remove_dir_all(root).unwrap();
}
