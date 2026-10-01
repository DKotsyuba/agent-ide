//! Runs one real confined `pyright` exactly as [`PythonChecker`] does and prints the raw run,
//! so a Seatbelt read-root or `--pythonpath` problem (T11B) can be diagnosed without the
//! snapshot's 200-character message cap.
//!
//! This mirrors [`PythonChecker::check`]'s interpreter resolution and cache setup and
//! [`PythonChecker::pyright_spec`], then runs the resulting [`RunSpec`] through the production
//! [`SeatbeltRunner`] exactly as the daemon does. The [`ProblemSnapshot`] printed at the end is
//! built with the crate's public [`parse_pyright_output`] parser directly on the raw output;
//! `PythonChecker::check` also short-circuits on a missing tool/interpreter and on timeout before
//! parsing, and that precedence is not reproduced here (those are already visible above as the
//! resolved interpreter, if any, and `timed_out` in the printed [`RunOutput`]).

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Arc, time::Duration, time::Instant};

use agent_ide::checks::{
    CheckRequest, ProblemSnapshot,
    python::{PythonChecker, parse_pyright_output, resolve_interpreter},
    runner::{ConfinedRunner, RunOutput, RunSpec, SeatbeltRunner},
};

const USAGE: &str = "usage: confined_pyright_check --worktree <path> --node <path> \
--pyright-cli <path> --cache-dir <path> [--timeout-s <n>]";

/// Parsed command-line arguments; required paths are absolute worktree/toolchain/cache roots.
struct Args {
    worktree: PathBuf,
    node: PathBuf,
    pyright_cli: PathBuf,
    cache_dir: PathBuf,
    timeout_s: u64,
}

impl Args {
    /// Parses flags by hand; unknown flags or a missing required flag are errors.
    fn parse<I: Iterator<Item = String>>(mut args: I) -> Result<Self, String> {
        let mut worktree = None;
        let mut node = None;
        let mut pyright_cli = None;
        let mut cache_dir = None;
        let mut timeout_s = 300u64;
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--worktree" => worktree = Some(PathBuf::from(next_value(&mut args, &flag)?)),
                "--node" => node = Some(PathBuf::from(next_value(&mut args, &flag)?)),
                "--pyright-cli" => {
                    pyright_cli = Some(PathBuf::from(next_value(&mut args, &flag)?));
                }
                "--cache-dir" => cache_dir = Some(PathBuf::from(next_value(&mut args, &flag)?)),
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
            node: node.ok_or("--node is required")?,
            pyright_cli: pyright_cli.ok_or("--pyright-cli is required")?,
            cache_dir: cache_dir.ok_or("--cache-dir is required")?,
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

    let Some(interpreter) = resolve_interpreter(&args.worktree) else {
        eprintln!(
            "no interpreter resolved for worktree {}",
            args.worktree.display()
        );
        return ExitCode::FAILURE;
    };
    println!("resolved interpreter: {}", interpreter.display());

    let checker = PythonChecker::new(
        Arc::new(SeatbeltRunner),
        args.node.clone(),
        args.pyright_cli.clone(),
        Duration::from_secs(args.timeout_s),
    );
    let request = CheckRequest {
        read_denies: Vec::new(),
        worktree: args.worktree,
        cache_dir: args.cache_dir,
        input_generation: 1,
    };
    // Mirrors PythonChecker::check: the cache tmp subdirectory must exist before the confined
    // process starts, since the process itself cannot create it outside its write root.
    if let Err(error) = fs::create_dir_all(request.cache_dir.join("tmp")) {
        eprintln!("failed to create cache tmp subdirectory: {error}");
        return ExitCode::FAILURE;
    }

    let spec = checker.pyright_spec(&request, &interpreter);
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

    println!("=== pyright summary (raw JSON) ===");
    print_pyright_summary(&output.stdout);

    println!("=== ProblemSnapshot (parse_pyright_output on the raw output) ===");
    let snapshot: ProblemSnapshot = parse_pyright_output(
        output.status,
        &output.stdout,
        &output.stderr,
        request.input_generation,
        duration_ms,
    );
    println!("state: {:?}", snapshot.state);
    println!(
        "errors: {}, warnings: {}",
        snapshot.errors, snapshot.warnings
    );
    println!("first 15 diagnostics:");
    for problem in snapshot.problems.iter().take(15) {
        println!(
            "  {}:{}:{} [{:?}] {:?} {}",
            problem.path,
            problem.line,
            problem.column,
            problem.severity,
            problem.code,
            problem.message
        );
    }

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

/// Prints the confined run outcome: status, timeout/truncation flags, and full stderr.
fn print_output(output: &RunOutput) {
    println!("=== RunOutput ===");
    println!("status: {:?}", output.status);
    println!("timed_out: {}", output.timed_out);
    println!("truncated: {}", output.truncated);
    println!("--- stderr (full, lossy utf8) ---");
    println!("{}", String::from_utf8_lossy(&output.stderr));
}

/// Prints pyright's own `summary` object from its `--outputjson` report, parsed generically
/// (without depending on this crate's private report struct) so a malformed report still prints
/// whatever fields are present.
fn print_pyright_summary(stdout: &[u8]) {
    match serde_json::from_slice::<serde_json::Value>(stdout) {
        Ok(value) => match value.get("summary") {
            Some(summary) => println!("{summary:#}"),
            None => println!("(no \"summary\" field in stdout)"),
        },
        Err(error) => println!("(stdout is not valid JSON: {error})"),
    }
}
