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

/// Environment variable that turns [`lease_holder_helper`] into a lease-holding child process.
const HOLDER_ENV: &str = "AGENT_IDE_RETENTION_LEASE_HOLDER";

/// Creates a fresh canonical scratch directory unique to this process and `name`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-ide-retention-contract-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
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
