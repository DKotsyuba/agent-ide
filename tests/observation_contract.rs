//! CLI observations of isolated repositories and missing/private endpoints, without daemon startup.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

#[path = "support/scratch.rs"]
mod scratch;

/// Distinguishes fixture directories within this test process under the configured task temp root.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Creates an isolated private tree below the caller-configured temp directory and owns cleanup.
fn fixture() -> scratch::Scratch {
    let root = std::env::temp_dir().join(format!(
        "o-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    scratch::Scratch::own(fs::canonicalize(root).unwrap())
}

/// Runs only this build's CLI with fixed test arguments; never calls daemon or default doctor.
fn cli(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(args)
        .output()
        .unwrap()
}

/// Records every fixture entry's mode, size, mtime, and exact file digest without following links.
/// Used before/after observation to detect created actors, leases, caches, stores, or file edits.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, u64, i64, i64, String)> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let hash = if metadata.is_file() {
            blake3::hash(&fs::read(&path).unwrap()).to_hex().to_string()
        } else {
            String::new()
        };
        result.insert(
            path.clone(),
            (
                metadata.mode(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                hash,
            ),
        );
        if metadata.is_dir() {
            result.extend(snapshot(&path));
        }
    }
    result
}

/// Actual Git common-dir lookup unifies a manually linked unborn worktree without commits,
/// registrations, daemon startup, runtime/cache/database creation, or refreshing any file state.
#[test]
fn observation_cli_preserves_repository_and_missing_runtime() {
    let root = fixture();
    let repo = root.join("repository");
    let linked = root.join("linked");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(&linked).unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let git_dir = repo.join(".git/worktrees/linked");
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
    fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    fs::write(
        git_dir.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .unwrap();
    fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    let common = fs::canonicalize(repo.join(".git")).unwrap();
    let identity = agent_ide::hook_hints::rendezvous_identity(&common);
    let runtime = agent_ide::hook_hints::effective_root()
        .unwrap()
        .join(format!("ai-r-{}", &identity[..16]));
    assert!(
        !runtime.exists(),
        "unique fixture must have no existing shared endpoint"
    );
    let before = snapshot(&root);
    let mut observations = Vec::new();
    for source in [&repo, &linked, &repo] {
        let output = cli(&[
            "observe".as_ref(),
            "--repo".as_ref(),
            source.as_os_str(),
            "--json".as_ref(),
        ]);
        assert!(output.status.success(), "{:?}", output.status);
        assert!(output.stderr.is_empty());
        assert!(output.stdout.len() <= 1024 * 1024);
        assert_eq!(
            output.stdout.iter().filter(|byte| **byte == b'\n').count(),
            1
        );
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["kind"], "ide_observation");
        assert!(envelope.get("error").is_none());
        let data = &envelope["data"];
        assert_eq!(data["product"], "agent-ide");
        assert_eq!(data["package_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(data["repository"]["identity"], identity);
        assert_eq!(data["repository"]["source_path"], source.to_str().unwrap());
        assert_eq!(data["instances"][0]["health"], "unavailable");
        assert!(data["instances"][0]["daemon_version"].is_null());
        assert_eq!(data["checks"]["availability"], "unavailable");
        assert_eq!(data["checks"]["coverage"], "unknown");
        assert_eq!(data["telemetry"]["availability"], "unavailable");
        assert_eq!(data["coverage"]["complete"], false);
        observations.push(envelope);
    }
    assert_eq!(
        observations[0]["data"]["repository"]["common_dir"],
        observations[1]["data"]["repository"]["common_dir"]
    );
    assert_eq!(before, snapshot(&root));
    assert!(!runtime.exists());
}

/// Existing daemon diagnostic text/exit semantics are unchanged, JSON is additive, and rejected
/// observation input returns one safe error branch without raw Git/OS stderr or state creation.
#[test]
fn observation_cli_refusals_and_doctor_json_preserve_legacy_behavior() {
    let root = fixture();
    let missing = root.join("missing-secret-path");
    let before = snapshot(&root);
    let legacy = cli(&[
        "doctor".as_ref(),
        "--runtime-dir".as_ref(),
        missing.as_os_str(),
    ]);
    assert_eq!(legacy.status.code(), Some(1));
    assert!(
        String::from_utf8(legacy.stdout)
            .unwrap()
            .starts_with("status=unavailable\nruntime=Missing\n")
    );
    let json = cli(&[
        "doctor".as_ref(),
        "--runtime-dir".as_ref(),
        missing.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(json.status.code(), Some(1));
    let diagnostic: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(diagnostic["kind"], "ide_daemon_diagnostic");
    assert_eq!(diagnostic["data"]["health"], "unavailable");
    assert!(diagnostic["data"]["daemon_version"].is_null());
    for args in [
        vec![
            "observe".as_ref(),
            "--repo".as_ref(),
            missing.as_os_str(),
            "--json".as_ref(),
        ],
        vec![
            "observe".as_ref(),
            "--repo".as_ref(),
            "relative".as_ref(),
            "--json".as_ref(),
        ],
        vec!["observe".as_ref(), "--bad-argument".as_ref()],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stderr.is_empty());
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(envelope.get("data").is_none());
        assert!(
            envelope["error"]["detail"]
                .as_str()
                .unwrap()
                .find("secret")
                .is_none()
        );
    }
    let denied = root.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    let denied_result = cli(&[
        "doctor".as_ref(),
        "--runtime-dir".as_ref(),
        denied.as_os_str(),
        "--json".as_ref(),
    ]);
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(denied_result.status.code(), Some(1));
    assert_eq!(fs::read_dir(&denied).unwrap().count(), 0);
    fs::remove_dir(denied).unwrap();
    assert_eq!(before, snapshot(&root));
    assert!(!missing.exists());
}

/// Ambient Git overrides must not replace the explicitly selected repository identity.
#[test]
fn observation_cli_ignores_ambient_git_repository_override() {
    let root = fixture();
    let repo = root.join("selected");
    let other = root.join("ambient");
    for path in [&repo, &other] {
        fs::create_dir(path).unwrap();
        assert!(
            Command::new("/usr/bin/git")
                .args(["init", "-q"])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }
    let before = snapshot(&root);
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["observe", "--repo"])
        .arg(&repo)
        .arg("--json")
        .env("GIT_DIR", other.join(".git"))
        .env("GIT_WORK_TREE", &other)
        .env("GIT_COMMON_DIR", other.join(".git"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["data"]["repository"]["common_dir"],
        repo.join(".git").to_str().unwrap()
    );
    assert_eq!(before, snapshot(&root));
}

/// Malformed health replies preserve proven observation classifications while both legacy doctor
/// modes retain their error/exit behavior. Real fixture bytes/times stay unchanged; no daemon,
/// Store, lease, actor, check, or provider is created, and the fake peer sees one health call per CLI.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn malformed_health_preserves_observation_facts_and_doctor_errors() {
    use std::os::unix::fs::OpenOptionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = fixture();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let common = fs::canonicalize(repo.join(".git")).unwrap();
    let identity = agent_ide::hook_hints::rendezvous_identity(&common);
    let runtime = root.join(format!("ai-r-{}", &identity[..16]));
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(runtime.join("agent-ide.lock"))
        .unwrap();
    let listener = tokio::net::UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
    let before = snapshot(&root);
    let peer = async {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read_u32().await.unwrap();
            assert!(size <= 4096);
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["method"], "health");
            let reply = serde_json::to_vec(&serde_json::json!({"version":1,
                "request_id":request["request_id"],"status":"malformed",
                "daemon_generation":"not-a-health-success"}))
            .unwrap();
            stream.write_u32(reply.len() as u32).await.unwrap();
            stream.write_all(&reply).await.unwrap();
        }
        drop(listener);
    };
    let calls = async {
        let text = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["doctor", "--runtime-dir"])
            .arg(&runtime)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        let json = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["doctor", "--runtime-dir"])
            .arg(&runtime)
            .arg("--json")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        let observed = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["observe", "--repo"])
            .arg(&repo)
            .arg("--json")
            .env("AGENT_IDE_CLAUDE_RENDEZVOUS_ROOT", &*root)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        (text, json, observed)
    };
    let ((text, json, observed), ()) =
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(calls, peer)
        })
        .await
        .expect("three bounded fixture health calls finish");
    assert_eq!(text.status.code(), Some(1));
    assert!(text.stdout.is_empty());
    assert!(
        String::from_utf8(text.stderr)
            .unwrap()
            .contains("invalid daemon response")
    );
    assert_eq!(json.status.code(), Some(2));
    let diagnostic: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(diagnostic.get("data").is_none());
    assert_eq!(diagnostic["error"]["code"], "diagnostic_unavailable");
    assert!(observed.status.success());
    assert!(observed.stderr.is_empty());
    let envelope: serde_json::Value = serde_json::from_slice(&observed.stdout).unwrap();
    let instance = &envelope["data"]["instances"][0];
    assert_eq!(instance["runtime"], "private");
    assert_eq!(instance["endpoint"], "socket");
    assert_eq!(instance["lock"], "unheld");
    assert_eq!(instance["health"], "unavailable");
    assert!(instance["daemon_version"].is_null());
    assert_eq!(before, snapshot(&root));
}
