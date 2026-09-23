//! Runs one real confined `cargo check` exactly as [`RustChecker`] does and prints the raw
//! cargo stderr, so a Seatbelt build failure can be diagnosed without the snapshot's 160-byte
//! `error:`-line cap.
//!
//! This mirrors [`RustChecker::run_check`]'s cache setup and [`RustChecker::cargo_check_spec`],
//! then runs the resulting [`RunSpec`] through the production [`SeatbeltRunner`] exactly as the
//! daemon does. The [`ProblemSnapshot`] printed at the end is built with the crate's public
//! [`parse_cargo_messages`] parser directly on the raw output; `RustChecker`'s private
//! `map_run_output` also short-circuits on timeout, truncation, and a `--locked` lockfile
//! refusal before parsing, and that precedence is not reproduced here (those are already visible
//! above as `timed_out`/`truncated` and in the full stderr), so a timed-out or truncated run's
//! printed snapshot is a plain parse rather than the daemon's `Unavailable(Timeout)`/
//! `Unavailable(Fatal)` verdict.

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Arc, time::Duration, time::Instant};

use agent_ide::checks::{
    CheckRequest, ProblemSnapshot,
    runner::{ConfinedRunner, RunOutput, RunSpec, SeatbeltRunner},
    rust::{RustChecker, parse_cargo_messages},
};

const USAGE: &str = "usage: confined_cargo_check --worktree <path> --toolchain-dir <path> \
--cache-dir <path> [--cargo-home <path>] [--developer-dir <path>] [--timeout-s <n>]";

/// Parsed command-line arguments; required paths are absolute worktree/toolchain/cache roots.
struct Args {
    worktree: PathBuf,
    toolchain_dir: PathBuf,
    cache_dir: PathBuf,
    cargo_home: Option<PathBuf>,
    developer_dir: Option<PathBuf>,
    timeout_s: u64,
}

impl Args {
    /// Parses flags by hand; unknown flags or a missing required flag are errors.
    fn parse<I: Iterator<Item = String>>(mut args: I) -> Result<Self, String> {
        let mut worktree = None;
        let mut toolchain_dir = None;
        let mut cache_dir = None;
        let mut cargo_home = None;
        let mut developer_dir = None;
        let mut timeout_s = 600u64;
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--worktree" => worktree = Some(PathBuf::from(next_value(&mut args, &flag)?)),
                "--toolchain-dir" => {
                    toolchain_dir = Some(PathBuf::from(next_value(&mut args, &flag)?));
                }
                "--cache-dir" => cache_dir = Some(PathBuf::from(next_value(&mut args, &flag)?)),
                "--cargo-home" => cargo_home = Some(PathBuf::from(next_value(&mut args, &flag)?)),
                "--developer-dir" => {
                    developer_dir = Some(PathBuf::from(next_value(&mut args, &flag)?));
                }
                "--timeout-s" => {
                    let raw = next_value(&mut args, &flag)?;
                    timeout_s = raw
                        .parse()
                        .map_err(|_| format!("invalid --timeout-s value: {raw}"))?;
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            worktree: worktree.ok_or("--worktree is required")?,
            toolchain_dir: toolchain_dir.ok_or("--toolchain-dir is required")?,
            cache_dir: cache_dir.ok_or("--cache-dir is required")?,
            cargo_home,
            developer_dir,
            timeout_s,
        })
    }
}

/// Returns the next argument or an error naming the flag that required it.
fn next_value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match Args::parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = fs::create_dir_all(&args.cache_dir) {
        eprintln!(
            "failed to create cache dir {}: {error}",
            args.cache_dir.display()
        );
        return ExitCode::FAILURE;
    }

    let checker = RustChecker::new(
        Arc::new(SeatbeltRunner),
        args.toolchain_dir.clone(),
        args.cargo_home.clone(),
        Duration::from_secs(args.timeout_s),
        args.developer_dir.clone(),
    );
    let request = CheckRequest {
        read_denies: Vec::new(),
        worktree: args.worktree,
        cache_dir: args.cache_dir,
        input_generation: 1,
    };
    // Mirrors RustChecker::run_check: the cache tmp/target subdirectories must exist before the
    // confined process starts, since the process itself cannot create them outside its write root.
    if let Err(error) = fs::create_dir_all(request.cache_dir.join("tmp"))
        .and_then(|()| fs::create_dir_all(request.cache_dir.join("target")))
    {
        eprintln!("failed to create cache subdirectories: {error}");
        return ExitCode::FAILURE;
    }

    let spec = checker.cargo_check_spec(&request);
    print_spec(&spec);

    let started = Instant::now();
    let output = match SeatbeltRunner.run(spec).await {
        Ok(output) => output,
        Err(error) => {
            eprintln!("runner failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    print_output(&output);

    println!("=== ProblemSnapshot (parse_cargo_messages on the raw output) ===");
    let snapshot: ProblemSnapshot = parse_cargo_messages(
        &output.stdout,
        &output.stderr,
        request.input_generation,
        duration_ms,
    );
    println!("{snapshot:#?}");

    ExitCode::SUCCESS
}

/// Prints every field of the confined run specification to stderr.
fn print_spec(spec: &RunSpec) {
    eprintln!("=== RunSpec ===");
    eprintln!("program: {}", spec.program.display());
    eprintln!("args: {:?}", spec.args);
    eprintln!("cwd: {}", spec.cwd.display());
    eprintln!("env:");
    for (key, value) in &spec.env {
        eprintln!("  {key}={value}");
    }
    eprintln!("read_roots:");
    for root in &spec.read_roots {
        eprintln!("  {}", root.display());
    }
    eprintln!("write_roots:");
    for root in &spec.write_roots {
        eprintln!("  {}", root.display());
    }
    eprintln!("timeout: {:?}", spec.timeout);
    eprintln!("max_output_bytes: {}", spec.max_output_bytes);
}

/// Prints the confined run outcome: status, timeout/truncation flags, full stderr, and a
/// bounded tail of stdout.
fn print_output(output: &RunOutput) {
    println!("=== RunOutput ===");
    println!("status: {:?}", output.status);
    println!("timed_out: {}", output.timed_out);
    println!("truncated: {}", output.truncated);
    println!("--- stderr (full, lossy utf8) ---");
    println!("{}", String::from_utf8_lossy(&output.stderr));
    println!("--- stdout (last 30 lines, lossy utf8) ---");
    println!("{}", last_lines(&output.stdout, 30));
}

/// Returns the last `n` lines of `bytes`, decoded as lossy UTF-8.
fn last_lines(bytes: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
