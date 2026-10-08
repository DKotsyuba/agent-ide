//! Contract checks for where the error log lands, that it survives daemon restarts, that the
//! `errors` reader finds what a daemon wrote, and that spawned processes never touch the real home.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_ide::userhome::{HOME_OVERRIDE_ENV, passwd_home};

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// A short unique scratch tree below `/private/tmp` (Unix socket paths are length limited).
fn scratch(name: &str) -> PathBuf {
    let path = PathBuf::from(format!(
        "/private/tmp/aide-elog-{}-{}-{name}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// The 16 hex repository key the reader derives from one canonical rendezvous key path.
fn key_of(rendezvous_key: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    blake3::hash(rendezvous_key.as_os_str().as_bytes())
        .to_hex()
        .to_string()[..16]
        .to_owned()
}

fn agent_ide() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-ide"))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("/usr/bin/git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// Starts a daemon on `runtime` (created here), waits for its socket, and returns the child.
fn start_daemon(runtime: &Path, configure: impl FnOnce(&mut Command)) -> std::process::Child {
    fs::create_dir_all(runtime).unwrap();
    fs::set_permissions(runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = agent_ide();
    command
        .args(["daemon", "--runtime-dir"])
        .arg(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    for _ in 0..400 {
        if std::os::unix::net::UnixStream::connect(runtime.join("agent-ide.sock")).is_ok() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    child.wait().unwrap();
    panic!("daemon did not bind its private socket");
}

/// Stops a daemon with SIGTERM (a clean shutdown that logs `stopped`) and waits for it.
fn terminate(mut child: std::process::Child) {
    // SAFETY: signalling the child this test itself spawned.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
}

fn events(home: &Path, key: &str) -> Vec<serde_json::Value> {
    fs::read_to_string(home.join(".agent-ide/logs").join(key).join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn errors(home: &Path, repo: &Path, extra: &[&str]) -> Output {
    agent_ide()
        .env(HOME_OVERRIDE_ENV, home)
        // A substituted `$HOME` (as `agent-run` hands its delegates) must not redirect the reader.
        .env("HOME", "/nonexistent-substitute-home")
        .args(["errors", "--repo"])
        .arg(repo)
        .args(extra)
        .output()
        .unwrap()
}

/// Events must survive a daemon restart (append, never truncate) and the reader must find them
/// for the repository path and for any path inside it, whatever `$HOME` says.
#[test]
fn log_survives_daemon_restart_and_reader_finds_it_from_a_subdirectory() {
    let base = scratch("restart");
    let home = base.join("home");
    let repo = fs::canonicalize(&base).unwrap().join("repo");
    fs::create_dir_all(repo.join("sub")).unwrap();
    // Not a git repository: the rendezvous key is the canonical directory itself.
    let key = key_of(&repo);
    let runtime = base.join(format!("ai-r-{key}"));
    for _ in 0..2 {
        let child = start_daemon(&runtime, |command| {
            command
                .env(HOME_OVERRIDE_ENV, &home)
                .env("HOME", "/nonexistent-substitute-home");
        });
        terminate(child);
    }
    let started = events(&home, &key)
        .iter()
        .filter(|event| event["method"] == "daemon" && event["outcome"] == "started")
        .count();
    assert_eq!(started, 2, "the second daemon must append, not truncate");
    let listing = errors(&home, &repo, &["--all"]);
    let stdout = String::from_utf8(listing.stdout).unwrap();
    assert_eq!(stdout.matches("daemon started").count(), 2, "{stdout}");
    let summary = errors(&home, &repo, &["--all", "--summary"]);
    assert!(
        String::from_utf8(summary.stdout)
            .unwrap()
            .contains("2 info daemon started -")
    );
    let _ = fs::remove_dir_all(&base);
}

/// `--repo` inside a linked worktree (or its subdirectory) finds the log the daemon keyed by the
/// repository's git common directory, and `--limit` keeps the newest events.
#[test]
fn reader_resolves_worktrees_to_the_repository_log() {
    let base = scratch("worktree");
    let home = base.join("home");
    let root = fs::canonicalize(&base).unwrap();
    let main = root.join("main");
    fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    fs::write(main.join("f"), "x").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let linked = root.join("linked");
    git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    fs::create_dir_all(linked.join("deep/er")).unwrap();
    let key = key_of(&fs::canonicalize(main.join(".git")).unwrap());
    let runtime = root.join(format!("ai-r-{key}"));
    for _ in 0..3 {
        terminate(start_daemon(&runtime, |command| {
            command.env(HOME_OVERRIDE_ENV, &home);
        }));
    }
    for repo in [&main, &linked, &linked.join("deep/er")] {
        let output = errors(&home, repo, &["--all", "--limit", "2"]);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 2, "{repo:?}: {stdout}");
    }
    let _ = fs::remove_dir_all(&base);
}

/// A managed Codex daemon runs in a random `ai-<random>` runtime directory; the spawning MCP
/// hands it the repository key so its log lands where `errors --repo` looks.
#[test]
fn log_key_environment_names_the_log_directory_of_a_random_runtime() {
    let base = scratch("codex");
    let home = base.join("home");
    let repo = fs::canonicalize(&base).unwrap().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let key = key_of(&repo);
    let runtime = base.join("ai-0123456789abcdef");
    terminate(start_daemon(&runtime, |command| {
        command
            .env(HOME_OVERRIDE_ENV, &home)
            .env("AGENT_IDE_LOG_KEY", &key);
    }));
    assert!(!events(&home, &key).is_empty());
    let output = errors(&home, &repo, &["--all"]);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("daemon started")
    );
    let _ = fs::remove_dir_all(&base);
}

/// A daemon spawned with the environment this test process was given (as every harness does) must
/// write its log below `AGENT_IDE_HOME` (set by `.cargo/config.toml`), never the real home.
#[test]
fn spawned_daemon_never_writes_under_the_real_home() {
    let base = scratch("realhome");
    let redirected = PathBuf::from(
        std::env::var_os(HOME_OVERRIDE_ENV)
            .expect("AGENT_IDE_HOME must be set for tests (see .cargo/config.toml)"),
    );
    let real = passwd_home().expect("passwd home");
    assert_ne!(real, redirected, "tests must not run against the real home");
    let key = key_of(&fs::canonicalize(&base).unwrap());
    let runtime = base.join(format!("ai-r-{key}"));
    terminate(start_daemon(&runtime, |_| {}));
    assert!(
        !real.join(".agent-ide/logs").join(&key).exists(),
        "the daemon wrote its log under the real home"
    );
    assert!(!events(&redirected, &key).is_empty());
    let _ = fs::remove_dir_all(&base);
    let _ = fs::remove_dir_all(redirected.join(".agent-ide/logs").join(&key));
}

/// `--help` lists the real subcommands and succeeds; a missing or unknown subcommand prints the
/// same listing to stderr with exit code 2; a known subcommand with bad arguments is unchanged.
#[test]
fn help_and_unknown_subcommands_print_usage() {
    for flag in ["--help", "-h", "help"] {
        let output = agent_ide().arg(flag).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{flag}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        for name in [
            "mcp",
            "daemon",
            "claude-hook",
            "claude-rendezvous",
            "errors",
            "--version",
        ] {
            assert!(
                stdout.contains(name),
                "{flag}: usage lacks {name}: {stdout}"
            );
        }
    }
    for arguments in [&[][..], &["frobnicate"][..]] {
        let output = agent_ide().args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("usage: agent-ide")
        );
        assert!(output.stdout.is_empty());
    }
    for subcommand in ["cache", "daemon", "self-install"] {
        for flag in ["--help", "-h"] {
            let output = agent_ide().args([subcommand, flag]).output().unwrap();
            assert_eq!(output.status.code(), Some(0), "{subcommand} {flag}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(
                stdout.starts_with("usage: agent-ide"),
                "{subcommand}: {stdout}"
            );
            assert!(
                stdout.contains("cache status|prune"),
                "{subcommand}: {stdout}"
            );
        }
    }
    let bad = agent_ide().arg("daemon").output().unwrap();
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        String::from_utf8(bad.stderr)
            .unwrap()
            .contains("invalid daemon response")
    );
    let version = agent_ide().arg("--version").output().unwrap();
    assert_eq!(version.status.code(), Some(0));
}
