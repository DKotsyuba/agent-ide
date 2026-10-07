//! The `agent-ide doctor` command with no arguments: the read-only installation doctor.
//!
//! Reports the launcher configuration, the toolchains it declares, the standalone install and
//! launcher shim, the host plugin pinning, Codex/Claude host wiring, stale runtime entries under
//! the temporary root, and the last day's error-journal volume as bounded JSON findings. It never
//! creates state, never starts a daemon, and only ever names paths below the effective user's own
//! home (see [`crate::userhome`]). Its one mutation is retiring the product's own abandoned
//! runtime directories (see [`is_product_runtime_name`]); no other temporary entry is counted,
//! probed or removed.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::app::DoctorStatus;
use crate::assistance::launcher::LauncherConfig;
use crate::{errorlog, userhome};

/// Findings above this count are dropped; the report stays bounded whatever the layout.
const MAX_FINDINGS: usize = 256;
/// Wall-clock ceiling for one toolchain `--version` probe.
const TOOLCHAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Runtime entries older than this below the temp root count as stale once nobody listens on
/// their sockets; a live daemon answers on its socket no matter how long it has been running.
const STALE_RUNTIME_AGE: Duration = Duration::from_secs(24 * 3600);

/// Name prefix of the shared repository daemons managed Claude sessions rendezvous with in
/// `/private/tmp` (the same prefix the launcher and the error log use).
const SHARED_RUNTIME_PREFIX: &str = "ai-r-";
/// Name prefix of the private per-process runtime directories the front creates below the temp
/// root for an owned daemon.
const OWNED_RUNTIME_PREFIX: &str = "ai-";
/// Hexadecimal characters after either runtime prefix (eight random or digest bytes).
const RUNTIME_SUFFIX_LEN: usize = 16;
/// Wall-clock ceiling for one socket-liveness connect in the stale-runtime check.
const SOCKET_LIVENESS_TIMEOUT: Duration = Duration::from_millis(250);
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
    check_running_daemons(&mut findings).await;
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
///
/// `.js`/`.mjs`/`.cjs` entries are Node modules rather than executables (language-server
/// bridges, compiler and server modules), so they are probed as
/// `<configured node> <file> --version` instead of being executed directly.
async fn check_toolchains(findings: &mut Vec<Finding>, config: Option<&LauncherConfig>) {
    let Some(config) = config else {
        return;
    };
    for (label, path, node) in declared_toolchains(config) {
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
        let node = node.filter(|_| is_js_module(&path));
        match probe_version(&path, node.as_deref()).await {
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

/// Reports whether `path` names a Node module (`server.js`, `cli.mjs`, `tool.cjs`, ...).
fn is_js_module(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "mjs" | "cjs")
    )
}

/// Collects the deduplicated `(label, path, node)` toolchain executables declared by `config`.
///
/// Provider executables are each configured language server's toolchain programs (the server
/// and the interpreters and modules its declaration names, see
/// [`LanguageServer::toolchain_programs`](crate::intelligence::server::LanguageServer::toolchain_programs)),
/// and the confined project-check tool paths of each declared language; the accepted `git` is
/// already digest-verified. The third element is the interpreter declared alongside the entry,
/// used only to probe `.js`-family modules that cannot exec themselves.
fn declared_toolchains(config: &LauncherConfig) -> Vec<(String, PathBuf, Option<PathBuf>)> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut declare = |path: &Path, node: Option<&Path>| {
        if seen.insert(path.to_path_buf()) {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            found.push((label, path.to_path_buf(), node.map(Path::to_path_buf)));
        }
    };
    for attachment in config.attachments() {
        let Some(target) = config.target(attachment) else {
            continue;
        };
        for provider in &target.providers {
            for (path, interpreter) in provider.server().toolchain_programs(provider) {
                declare(&path, interpreter.as_deref());
            }
        }
    }
    if let Some(checks) = config.project_checks() {
        for (_, section) in checks.sections() {
            for (path, interpreter) in section.programs() {
                declare(&path, interpreter.as_deref());
            }
        }
    }
    found
}

/// Runs one bounded `--version` probe, returning its first version-looking line on a clean exit.
///
/// When `node` is set, `path` is a Node module probed as `node path --version`. A version line
/// on stdout or stderr is accepted (some language servers print `--version` on either stream);
/// the probe counts as unresponsive only on a non-zero exit, no version-looking line at all,
/// or the 5 s timeout. Version-looking means a non-empty line carrying a digit that is not
/// JSON-RPC framing — a protocol-only server module may answer `--version` only with its
/// `Content-Length`-framed startup event, which names no version and must not read as one.
async fn probe_version(path: &Path, node: Option<&Path>) -> Option<String> {
    let mut command = match node {
        Some(node) => {
            let mut command = tokio::process::Command::new(node);
            command.arg(path);
            command
        }
        None => tokio::process::Command::new(path),
    };
    let output = tokio::time::timeout(TOOLCHAIN_PROBE_TIMEOUT, command.arg("--version").output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version_line = |bytes: &[u8]| -> Option<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|line| {
                !line.is_empty()
                    && !line.starts_with("Content-")
                    && !line.starts_with('{')
                    && line.chars().any(|character| character.is_ascii_digit())
            })
            .map(str::to_owned)
    };
    version_line(&output.stdout).or_else(|| version_line(&output.stderr))
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

