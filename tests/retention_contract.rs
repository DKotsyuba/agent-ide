//! Cross-process cache retention contract: a worktree lease held by another process keeps a
//! sweep off that worktree's check cache (`docs/cache-retention.md`).
//!
//! Its own test binary, because spawning a child duplicates every open descriptor until `exec`,
//! which would briefly extend other tests' leases if they shared the process.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use agent_ide::retention::{Fate, Lease, MARKER_FILE_NAME, Report, sweep_with, worktree_key};
use support::Scratch;

#[path = "support/scratch.rs"]
mod support;

/// Environment variable that turns [`lease_holder_helper`] into a lease-holding child process.
const HOLDER_ENV: &str = "AGENT_IDE_RETENTION_LEASE_HOLDER";

/// Creates a fresh canonical scratch directory unique to this process and `name`.
fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "agent-ide-retention-contract-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    Scratch::own(std::fs::canonicalize(dir).unwrap())
}

/// Returns the fate the sweep gave `dir`, or `None` when the policy did not select it.
fn fate_of(report: &Report, dir: &Path) -> Option<Fate> {
    report
        .verdicts
        .iter()
        .find(|verdict| verdict.path == dir)
        .map(|verdict| verdict.fate)
}

/// Child-process half of the cross-process test: holds a lease on `<home>\n<worktree>` from the
/// environment, prints `held`, and keeps it until stdin closes. A no-op in normal runs.
#[test]
fn lease_holder_helper() {
    let Some(spec) = std::env::var_os(HOLDER_ENV) else {
        return;
    };
    let spec = spec.into_string().unwrap();
    let (home, worktree) = spec.split_once('\n').unwrap();
    let _lease = Lease::acquire(Path::new(home), Path::new(worktree)).expect("child lease");
    println!("held");
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

/// A lease held by another process blocks the claim until that process exits.
#[test]
fn a_lease_held_by_another_process_keeps_the_cache_until_it_exits() {
    let home = scratch("home");
    let worktree = scratch("worktree");
    let dir = home
        .join("checks")
        .join("0123456789abcdef")
        .join(worktree_key(&worktree));
    std::fs::create_dir_all(dir.join("digest/lang/target")).unwrap();
    std::fs::write(dir.join("digest/lang/target/artifact"), vec![7u8; 8192]).unwrap();
    std::fs::write(
        dir.join(MARKER_FILE_NAME),
        worktree.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_holder_helper", "--nocapture"])
        .env(
            HOLDER_ENV,
            format!("{}\n{}", home.display(), worktree.display()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.any(|line| line.unwrap() == "held"),
        "the child took its lease"
    );
    let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
    let nobody = || Some(Vec::new());

    let report = sweep_with(&home, true, later, &nobody);
    assert_eq!(fate_of(&report, &dir), Some(Fate::InUse));
    assert!(dir.exists());

    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let report = sweep_with(&home, true, later, &nobody);
    assert_eq!(fate_of(&report, &dir), Some(Fate::Removed));
    assert!(!dir.exists());
}

/// Copies the real product binary to a distinct development path (optionally with its lease
/// proof overwritten), runs it as a daemon and returns what `cache status` says about its pid:
/// `(listed as a lease-taking build, listed as pausing eviction)`.
fn status_view_of_a_copied_build(name: &str, strip_proof: bool) -> (bool, bool) {
    let proof: &[u8] = b"agent-ide/worktree-lease-protocol/proof-1";
    let home = scratch(name);
    let state = home.join(".agent-ide");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_agent-ide")).unwrap();
    let found = bytes
        .windows(proof.len())
        .filter(|window| *window == proof)
        .count();
    assert!(found > 0, "the product binary carries its lease proof");
    let lock_proof: &[u8] = b"agent-ide/hint-publish-lock/proof-1";
    assert!(
        bytes
            .windows(lock_proof.len())
            .any(|window| window == lock_proof),
        "the product binary carries its hint-lock proof"
    );
    if strip_proof {
        for at in (0..bytes.len() - proof.len())
            .filter(|&at| &bytes[at..at + proof.len()] == proof)
            .collect::<Vec<_>>()
        {
            bytes[at..at + proof.len()].fill(b'x');
        }
    }
    let build = home.join("gate/target/debug/agent-ide");
    std::fs::create_dir_all(build.parent().unwrap()).unwrap();
    std::fs::write(&build, bytes).unwrap();
    std::fs::set_permissions(&build, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    if strip_proof {
        // Editing a signed Mach-O invalidates its signature; the kernel would kill the copy.
        let signed = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&build)
            .output()
            .unwrap();
        assert!(signed.status.success(), "re-signing the edited copy");
    }
    // A socket path must stay short, so the runtime directory is not below the long scratch home.
    let runtime = PathBuf::from(format!("/private/tmp/ai-rc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let mut daemon = Command::new(&build)
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime)
        .env("AGENT_IDE_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = (0..200).any(|_| {
        std::thread::sleep(Duration::from_millis(25));
        std::os::unix::net::UnixStream::connect(runtime.join("agent-ide.sock")).is_ok()
    });
    let output = ready.then(|| {
        Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["cache", "status"])
            .env("AGENT_IDE_HOME", &home)
            .output()
            .unwrap()
    });
    let exited = daemon.try_wait().unwrap();
    // Cleanup comes before every assertion so a failure never leaves the daemon behind.
    let pid = daemon.id();
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&runtime);
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        ready && exited.is_none(),
        "the copied build served and stayed up ({exited:?})"
    );
    let output = output.unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let pid = format!("  pid {pid} ");
    // Section by section: the build group, then the paused group, each indented under a header.
    let mut section = "";
    let (mut build_listed, mut pausing_listed) = (false, false);
    for line in text.lines() {
        if line.contains("take leases and do not pause eviction") {
            section = "build";
        } else if line.contains("eviction of checks/telemetry paused until") {
            section = "paused";
        } else if !line.starts_with("  ") {
            section = "";
        } else if line.starts_with(&pid) {
            build_listed |= section == "build";
            pausing_listed |= section == "paused";
        }
    }
    (build_listed, pausing_listed)
}

/// The real product binary, run from a copy at a distinct development path (not this test's own
/// inode, not under `standalone/releases`), is listed by `cache status` as a lease-taking build
/// that does not pause eviction; the same copy with its lease proof erased pauses eviction.
#[test]
fn a_copied_product_build_is_proven_by_its_executable_and_an_unproven_one_pauses() {
    assert_eq!(
        status_view_of_a_copied_build("proven-build", false),
        (true, false)
    );
    assert_eq!(
        status_view_of_a_copied_build("unproven-build", true),
        (false, true)
    );
}

/// `cache status` reports the Claude hook key hints below the temporary root as a dry run: how
/// many it found and how many a prune or the hourly sweep would remove.
#[test]
fn cache_status_reports_the_hook_key_hints() {
    let home = scratch("hint-status");
    let state = home.join(".agent-ide");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["cache", "status"])
        .env("AGENT_IDE_HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let line = text
        .lines()
        .find(|line| line.starts_with("hook key hints: "))
        .unwrap_or_else(|| panic!("no hint line in {text}"));
    assert!(line.ends_with("stale and removable"), "{line}");
    let _ = std::fs::remove_dir_all(&home);
}
