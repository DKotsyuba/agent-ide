//! Contract checks for the argument-less `agent-ide doctor` installation report: a healthy fake
//! layout builder, then one test per contract point: healthy layout exits 0 with no error
//! findings, missing config is an error exiting 2, plugin mismatch warns with the restart hint,
//! and journal lines are counted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_ide::errorlog;
use agent_ide::userhome::HOME_OVERRIDE_ENV;

/// The version every healthy-layout fixture pins: the binary under test's own version.
const BINVER: &str = env!("CARGO_PKG_VERSION");

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// A short unique scratch tree below `/private/tmp`.
fn scratch(name: &str) -> PathBuf {
    let path = PathBuf::from(format!(
        "/private/tmp/aide-doctor-{}-{}-{name}",
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

/// One fake per-user home: config written, everything else added per test.
struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Creates the scratch tree and the launcher configuration inside it.
    fn new(name: &str) -> Self {
        let root = scratch(name);
        let config_dir = root.join(".config/agent-ide");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("launcher.json"),
            format!(
                r#"{{"version":1,"limits":{{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096}},"targets":[],"allowed_roots":["{}"]}}"#,
                root.display()
            ),
        )
        .unwrap();
        Self { root }
    }

    /// The launcher configuration path inside this layout.
    fn config(&self) -> PathBuf {
        self.root.join(".config/agent-ide/launcher.json")
    }

    /// Lays a complete standalone release dir plus `current` symlink.
    fn install(&self, version: &str) {
        let releases = self.root.join(".agent-ide/standalone/releases");
        fs::create_dir_all(releases.join(version)).unwrap();
        fs::write(releases.join(version).join("COMPLETE"), b"").unwrap();
        std::os::unix::fs::symlink(
            releases.join(version),
            self.root.join(".agent-ide/standalone/current"),
        )
        .unwrap();
    }

    /// Writes an executable launcher shim.
    fn shim(&self) {
        let bin = self.root.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("agent-ide"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("agent-ide"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Lays the plugin dir, manifest, and `current` symlink at `version`.
    fn plugin(&self, version: &str) {
        let plugin = self.root.join(".local/share/agent-ide/plugin");
        fs::create_dir_all(plugin.join(version).join(".claude-plugin")).unwrap();
        fs::write(
            plugin.join(version).join(".claude-plugin/plugin.json"),
            format!(r#"{{"name":"agent-ide","version":"{version}"}}"#),
        )
        .unwrap();
        std::os::unix::fs::symlink(plugin.join(version), plugin.join("current")).unwrap();
    }

    /// Writes the Codex hooks fragment, Codex MCP entry, and Claude settings reference.
    fn hosts(&self) {
        let codex = self.root.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"/bin/agent-ide codex-hook --managed"}]}]}}"#,
        )
        .unwrap();
        fs::write(
            codex.join("config.toml"),
            "[mcp_servers.agent-ide]\ncommand = \"/bin/agent-ide\"\n",
        )
        .unwrap();
        let claude = self.root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("settings.json"), r#"{"hooks":"agent-ide"}"#).unwrap();
    }

    /// Appends one journal line to one repository log at `level`.
    fn journal_line(&self, repo: &str, level: &str) {
        let dir = self.root.join(format!(".agent-ide/logs/{repo}"));
        fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let line = serde_json::json!({
            "ts": errorlog::format_rfc3339(now),
            "level": level,
            "method": "check",
            "outcome": "failed",
        });
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .unwrap();
        use std::io::Write as _;
        writeln!(file, "{line}").unwrap();
    }

    /// Runs `doctor` against this layout and returns its full result.
    fn doctor(&self) -> std::process::Output {
        agent_ide()
            .arg("doctor")
            .env(HOME_OVERRIDE_ENV, &self.root)
            .output()
            .unwrap()
    }
}

/// Parses `output`'s stdout as the doctor report JSON (after asserting its exact shape).
fn report_of(output: &std::process::Output) -> serde_json::Value {
    report_shape(output)
}

/// Asserts the exact report shape and returns the parsed JSON.
fn report_shape(output: &std::process::Output) -> serde_json::Value {
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut keys: Vec<_> = report.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["checked_at", "findings", "home", "version"],
        "exact top-level report keys"
    );
    for finding in report["findings"].as_array().unwrap() {
        let mut keys: Vec<_> = finding.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["code", "component", "detail", "severity"],
            "exact finding keys"
        );
        assert!(
            matches!(
                finding["severity"].as_str(),
                Some("error" | "warn" | "info")
            ),
            "closed severity: {finding}"
        );
    }
    report
}

