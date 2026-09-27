//! Contract checks for `agent-ide init`: the fresh-home template passes `launcher check`,
//! existing configurations are never touched, and a symlinked config path is refused.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_ide::userhome::HOME_OVERRIDE_ENV;

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// A short unique scratch tree below `/private/tmp`.
fn scratch(name: &str) -> PathBuf {
    let path = PathBuf::from(format!(
        "/private/tmp/aide-init-{}-{}-{name}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn agent_ide() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-ide"))
}

/// Runs `init` with `AGENT_IDE_HOME` pointed at `effective` and returns its full result.
fn init(effective: &Path, args: &[&str]) -> std::process::Output {
    agent_ide()
        .arg("init")
        .args(args)
        .env(HOME_OVERRIDE_ENV, effective)
        .output()
        .unwrap()
}

/// Asserts the command succeeded and returns its stdout.
fn expect_success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "expected success, got {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn fresh_home_writes_a_template_that_passes_launcher_check() {
    let scratch = scratch("fresh");
    let effective = scratch.join("home");
    let output = init(
        &effective,
        &[
            "--home",
            &effective.join(".agent-ide").to_string_lossy(),
            "--allowed-root",
            "/private/tmp",
        ],
    );
    let stdout = expect_success(output);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let home = effective.join(".agent-ide");
    let config = effective.join(".config/agent-ide/launcher.json");
    assert_eq!(report["home"], home.to_string_lossy().as_ref());
    assert_eq!(report["config"], config.to_string_lossy().as_ref());
    let created = report["created"].as_array().unwrap();
    assert!(created.contains(&serde_json::json!(home.to_string_lossy().as_ref())));
    assert!(created.contains(&serde_json::json!(config.to_string_lossy().as_ref())));

    let home_mode = fs::metadata(&home).unwrap().permissions().mode();
    assert_eq!(home_mode & 0o777, 0o700, "home must be 0700");
    let config_mode = fs::metadata(&config).unwrap().permissions().mode();
    assert_eq!(config_mode & 0o777, 0o600, "config must be 0600");

    // The template is the documented minimal shape rebindable to a real candidate.
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(written["version"], 1);
    assert_eq!(written["limits"]["operation_ms"], 120_000);
    assert_eq!(
        written["allowed_roots"],
        serde_json::json!(["/private/tmp"])
    );
    assert_eq!(
        written["targets"][0]["attachment"],
        "opaque-launcher-channel"
    );
    assert_eq!(written["targets"][0]["candidate"], "/private/tmp");

    let check = agent_ide()
        .args(["launcher", "check"])
        .arg(&config)
        .env(HOME_OVERRIDE_ENV, &effective)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "launcher check rejected the fresh template: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn existing_config_is_never_modified() {
    let scratch = scratch("existing");
    let effective = scratch.join("home");
    let config_dir = effective.join(".config/agent-ide");
    fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("launcher.json");
    let existing = format!(
        r#"{{"version":1,"limits":{{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096}},"targets":[],"allowed_roots":["{}"]}}"#,
        scratch.display()
    );
    fs::write(&config, &existing).unwrap();

    let stdout = expect_success(init(&effective, &[]));
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), existing);
    let created = report["created"].as_array().unwrap();
    assert!(created.contains(&serde_json::json!(
        effective.join(".agent-ide").to_string_lossy().as_ref()
    )));
    assert!(
        !created
            .iter()
            .any(|path| path == &serde_json::json!(config.to_string_lossy().as_ref()))
    );

    let check = agent_ide()
        .args(["launcher", "check"])
        .arg(&config)
        .env(HOME_OVERRIDE_ENV, &effective)
        .output()
        .unwrap();
    assert!(check.status.success());
    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn symlinked_config_path_is_refused() {
    let scratch = scratch("symlink");
    let effective = scratch.join("home");
    let config_dir = effective.join(".config/agent-ide");
    fs::create_dir_all(&config_dir).unwrap();
    let real = scratch.join("real-config.json");
    fs::write(&real, "{}").unwrap();
    let config = config_dir.join("launcher.json");
    std::os::unix::fs::symlink(&real, &config).unwrap();

    let output = init(&effective, &[]);
    assert_eq!(output.status.code(), Some(2), "refusal must exit 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "exactly one reason line: {stderr}"
    );
    assert!(
        stderr.contains("symlink"),
        "reason names the refusal: {stderr}"
    );
    // The linked file is untouched and the link itself is preserved.
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    assert!(
        fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let _ = fs::remove_dir_all(&scratch);
}
