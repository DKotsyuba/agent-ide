//! The `agent-ide doctor` command with no arguments: the read-only installation doctor.
//!
//! Reports the launcher configuration, the toolchains it declares, the standalone install and
//! launcher shim, the host plugin pinning, Codex/Claude host wiring, stale runtime entries under
//! the temporary root, and the last day's error-journal volume as bounded JSON findings. It never
//! creates or mutates state, never starts a daemon, and only ever names paths below the effective
//! user's own home (see [`crate::userhome`]).

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::assistance::launcher::LauncherConfig;
use crate::{errorlog, userhome};

/// Findings above this count are dropped; the report stays bounded whatever the layout.
const MAX_FINDINGS: usize = 256;
/// Wall-clock ceiling for one toolchain `--version` probe.
const TOOLCHAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Runtime entries older than this below the temp root count as stale; managed daemons idle-exit
/// within at most one hour, so a day-old `ai-` entry has no live owner.
const STALE_RUNTIME_AGE: Duration = Duration::from_secs(24 * 3600);
/// Error-level journal lines in the last day above which the volume finding warns.
const ERROR_LOG_WARN_COUNT: u64 = 50;

/// One bounded installation finding.
#[derive(Serialize)]
pub struct Finding {
    /// Closed finding code within its component.
    pub code: &'static str,
    /// `error` fails the doctor; `warn` and `info` never do.
    pub severity: &'static str,
    /// Closed component tag: `launcher_config`, `toolchains`, `install`, `plugin`, `hosts`,
    /// `daemons`, or `errors`.
    pub component: &'static str,
    /// One-line human explanation; only ever names this user's own paths.
    pub detail: String,
}

/// The complete installation doctor report.
#[derive(Serialize)]
pub struct Report {
    /// The running binary's version.
    pub version: String,
    /// The effective home directory the checks were resolved against.
    pub home: String,
    /// RFC 3339 UTC moment of the check.
    pub checked_at: String,
    /// Findings in check order, capped at 256 entries.
    pub findings: Vec<Finding>,
}

/// Runs every installation check read-only and returns the bounded report.
pub async fn report() -> Report {
    let mut findings = Vec::new();
    let Some(effective) = userhome::user_home() else {
        push(
            &mut findings,
            "no_home",
            "error",
            "install",
            "no home directory is available".to_owned(),
        );
        return finish(findings, PathBuf::new());
    };
    let home = effective.join(".agent-ide");
    let launcher = check_launcher(&mut findings, &effective);
    check_toolchains(&mut findings, launcher.as_ref()).await;
    check_install(&mut findings, &effective);
    check_plugin(&mut findings, &effective);
    check_hosts(&mut findings, &effective);
    check_stale_runtimes(&mut findings);
    check_error_journal(&mut findings, &home);
    finish(findings, home)
}

/// Renders the report with this binary's version and the current RFC 3339 moment.
fn finish(findings: Vec<Finding>, home: PathBuf) -> Report {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Report {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        home: home.to_string_lossy().into_owned(),
        checked_at: errorlog::format_rfc3339(now),
        findings,
    }
}

/// Appends one finding while the report stays under its cap.
fn push(
    findings: &mut Vec<Finding>,
    code: &'static str,
    severity: &'static str,
    component: &'static str,
    detail: String,
) {
    if findings.len() < MAX_FINDINGS {
        findings.push(Finding {
            code,
            severity,
            component,
            detail,
        });
    }
}

/// Verifies the launcher configuration file and returns it when fully valid.
///
/// Reuses `LauncherConfig::read` and `verify`, the daemon's own startup checks, so a missing,
/// malformed, or digest-stale executable is the same `error` the daemon would fail on.
fn check_launcher(findings: &mut Vec<Finding>, effective: &Path) -> Option<LauncherConfig> {
    let path = effective.join(".config").join("agent-ide/launcher.json");
    if fs::symlink_metadata(&path).is_err() {
        push(
            findings,
            "missing",
            "error",
            "launcher_config",
            format!("launcher configuration {} not found", path.display()),
        );
        return None;
    }
    match LauncherConfig::read(&path) {
        Ok(config) => match config.verify() {
            Ok(()) => {
                push(
                    findings,
                    "verified",
                    "info",
                    "launcher_config",
                    format!("launcher configuration {} verified", path.display()),
                );
                Some(config)
            }
            Err(_) => {
                push(
                    findings,
                    "digest_mismatch",
                    "error",
                    "launcher_config",
                    format!(
                        "an accepted executable in {} no longer matches its digest",
                        path.display()
                    ),
                );
                None
            }
        },
        Err(_) => {
            push(
                findings,
                "invalid",
                "error",
                "launcher_config",
                format!("launcher configuration {} is malformed", path.display()),
            );
            None
        }
    }
}