/// Reports whether `name` is exactly the name of one of the product's own runtime directories:
/// `ai-` or `ai-r-` followed by sixteen lowercase hexadecimal characters.
///
/// Every other temporary entry belongs to some other program or to a cache a live session still
/// reads (the `ai-k-` key caches carry hook state of running sessions), so doctor neither
/// counts, probes nor removes it.
fn is_product_runtime_name(name: &str) -> bool {
    [SHARED_RUNTIME_PREFIX, OWNED_RUNTIME_PREFIX]
        .iter()
        .filter_map(|prefix| name.strip_prefix(prefix))
        .any(|suffix| {
            suffix.len() == RUNTIME_SUFFIX_LEN
                && suffix
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Lists this user's live daemons below the temp root with their versions, flags outdated ones
/// (0.6.7), and retires the product's own abandoned runtime directories; never names their paths.
///
/// A live daemon is one of this user's runtime directories whose daemon socket answers the
/// side-effect-free health exchange, so its reported version is read from the reply the product
/// itself uses for the same decision. Only versions are reported, never paths.
///
/// Only a directory named exactly like a product runtime ([`is_product_runtime_name`]), owned by
/// this user and private (no group or other access) is a candidate. A candidate older than a day
/// whose sockets answer nobody (no daemon, so no lease either) is abandoned: it is removed and
/// counted as pruned, or counted as stale when removal fails. A younger or answering candidate is
/// never touched.
///
/// The shared repository daemons a managed Claude session rendezvouses with live in
/// `/private/tmp` under the `ai-r-` prefix rather than below the temp root, so those entries
/// are listed too — they are exactly the long-lived daemons an upgrade can leave outdated.
async fn check_running_daemons(findings: &mut Vec<Finding>) {
    let Ok(root) = fs::canonicalize(std::env::temp_dir()) else {
        return;
    };
    let mut candidates = Vec::new();
    let shared = Path::new("/private/tmp");
    let mut roots = vec![root.as_path()];
    if root.as_path() != shared {
        roots.push(shared);
    }
    for dir in roots {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !is_product_runtime_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            // `DirEntry::metadata` does not follow a symlink, so a link named like a runtime is
            // neither a directory nor a candidate.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let owned = metadata.uid() == unsafe { libc::geteuid() }
                && metadata.is_dir()
                && metadata.mode() & 0o077 == 0;
            if owned {
                candidates.push(entry.path());
            }
        }
    }
    let mut versions: BTreeMap<String, usize> = BTreeMap::new();
    let mut outdated = 0usize;
    let mut stale = 0usize;
    let mut pruned = 0usize;
    for path in candidates {
        let stale_age = fs::symlink_metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_RUNTIME_AGE);
        if !has_live_listener(&path).await {
            if stale_age {
                if fs::remove_dir_all(&path).is_ok() {
                    pruned += 1;
                } else {
                    stale += 1;
                }
            }
            continue;
        }
        let Some((version, outdated_version)) = live_daemon_version(&path).await else {
            continue;
        };
        if outdated_version {
            outdated += 1;
        }
        *versions
            .entry(version.unwrap_or_else(|| "pre-0.6.7 (unknown)".to_owned()))
            .or_default() += 1;
    }
    if !versions.is_empty() {
        let listed = versions
            .iter()
            .map(|(version, count)| format!("{version} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        push(
            findings,
            "running",
            "info",
            "daemons",
            format!(
                "{} live agent-ide daemon{} serving: {listed}",
                versions.values().sum::<usize>(),
                if versions.values().sum::<usize>() == 1 {
                    ""
                } else {
                    "s"
                },
            ),
        );
    }
    if outdated > 0 {
        push(
            findings,
            "outdated",
            "warn",
            "daemons",
            format!(
                "{outdated} live daemon{} outdated for this binary ({}); a new front replaces {} \
                 automatically once no session is bound, and serves on with a start-card notice \
                 while one is",
                if outdated == 1 { "" } else { "s" },
                env!("CARGO_PKG_VERSION"),
                if outdated == 1 { "it" } else { "them" },
            ),
        );
    }
    if pruned > 0 {
        push(
            findings,
            "stale_runtime_pruned",
            "info",
            "daemons",
            format!(
                "{pruned} abandoned agent-ide runtime director{} removed (older than a day, no daemon listening)",
                if pruned == 1 { "y" } else { "ies" },
            ),
        );
    }
    if stale > 0 {
        push(
            findings,
            "stale_runtime",
            "warn",
            "daemons",
            format!(
                "{stale} stale agent-ide runtime entries owned by this user under the temporary directory could not be removed"
            ),
        );
    }
}

/// Reads one live daemon's health-reported version and replacement status, or `None` when the
/// entry is not a runtime directory this product can query.
///
/// `None` inside `Some` marks a daemon that reports no version: one older than 0.6.7, the exact
/// case the outdated finding names.
async fn live_daemon_version(path: &Path) -> Option<(Option<String>, bool)> {
    let report = crate::app::doctor_report(path).await.ok()?;
    let DoctorStatus::Healthy { daemon_generation } = report.status else {
        return None;
    };
    Some((
        crate::app::reported_daemon_version(&daemon_generation).map(str::to_owned),
        crate::app::daemon_needs_replacement(&daemon_generation, env!("CARGO_PKG_VERSION")),
    ))
}

/// Reports whether any socket directly inside the runtime directory `path` still has a listening
/// owner. A successful connect proves a live daemon; refusal, a missing path, or any other connect
/// failure means nobody is listening. This is the same liveness question `retire_stale_socket`
/// asks before removing a socket file.
async fn has_live_listener(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    let sockets: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_socket()))
        .map(|entry| entry.path())
        .collect();
    for socket in sockets {
        if let Ok(Ok(_)) = tokio::time::timeout(
            SOCKET_LIVENESS_TIMEOUT,
            tokio::net::UnixStream::connect(&socket),
        )
        .await
        {
            return true;
        }
    }
    false
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