#[test]
fn healthy_layout_exits_zero_with_no_error_findings() {
    let layout = Layout::new("healthy");
    layout.install(BINVER);
    layout.shim();
    layout.plugin(BINVER);
    layout.hosts();
    let output = layout.doctor();
    assert!(
        output.status.success(),
        "healthy layout must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report_shape(&output);
    assert_eq!(report["version"], BINVER);
    assert_eq!(
        report["home"],
        layout.root.join(".agent-ide").to_string_lossy().as_ref()
    );
    assert!(report["checked_at"].as_str().unwrap().ends_with('Z'));
    let error_findings: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["severity"] == "error")
        .collect();
    assert!(
        error_findings.is_empty(),
        "no error findings: {error_findings:?}"
    );
    let findings = report["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| finding["component"] == "launcher_config"
                && finding["severity"] == "info"),
        "launcher config verified: {findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding["component"] == "plugin" && finding["code"] == "current"),
        "plugin pinned: {findings:?}"
    );
    let _ = fs::remove_dir_all(&layout.root);
}

#[test]
fn missing_config_is_an_error_exiting_two() {
    let layout = Layout::new("no-config");
    fs::remove_file(layout.config()).unwrap();
    let output = layout.doctor();
    assert_eq!(output.status.code(), Some(2), "missing config must exit 2");
    let report = report_of(&output);
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["component"] == "launcher_config"
                    && finding["severity"] == "error"
                    && finding["code"] == "missing"
            }),
        "missing-config error finding: {report}"
    );
    let _ = fs::remove_dir_all(&layout.root);
}

#[test]
fn plugin_version_mismatch_warns_with_the_restart_hint() {
    let layout = Layout::new("plugin-mismatch");
    layout.install(BINVER);
    layout.shim();
    layout.plugin("0.0.1");
    layout.hosts();
    let output = layout.doctor();
    assert_eq!(
        output.status.code(),
        Some(0),
        "warn findings never fail the doctor"
    );
    let report = report_of(&output);
    let mismatch = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["component"] == "plugin")
        .unwrap();
    assert_eq!(mismatch["severity"], "warn");
    assert_eq!(mismatch["code"], "version_mismatch");
    assert!(
        mismatch["detail"]
            .as_str()
            .unwrap()
            .contains("restart agent-run after install"),
        "restart hint present: {mismatch}"
    );
    let _ = fs::remove_dir_all(&layout.root);
}

#[test]
fn error_journal_lines_in_the_last_day_are_counted() {
    let layout = Layout::new("journal");
    layout.journal_line("0123456789abcdef", "error");
    layout.journal_line("0123456789abcdef", "error");
    layout.journal_line("0123456789abcdef", "info");
    let output = layout.doctor();
    assert!(output.status.success());
    let report = report_of(&output);
    let volume = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["component"] == "errors")
        .unwrap();
    assert_eq!(volume["code"], "volume");
    assert_eq!(volume["severity"], "info");
    assert_eq!(
        volume["detail"],
        "2 error-level journal lines in the last 24 hours"
    );
    let _ = fs::remove_dir_all(&layout.root);
}