/// Probes every toolchain executable the configuration declares with a bounded `--version`.
async fn check_toolchains(findings: &mut Vec<Finding>, config: Option<&LauncherConfig>) {
    let Some(config) = config else {
        return;
    };
    for (label, path) in declared_toolchains(config) {
        if !path.is_file() {
            push(
                findings,
                "missing",
                "warn",
                "toolchains",
                format!("{label} {} not found", path.display()),
            );
            continue;
        }
        match probe_version(&path).await {
            Some(version) => push(
                findings,
                "version",
                "info",
                "toolchains",
                format!("{label} {version}"),
            ),
            None => push(
                findings,
                "unresponsive",
                "warn",
                "toolchains",
                format!("{label} {} did not answer --version", path.display()),
            ),
        }
    }
}

/// Collects the deduplicated `(label, path)` toolchain executables declared by `config`.
///
/// Provider executables are the configured language servers (rust-analyzer, pyright,
/// typescript-language-server), plus each provider's Node and TypeScript `tsserver`, and the
/// confined project-check tool paths; the accepted `git` is already digest-verified.
fn declared_toolchains(config: &LauncherConfig) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut declare = |path: &Path| {
        if seen.insert(path.to_path_buf()) {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            found.push((label, path.to_path_buf()));
        }
    };
    for attachment in config.attachments() {
        let Some(target) = config.target(attachment) else {
            continue;
        };
        for provider in &target.providers {
            declare(&provider.executable.path);
            if let Some(node) = &provider.node {
                declare(&node.path);
            }
            if let Some(typescript) = &provider.typescript {
                declare(&typescript.tsserver.path);
            }
        }
    }
    if let Some(checks) = config.project_checks() {
        if let Some(python) = checks.python() {
            declare(python.node());
            declare(python.pyright_cli());
        }
        if let Some(typescript) = checks.typescript() {
            declare(typescript.node());
            declare(typescript.tsc_cli());
        }
    }
    found
}

/// Runs one bounded `--version` probe, returning its first stdout line on a clean exit.
async fn probe_version(path: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        TOOLCHAIN_PROBE_TIMEOUT,
        tokio::process::Command::new(path).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_owned();
    (!line.is_empty()).then_some(line)
}

/// Reports the standalone release pinning and the launcher shim.
fn check_install(findings: &mut Vec<Finding>, effective: &Path) {
    let current = effective.join(".agent-ide/standalone/current");
    let installed = fs::canonicalize(&current).ok().and_then(|release| {
        let version = release.file_name()?.to_string_lossy().into_owned();
        release.join("COMPLETE").is_file().then_some(version)
    });
    let running = std::env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    match installed {
        Some(version) if version == env!("CARGO_PKG_VERSION") => push(
            findings,
            "current",
            "info",
            "install",
            format!("standalone release {version} is current"),
        ),
        Some(version) => {
            let in_prefix = running
                .as_ref()
                .is_some_and(|exe| exe.starts_with(effective.join(".agent-ide/standalone")));
            let detail = if in_prefix {
                format!(
                    "running binary {} differs from installed release {version}",
                    env!("CARGO_PKG_VERSION")
                )
            } else {
                format!(
                    "running from a source build outside the prefix; installed release is {version}"
                )
            };
            push(findings, "version_mismatch", "warn", "install", detail);
        }
        None => push(
            findings,
            "missing",
            "warn",
            "install",
            format!(
                "{} does not resolve to a complete release",
                current.display()
            ),
        ),
    }
    let shim = effective.join(".local/bin/agent-ide");
    match fs::symlink_metadata(&shim) {
        Ok(_) => push(
            findings,
            "present",
            "info",
            "install",
            format!("launcher shim {} present", shim.display()),
        ),
        Err(_) => push(
            findings,
            "missing",
            "warn",
            "install",
            format!("launcher shim {} missing", shim.display()),
        ),
    }
}

