//! Contract tests for `agent-ide self-install`: sealed-bundle verification, the immutable
//! standalone layout, both `current` swaps, launcher ownership, and refusal paths. Everything
//! runs against disposable temp dirs through the real CLI executable; the real `~/.local/bin`
//! and `~/.local/share/agent-ide` are never candidates.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_ide::selfinstall::sha256_hex;

/// The version every fixture bundle carries; the payload binary is the real test executable.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Distinguishes temp roots across scenarios inside one test-process run.
static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Returns a fresh, unique absolute temp root for one scenario; nothing is created here.
fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-self-install-contract-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Builds one sealed bundle for `version` below `<root>/<name>`: the real test executable as
/// the payload, both plugin manifests, hooks, a skill file carrying `payload`, the metadata
/// seal, `SHA256SUMS` (every regular file except itself), and `COMPLETE` written last.
fn sealed_bundle_named(root: &Path, name: &str, version: &str, payload: &str) -> PathBuf {
    let bundle = root.join(name);
    for dir in [
        ".claude-plugin",
        ".codex-plugin",
        "agents",
        "hooks",
        "skills/agent-ide",
    ] {
        fs::create_dir_all(bundle.join(dir)).unwrap();
    }
    fs::copy(env!("CARGO_BIN_EXE_agent-ide"), bundle.join("agent-ide")).unwrap();
    fs::write(
        bundle.join(".claude-plugin/plugin.json"),
        format!(r#"{{"version":"{version}"}}"#),
    )
    .unwrap();
    fs::write(
        bundle.join(".codex-plugin/plugin.json"),
        format!(r#"{{"version":"{version}"}}"#),
    )
    .unwrap();
    fs::write(bundle.join("agents/ide-reviewer.md"), "reviewer\n").unwrap();
    fs::write(bundle.join("hooks/hooks.json"), "{}").unwrap();
    fs::write(bundle.join("hooks/claude-hook.sh"), "#!/bin/sh\n").unwrap();
    fs::write(
        bundle.join("skills/agent-ide/SKILL.md"),
        format!("{payload}\n"),
    )
    .unwrap();
    fs::write(
        bundle.join("metadata.json"),
        format!(r#"{{"version":"{version}","format":1}}"#),
    )
    .unwrap();
    let mut sums = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&bundle)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    while let Some(path) = entries.pop() {
        if path.is_dir() {
            entries.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else {
            let relative = path.strip_prefix(&bundle).unwrap().to_str().unwrap();
            sums.push(format!(
                "{}  {relative}",
                sha256_hex(&fs::read(&path).unwrap())
            ));
        }
    }
    sums.push(format!("{}  COMPLETE", sha256_hex(b"complete\n")));
    let mut sums: Vec<String> = sums.into_iter().collect();
    sums.sort();
    fs::write(bundle.join("SHA256SUMS"), format!("{}\n", sums.join("\n"))).unwrap();
    fs::write(bundle.join("COMPLETE"), "complete\n").unwrap();
    bundle
}

/// Builds one sealed bundle under the default `bundle` name.
fn sealed_bundle(root: &Path, version: &str, payload: &str) -> PathBuf {
    sealed_bundle_named(root, "bundle", version, payload)
}

/// Runs `self-install` with every directory under `root` and fails with captured output when
/// the command exits non-zero.
fn install(bundle: &Path, root: &Path, version: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["self-install", "--release"])
        .arg(bundle)
        .args(["--version", version])
        .args(["--home", &root.join("home").to_string_lossy()])
        .args(["--prefix", &root.join("prefix").to_string_lossy()])
        .args(["--bin-dir", &root.join("bin").to_string_lossy()])
        .args(["--share-dir", &root.join("share").to_string_lossy()])
        .output()
        .expect("agent-ide self-install must execute")
}

/// Runs `self-install`, panicking with the captured output when it exits non-zero.
fn install_ok(bundle: &Path, root: &Path, version: &str) -> serde_json::Value {
    let output = install(bundle, root, version);
    assert!(
        output.status.success(),
        "self-install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("summary must be one JSON object")
}

/// Asserts the installed layout for `version`: the immutable release, both `current` swaps,
/// the regenerated hook, and the exact managed shim bytes.
fn assert_layout(root: &Path, version: &str, _home: &Path) {
    let prefix = root.join("prefix");
    let share = root.join("share");
    let release = prefix.join("releases").join(version);
    assert!(
        release.join("agent-ide").is_file(),
        "missing release binary"
    );
    assert!(release.join("COMPLETE").is_file(), "missing release seal");
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("releases/{version}")),
        "prefix current must select the release"
    );
    let plugin_version = share.join("plugin").join(version);
    for part in [
        ".claude-plugin",
        ".codex-plugin",
        "agents",
        "hooks",
        "skills",
    ] {
        assert!(
            plugin_version.join(part).is_dir(),
            "missing plugin part {part}"
        );
    }
    assert!(plugin_version.join("hooks/hooks.json").is_file());
    assert!(plugin_version.join("skills/agent-ide/SKILL.md").is_file());
    let hook = fs::read_to_string(plugin_version.join("hooks/claude-hook.sh")).unwrap();
    assert_eq!(
        hook,
        format!(
            "#!/bin/sh\nexec \"{}\" claude-hook\n",
            root.join("bin/agent-ide").display()
        ),
        "installed hook must exec the managed launcher"
    );
    assert_eq!(
        fs::read_link(share.join("plugin/current")).unwrap(),
        PathBuf::from(version),
        "plugin current must select the version"
    );
    let shim = fs::read_to_string(root.join("bin/agent-ide")).unwrap();
    assert_eq!(
        shim,
        format!(
            "#!/bin/sh\n# agent-ide managed launcher v1\nexec '{}/current/agent-ide' \"$@\"\n",
            prefix.display(),
        ),
        "launcher must be the exact managed shim"
    );
    let mode = fs::metadata(root.join("bin/agent-ide"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "launcher must be mode 0755");
}

/// Installs one sealed bundle into a disposable root and proves the complete layout, the JSON
/// summary, and that the written launcher actually execs the release binary.
#[test]
fn fresh_install_lays_out_the_standalone_layout_and_launches() {
    let root = unique_root("fresh");
    let bundle = sealed_bundle(&root, VERSION, "fresh");
    let summary = install_ok(&bundle, &root, VERSION);
    assert_eq!(summary["action"], "installed");
    assert_eq!(summary["version"], VERSION);
    assert_layout(&root, VERSION, &root.join("home"));
    let launched = Command::new(root.join("bin/agent-ide"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(launched.status.success(), "launcher must exec the release");
    assert_eq!(
        String::from_utf8_lossy(&launched.stdout),
        format!("agent-ide {VERSION}\n")
    );
    let _ = fs::remove_dir_all(&root);
}

/// Re-running for the already-selected version reports `refreshed`, keeps one immutable
/// release, and leaves identical launcher bytes.
#[test]
fn repeat_install_of_the_selected_version_only_refreshes_the_launcher() {
    let root = unique_root("repeat");
    let bundle = sealed_bundle(&root, VERSION, "repeat");
    install_ok(&bundle, &root, VERSION);
    let launcher_before = fs::read(root.join("bin/agent-ide")).unwrap();
    let summary = install_ok(&bundle, &root, VERSION);
    assert_eq!(summary["action"], "refreshed");
    let releases: Vec<_> = fs::read_dir(root.join("prefix/releases"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(releases.len(), 1, "repeat must not create a second release");
    assert_eq!(
        fs::read(root.join("bin/agent-ide")).unwrap(),
        launcher_before,
        "refreshed launcher must keep identical bytes"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Installing a second version swaps both `current` symlinks while both immutable releases
/// stay in place, and reinstalling the first version swaps back.
#[test]
fn a_second_version_swaps_current_and_plugin_current() {
    let root = unique_root("swap");
    let first = sealed_bundle_named(&root, "bundle-0.3.9", "0.3.9", "first");
    install_ok(&first, &root, "0.3.9");
    let second = sealed_bundle_named(&root, "bundle", VERSION, "second");
    install_ok(&second, &root, VERSION);
    assert_eq!(
        fs::read_link(root.join("prefix/current")).unwrap(),
        PathBuf::from(format!("releases/{VERSION}"))
    );
    assert_eq!(
        fs::read_link(root.join("share/plugin/current")).unwrap(),
        PathBuf::from(VERSION)
    );
    assert!(root.join("prefix/releases/0.3.9").is_dir());
    assert!(root.join("share/plugin/0.3.9").is_dir());
    install_ok(&first, &root, "0.3.9");
    assert_eq!(
        fs::read_link(root.join("prefix/current")).unwrap(),
        PathBuf::from("releases/0.3.9"),
        "reinstalling the older version must swap back"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A byte-different bundle for the already-installed version is refused and changes nothing.
#[test]
fn a_byte_different_bundle_for_the_installed_version_is_refused() {
    let root = unique_root("differ");
    install_ok(&sealed_bundle(&root, VERSION, "original"), &root, VERSION);
    let current_before = fs::read_link(root.join("prefix/current")).unwrap();
    let launcher_before = fs::read(root.join("bin/agent-ide")).unwrap();
    let other = sealed_bundle_named(&root, "bundle-other", VERSION, "tampered bytes");
    let output = install(&other, &root, VERSION);
    assert!(
        !output.status.success(),
        "a different bundle must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to overwrite a different immutable release"),
        "unexpected refusal: {stderr}"
    );
    assert_eq!(
        fs::read_link(root.join("prefix/current")).unwrap(),
        current_before
    );
    assert_eq!(
        fs::read(root.join("bin/agent-ide")).unwrap(),
        launcher_before
    );
    let _ = fs::remove_dir_all(&root);
}

/// With `--replace` (source builds), a byte-different bundle for the installed version replaces
/// the release directory whole and re-selects it.
#[test]
fn replace_reinstalls_a_byte_different_source_build_of_the_same_version() {
    let root = unique_root("replace");
    install_ok(&sealed_bundle(&root, VERSION, "original"), &root, VERSION);
    let other = sealed_bundle_named(&root, "bundle-other", VERSION, "rebuilt bytes");
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["self-install", "--replace", "--release"])
        .arg(&other)
        .args(["--version", VERSION])
        .args(["--home", &root.join("home").to_string_lossy()])
        .args(["--prefix", &root.join("prefix").to_string_lossy()])
        .args(["--bin-dir", &root.join("bin").to_string_lossy()])
        .args(["--share-dir", &root.join("share").to_string_lossy()])
        .output()
        .expect("agent-ide self-install must execute");
    assert!(
        output.status.success(),
        "replace must succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let installed = fs::read(
        root.join("prefix/releases")
            .join(VERSION)
            .join("SHA256SUMS"),
    )
    .unwrap();
    let candidate = fs::read(other.join("SHA256SUMS")).unwrap();
    assert_eq!(
        installed, candidate,
        "the replaced release must carry the new manifest"
    );
    assert_eq!(
        fs::read_to_string(
            root.join("share/plugin")
                .join(VERSION)
                .join("skills/agent-ide/SKILL.md")
        )
        .unwrap(),
        "rebuilt bytes\n",
        "the replaced release must restage its plugin"
    );
    assert!(
        !fs::read_dir(root.join("prefix/releases"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".replaced-")),
        "the retired copy must be removed"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A foreign script at the launcher path is refused and left untouched.
#[test]
fn an_unowned_launcher_is_refused() {
    let root = unique_root("unowned");
    fs::create_dir_all(root.join("bin")).unwrap();
    let launcher = root.join("bin/agent-ide");
    fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    let output = install(&sealed_bundle(&root, VERSION, "unowned"), &root, VERSION);
    assert!(
        !output.status.success(),
        "an unowned launcher must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unowned launcher"),
        "unexpected refusal: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&launcher).unwrap(),
        "#!/bin/sh\nexit 0\n",
        "the refused launcher must be untouched"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A plain Mach-O binary installed by the historical install-local.sh moves aside exactly
/// once, under its own `--version` label, and the managed shim replaces it.
#[test]
fn a_plain_binary_launcher_is_migrated_aside_once() {
    let root = unique_root("migrate");
    fs::create_dir_all(root.join("bin")).unwrap();
    let launcher = root.join("bin/agent-ide");
    fs::copy(env!("CARGO_BIN_EXE_agent-ide"), &launcher).unwrap();
    let original = fs::read(&launcher).unwrap();
    let backup = root.join(format!("bin/agent-ide.bak-agent-ide-{VERSION}"));
    install_ok(&sealed_bundle(&root, VERSION, "migrate"), &root, VERSION);
    assert_eq!(
        fs::read(&backup).unwrap(),
        original,
        "the previous binary must move aside byte-identical"
    );
    assert!(
        fs::read_to_string(&launcher)
            .unwrap()
            .starts_with("#!/bin/sh")
    );
    let _ = fs::remove_dir_all(&root);
}

/// One flipped payload byte after sealing breaks the digest and refuses the install.
#[test]
fn a_tampered_bundle_is_refused_by_hash_mismatch() {
    let root = unique_root("tamper");
    let bundle = sealed_bundle(&root, VERSION, "tamper");
    fs::write(bundle.join("skills/agent-ide/SKILL.md"), "flipped\n").unwrap();
    let output = install(&bundle, &root, VERSION);
    assert!(!output.status.success(), "a hash mismatch must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("hash mismatch"),
        "unexpected refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !root.join("prefix/current").exists(),
        "nothing may be installed"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A `COMPLETE` marker that is not exactly `complete\n` refuses the install.
#[test]
fn a_bundle_without_the_exact_complete_seal_is_refused() {
    let root = unique_root("noseal");
    let bundle = sealed_bundle(&root, VERSION, "noseal");
    fs::write(bundle.join("COMPLETE"), "complete").unwrap();
    let output = install(&bundle, &root, VERSION);
    assert!(!output.status.success(), "a broken seal must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("COMPLETE"),
        "unexpected refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

/// `..` and absolute paths inside `SHA256SUMS` refuse the install.
#[test]
fn unsafe_sha256sums_paths_are_refused() {
    for relative in ["../escape", "/etc/passwd"] {
        let root = unique_root("unsafe");
        let bundle = sealed_bundle(&root, VERSION, "unsafe");
        let sums = fs::read_to_string(bundle.join("SHA256SUMS")).unwrap();
        let line = format!("{}  {relative}", sha256_hex(b"complete\n"));
        let mut lines: Vec<&str> = sums.lines().collect();
        lines.push(&line);
        lines.sort();
        fs::write(bundle.join("SHA256SUMS"), format!("{}\n", lines.join("\n"))).unwrap();
        let output = install(&bundle, &root, VERSION);
        assert!(
            !output.status.success(),
            "{relative} in SHA256SUMS must be refused"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unsafe relative path"),
            "unexpected refusal: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// A symlink anywhere inside the bundle refuses the install.
#[test]
fn a_symlink_inside_the_bundle_is_refused() {
    let root = unique_root("symlink");
    let bundle = sealed_bundle(&root, VERSION, "symlink");
    std::os::unix::fs::symlink("agent-ide", bundle.join("link")).unwrap();
    let output = install(&bundle, &root, VERSION);
    assert!(!output.status.success(), "a bundle symlink must be refused");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("symlink"),
        "unexpected refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

/// Missing, repeated, unknown, and malformed invocations fail before touching the filesystem.
#[test]
fn bad_invocations_are_refused() {
    let cli = env!("CARGO_BIN_EXE_agent-ide");
    let root = unique_root("badcli");
    let bundle = sealed_bundle(&root, VERSION, "badcli");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["self-install"], "incomplete"),
        (vec!["self-install", "--release"], "flag without value"),
        (
            vec![
                "self-install",
                "--release",
                bundle.to_str().unwrap(),
                "--version",
                "0.4",
            ],
            "two-part version",
        ),
        (
            vec![
                "self-install",
                "--release",
                bundle.to_str().unwrap(),
                "--version",
                "a.b.c",
            ],
            "non-numeric version",
        ),
        (
            vec![
                "self-install",
                "--release",
                bundle.to_str().unwrap(),
                "--version",
                VERSION,
                "--unknown-flag",
                "x",
            ],
            "unknown flag",
        ),
        (
            vec![
                "self-install",
                "--release",
                bundle.to_str().unwrap(),
                "--version",
                VERSION,
                "--prefix",
                "/",
            ],
            "root prefix",
        ),
    ];
    for (arguments, description) in cases {
        let output = Command::new(cli).args(&arguments).output().unwrap();
        assert!(
            !output.status.success(),
            "{description}: {arguments:?} must be refused"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).is_empty(),
            "{description} must print a reason"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

/// Without explicit flags, `AGENT_IDE_HOME` relocates the whole per-user tree: the state home is
/// `<user home>/.agent-ide`, the prefix `<home>/standalone`, bin and share under `.local`.
#[test]
fn defaults_resolve_from_the_agent_ide_home_override() {
    let root = unique_root("defaults");
    let bundle = sealed_bundle(&root, VERSION, "defaults");
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args([
            "self-install",
            "--release",
            bundle.to_str().unwrap(),
            "--version",
            VERSION,
        ])
        .env("AGENT_IDE_HOME", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "defaults install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join(".agent-ide/standalone/current").exists());
    assert!(root.join(".local/bin/agent-ide").is_file());
    assert!(root.join(".local/share/agent-ide/plugin/current").exists());
    let _ = fs::remove_dir_all(&root);
}

/// Installing version B over an install of version A against an unowned launcher is refused
/// with neither `current` symlink changed and no release written: every check runs before the
/// first mutation.
#[test]
fn a_refused_second_version_install_leaves_no_mixed_state() {
    let root = unique_root("mixed");
    install_ok(
        &sealed_bundle_named(&root, "bundle-a", "0.3.9", "first"),
        &root,
        "0.3.9",
    );
    // Replace the managed shim with a foreign script the installer cannot own.
    let launcher = root.join("bin/agent-ide");
    fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    let prefix_current = fs::read_link(root.join("prefix/current")).unwrap();
    let plugin_current = fs::read_link(root.join("share/plugin/current")).unwrap();
    let output = install(
        &sealed_bundle_named(&root, "bundle-b", VERSION, "second"),
        &root,
        VERSION,
    );
    assert!(
        !output.status.success(),
        "the unowned launcher must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unowned launcher"),
        "unexpected refusal: {stderr}"
    );
    assert_eq!(
        fs::read_link(root.join("prefix/current")).unwrap(),
        prefix_current,
        "prefix current must not change on refusal"
    );
    assert_eq!(
        fs::read_link(root.join("share/plugin/current")).unwrap(),
        plugin_current,
        "plugin current must not change on refusal"
    );
    assert!(
        !root.join("prefix/releases").join(VERSION).exists(),
        "the refused version's release must not be written"
    );
    assert_eq!(
        fs::read_to_string(&launcher).unwrap(),
        "#!/bin/sh\nexit 0\n",
        "the refused launcher must be untouched"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Runs `self-install --replace` with every directory under `root`.
fn replace_install(bundle: &Path, root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["self-install", "--replace", "--release"])
        .arg(bundle)
        .args(["--version", VERSION])
        .args(["--home", &root.join("home").to_string_lossy()])
        .args(["--prefix", &root.join("prefix").to_string_lossy()])
        .args(["--bin-dir", &root.join("bin").to_string_lossy()])
        .args(["--share-dir", &root.join("share").to_string_lossy()])
        .output()
        .expect("agent-ide self-install must execute")
}

/// Kills and reaps the wrapped child on drop, so a failed assertion never orphans it.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `--replace` never replaces or deletes a release directory a live process runs from: it is
/// refused with the release byte-identical, and succeeds once the process has exited.
#[test]
fn replace_refuses_a_release_a_live_process_runs_from() {
    let root = unique_root("live-release");
    install_ok(&sealed_bundle(&root, VERSION, "original"), &root, VERSION);
    let release = root.join("prefix/releases").join(VERSION);
    // `launcher check` opens a FIFO and blocks until a writer appears: a live `agent-ide` process
    // executing from inside the installed release.
    let fifo = root.join("block.fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let child = KillOnDrop(
        Command::new(release.join("agent-ide"))
            .args(["launcher", "check"])
            .arg(&fifo)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let running = fs::canonicalize(&release).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !agent_ide::retention::process_snapshot()
        .unwrap()
        .iter()
        .any(|(pid, exe)| {
            *pid == child.0.id() as i32
                && exe.as_deref().is_some_and(|exe| exe.starts_with(&running))
        })
    {
        assert!(std::time::Instant::now() < deadline, "child never appeared");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let manifest = fs::read(release.join("SHA256SUMS")).unwrap();
    let other = sealed_bundle_named(&root, "bundle-other", VERSION, "rebuilt bytes");
    let refused = replace_install(&other, &root);
    assert!(!refused.status.success(), "replace must be refused");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("refusing to replace"), "{stderr}");
    assert!(
        stderr.contains(&format!("pid {}", child.0.id())),
        "{stderr}"
    );
    assert_eq!(fs::read(release.join("SHA256SUMS")).unwrap(), manifest);
    assert!(release.join("agent-ide").is_file());
    // Release and reap the child; the replace then goes through.
    drop(child);
    let accepted = replace_install(&other, &root);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        fs::read(release.join("SHA256SUMS")).unwrap(),
        fs::read(other.join("SHA256SUMS")).unwrap()
    );
    let _ = fs::remove_dir_all(&root);
}

/// A scratch install naming only `--home` and `--prefix` follows the scratch root for the
/// launcher and the plugin root; the user's live `~/.local/bin/agent-ide` stays byte-identical.
#[test]
fn a_scratch_install_never_rewrites_the_live_launcher() {
    let root = unique_root("scratch");
    // A stand-in user home (the test never points at the real one) holding a live launcher.
    let user = root.join("user");
    let live = user.join(".local/bin/agent-ide");
    fs::create_dir_all(live.parent().unwrap()).unwrap();
    fs::write(
        &live,
        "#!/bin/sh\n# agent-ide managed launcher v1\nexec '/live/current/agent-ide' \"$@\"\n",
    )
    .unwrap();
    let before = fs::read(&live).unwrap();
    let bundle = sealed_bundle(&root, VERSION, "scratch");
    for (flag, dir) in [("--home", "scratch-home"), ("--prefix", "scratch-prefix")] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["self-install", "--release"])
            .arg(&bundle)
            .args(["--version", VERSION])
            .args([flag, &root.join(dir).to_string_lossy()])
            .env("AGENT_IDE_HOME", &user)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(&live).unwrap(),
            before,
            "{flag} rewrote the live launcher"
        );
        assert!(root.join(dir).join("bin/agent-ide").is_file(), "{flag}");
        assert!(
            root.join(dir)
                .join("share/agent-ide/plugin/current")
                .exists(),
            "{flag}"
        );
    }
    assert!(!user.join(".local/share").exists());
    let _ = fs::remove_dir_all(&root);
}