/// `.js`/`.mjs`/`.cjs` toolchain entries are probed through the configured Node — a
/// non-executable `tsc.js` reports a version instead of `unresponsive` — an interpreter
/// answering `--version` on stderr is accepted, and JSON-RPC framing noise (a `tsserver.js`
/// answer that carries no version) never reads as a version.
#[test]
fn js_and_stderr_toolchains_report_versions_not_unresponsive() {
    let layout = Layout::new("js-probe");
    let tools = layout.root.join("tools");
    fs::create_dir_all(&tools).unwrap();
    // A fake node: its own --version is answered on stderr, a module probe echoes the module
    // path on stdout, and a framing-only module gets the tsserver-shaped JSON-RPC noise that
    // names no version.
    fs::write(
        tools.join("node"),
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo >&2 \"fake-node 24.4.0-fake\" ;;\n  *noisy.js) printf 'Content-Length: 76\\n\\n{\"seq\":0,\"type\":\"event\",\"body\":{\"pid\":33367}}\\n' ;;\n  *) echo \"fake-node $1\" ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(tools.join("node"), fs::Permissions::from_mode(0o755)).unwrap();
    // Both modules are non-executable: only the node probe can answer for them.
    fs::write(tools.join("tsc.js"), "module.exports = 'fake tsc';\n").unwrap();
    fs::write(tools.join("noisy.js"), "module.exports = 'noise';\n").unwrap();
    let node = tools.join("node");
    let tsc = tools.join("tsc.js");
    let noisy = tools.join("noisy.js");
    fs::write(
        layout.config(),
        serde_json::json!({
            "version": 1,
            "limits": {"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},
            "targets": [],
            "allowed_roots": [layout.root],
            "project_checks": {
                "typescript": {"node": node, "tsc_cli": tsc},
                "python": {"node": node, "pyright_cli": noisy},
            },
        })
        .to_string(),
    )
    .unwrap();
    let output = layout.doctor();
    assert!(
        output.status.success(),
        "warn findings never fail the doctor: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report_of(&output);
    let toolchains: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["component"] == "toolchains")
        .collect();
    let finding_of = |label: &str| -> serde_json::Value {
        toolchains
            .iter()
            .find(|finding| finding["detail"].as_str().unwrap().starts_with(label))
            .map(|finding| (*finding).clone())
            .unwrap_or_else(|| panic!("no {label} finding: {toolchains:?}"))
    };
    // The interpreter's own probe accepts its stderr line.
    let node_finding = finding_of("node ");
    assert_eq!(node_finding["code"], "version");
    assert_eq!(node_finding["detail"], "node fake-node 24.4.0-fake");
    // The non-executable tsc.js went through node: the version line carries its path.
    let tsc_finding = finding_of("tsc.js ");
    assert_eq!(tsc_finding["code"], "version");
    let tsc_detail = tsc_finding["detail"].as_str().unwrap();
    assert!(
        tsc_detail.starts_with("tsc.js fake-node ") && tsc_detail.ends_with("/tsc.js"),
        "tsc.js must be probed as `node tsc.js --version`: {tsc_detail}"
    );
    // The framing-only answer is no version: noisy.js reads as unresponsive.
    let noisy_finding = finding_of("noisy.js ");
    assert_eq!(noisy_finding["code"], "unresponsive");
    assert_eq!(noisy_finding["severity"], "warn");
    assert_eq!(
        toolchains.len(),
        3,
        "exactly the declared tools: {toolchains:?}"
    );
    let _ = fs::remove_dir_all(&layout.root);
}

/// Spawns one health-only daemon at `runtime`, optionally reporting a test version, and waits for
/// its socket to answer before returning the child for the caller to terminate.
fn spawn_health_daemon(runtime: &std::path::Path, version: Option<&str>) -> std::process::Child {
    use std::process::Stdio;
    let mut command = agent_ide();
    command
        .args(["daemon", "--runtime-dir"])
        .arg(runtime)
        .env_remove("AGENT_IDE_LAUNCHER_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(version) = version {
        command.env("AGENT_IDE_TEST_DAEMON_VERSION", version);
    }
    let mut child = command.spawn().unwrap();
    let socket = runtime.join("agent-ide.sock");
    let mut answered = false;
    for _ in 0..400 {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            answered = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    if !answered {
        let _ = child.kill();
        let _ = child.wait();
        panic!("health daemon never answered on {}", socket.display());
    }
    child
}

/// Terminates a test-spawned daemon through the same orderly SIGTERM path as production.
fn stop_health_daemon(mut child: std::process::Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    };
    let _ = child.wait();
}

/// The daemons component lists this user's live daemons with their reported versions and flags
/// the outdated one, while the install pieces stay at their own findings (0.6.7).
#[test]
fn live_daemons_are_listed_with_versions_and_outdated_ones_flagged() {
    let layout = Layout::new("daemons");
    layout.install(BINVER);
    layout.shim();
    layout.plugin(BINVER);
    layout.hosts();
    let temp_root = fs::canonicalize(std::env::temp_dir()).unwrap();
    let unique = TEST_ID.fetch_add(2, Ordering::Relaxed);
    let current_runtime = temp_root.join(format!(
        "aide-doctor-live-{}-{unique}-current",
        std::process::id()
    ));
    let outdated_runtime = temp_root.join(format!(
        "aide-doctor-live-{}-{unique}-outdated",
        std::process::id()
    ));
    let current = spawn_health_daemon(&current_runtime, None);
    let outdated = spawn_health_daemon(&outdated_runtime, Some("0.6.4"));

    let output = layout.doctor();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report_of(&output);
    let daemons: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["component"] == "daemons")
        .collect();
    let running = daemons
        .iter()
        .find(|finding| finding["code"] == "running")
        .expect("a live daemon must be listed");
    assert_eq!(running["severity"], "info");
    let detail = running["detail"].as_str().unwrap();
    assert!(detail.contains(BINVER), "current daemon listed: {detail}");
    assert!(detail.contains("0.6.4"), "outdated daemon listed: {detail}");
    let outdated_finding = daemons
        .iter()
        .find(|finding| finding["code"] == "outdated")
        .expect("an outdated daemon must be flagged");
    assert_eq!(outdated_finding["severity"], "warn");
    assert!(
        outdated_finding["detail"]
            .as_str()
            .unwrap()
            .contains("outdated for this binary"),
        "outdated finding names the comparison: {outdated_finding}"
    );

    stop_health_daemon(current);
    stop_health_daemon(outdated);
    assert!(!current_runtime.exists() && !outdated_runtime.exists());
    let _ = fs::remove_dir_all(&layout.root);
}