/// Reports the host plugin pinning against this binary's version.
fn check_plugin(findings: &mut Vec<Finding>, effective: &Path) {
    let current = effective.join(".local/share/agent-ide/plugin/current");
    let version = fs::canonicalize(&current)
        .ok()
        .and_then(|dir| fs::read_to_string(dir.join(".claude-plugin/plugin.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|manifest| {
            manifest
                .get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match version {
        Some(version) if version == env!("CARGO_PKG_VERSION") => push(
            findings,
            "current",
            "info",
            "plugin",
            format!("plugin {version} matches the binary"),
        ),
        Some(version) => push(
            findings,
            "version_mismatch",
            "warn",
            "plugin",
            format!(
                "plugin {version} differs from binary {}; restart agent-run after install",
                env!("CARGO_PKG_VERSION")
            ),
        ),
        None => push(
            findings,
            "missing",
            "warn",
            "plugin",
            format!(
                "plugin {} does not resolve to a readable manifest",
                current.display()
            ),
        ),
    }
}

/// Reports how the Codex and Claude hosts currently reference agent-ide.
fn check_hosts(findings: &mut Vec<Finding>, effective: &Path) {
    let codex_hooks = effective.join(".codex/hooks.json");
    if fs::read_to_string(&codex_hooks).is_ok_and(|text| text.contains("agent-ide")) {
        push(
            findings,
            "configured",
            "info",
            "hosts",
            "codex hooks.json references agent-ide".to_owned(),
        );
    } else {
        push(
            findings,
            "not_configured",
            "warn",
            "hosts",
            format!("{} has no agent-ide handlers", codex_hooks.display()),
        );
    }
    let claude = [effective.join(".claude/settings.json")]
        .into_iter()
        .chain(
            std::env::current_dir()
                .ok()
                .map(|dir| dir.join(".claude/settings.json")),
        )
        .find_map(|path| {
            fs::read_to_string(path)
                .ok()
                .filter(|text| text.contains("agent-ide"))
        });
    match claude {
        Some(_) => push(
            findings,
            "configured",
            "info",
            "hosts",
            "claude settings reference agent-ide".to_owned(),
        ),
        None => push(
            findings,
            "not_configured",
            "warn",
            "hosts",
            "no claude settings file mentions agent-ide".to_owned(),
        ),
    }
    let codex_config = effective.join(".codex/config.toml");
    let mcp =
        fs::read_to_string(&codex_config).is_ok_and(|text| text.contains("mcp_servers.agent-ide"));
    push(
        findings,
        "codex_mcp",
        "info",
        "hosts",
        if mcp {
            "codex config.toml declares the agent-ide MCP server".to_owned()
        } else {
            format!(
                "{} has no [mcp_servers.agent-ide] entry",
                codex_config.display()
            )
        },
    );
}

/// Counts this user's stale `ai-` runtime entries below the temp root; never names them.
///
/// Only entries owned by the effective user with a day-old modification time are counted, so
/// other users' paths are never even read.
fn check_stale_runtimes(findings: &mut Vec<Finding>) {
    let Ok(root) = fs::canonicalize(std::env::temp_dir()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let stale = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("ai-"))
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| {
            metadata.uid() == unsafe { libc::geteuid() }
                && (metadata.is_dir() || metadata.file_type().is_socket())
        })
        .filter(|metadata| {
            metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STALE_RUNTIME_AGE)
        })
        .count();
    if stale > 0 {
        push(
            findings,
            "stale_runtime",
            "warn",
            "daemons",
            format!(
                "{stale} stale agent-ide runtime entries owned by this user under the temporary directory"
            ),
        );
    }
}

/// Counts error-level journal lines of the last day across every repository log directory.
fn check_error_journal(findings: &mut Vec<Finding>, home: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = errorlog::format_rfc3339(now.saturating_sub(24 * 3600));
    let mut count = 0u64;
    if let Ok(entries) = fs::read_dir(home.join("logs")) {
        for dir in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
        {
            for event in errorlog::read_events(&dir) {
                count += u64::from(event.level == "error" && event.timestamp >= cutoff);
            }
        }
    }
    push(
        findings,
        "volume",
        if count > ERROR_LOG_WARN_COUNT {
            "warn"
        } else {
            "info"
        },
        "errors",
        format!("{count} error-level journal lines in the last 24 hours"),
    );
}
