//! Prints a reusable project tree or its identical JSON data without starting the IDE daemon.
//!
//! Run: cargo run --example project_structure -- ROOT --depth 4 --headers --skeleton --imports
//! Add --changed-only for Git-changed files, --diff for counts, or --json for structured output.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use agent_ide::project::structure::{StructureOptions, build_structure, render_tree};

/// Parses a small demonstration CLI and invokes the same public builder library callers use.
fn main() -> Result<(), Box<dyn Error>> {
    agent_ide::languages::install();
    let mut args = std::env::args().skip(1);
    let root = args.next().ok_or("usage: project_structure ROOT [--options FILE] [--benchmark N] [--workers N] [--depth N] [--dirs-only] [--no-hidden] [--no-gitignore] [--include-generated] [--expand PATH] [--exclude PATH] [--extension EXT] [--changed-only] [--diff] [--headers] [--skeleton] [--no-skeleton-comments] [--compact] [--imports] [--size] [--lines] [--max-nodes N] [--json]")?;
    let mut options = StructureOptions::default();
    let mut json = false;
    let mut repetitions = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--workers" => {
                options.workers = args.next().ok_or("--workers requires a number")?.parse()?;
            }
            "--benchmark" => {
                let count: usize = args
                    .next()
                    .ok_or("--benchmark requires a repetition count")?
                    .parse()?;
                if !(1..=100).contains(&count) {
                    return Err("benchmark repetitions must be in 1..=100".into());
                }
                repetitions = Some(count);
            }
            "--options" => {
                let file = args.next().ok_or("--options requires a JSON file")?;
                options = serde_json::from_slice(&std::fs::read(file)?)?;
            }
            "--depth" => {
                options.max_depth = args.next().ok_or("--depth requires a number")?.parse()?
            }
            "--dirs-only" => options.include_files = false,
            "--no-hidden" => options.include_hidden = false,
            "--no-gitignore" => options.respect_gitignore = false,
            "--include-generated" => options.exclude_generated = false,
            "--expand" => options.expand_paths.push(PathBuf::from(
                args.next().ok_or("--expand requires a path")?,
            )),
            "--exclude" => options.exclude_paths.push(PathBuf::from(
                args.next().ok_or("--exclude requires a path")?,
            )),
            "--extension" => options
                .extensions
                .push(args.next().ok_or("--extension requires an extension")?),
            "--changed-only" => options.changed_only = true,
            "--diff" => options.include_diff = true,
            "--headers" => options.include_header = true,
            "--skeleton" => options.include_skeleton = true,
            "--no-skeleton-comments" => options.include_skeleton_comments = false,
            "--skeleton-comments" => options.include_skeleton_comments = true,
            "--compact" => options.compact = true,
            "--imports" => options.include_imports = true,
            "--size" => options.include_size = true,
            "--lines" => options.include_line_count = true,
            "--max-nodes" => {
                options.max_nodes = args
                    .next()
                    .ok_or("--max-nodes requires a number")?
                    .parse()?
            }
            "--json" => json = true,
            _ => return Err(format!("unknown option: {flag}").into()),
        }
    }
    if let Some(repetitions) = repetitions {
        println!(
            "{}",
            serde_json::to_string_pretty(&benchmark(Path::new(&root), &options, repetitions)?)?
        );
        return Ok(());
    }
    let structure = build_structure(Path::new(&root), &options)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&structure)?);
    } else {
        print!("{}", render_tree(&structure));
    }
    Ok(())
}

/// Measures fresh collections and both formats in process; hashing/printing are outside timers.
fn benchmark(
    root: &Path,
    options: &StructureOptions,
    repetitions: usize,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut runs = Vec::new();
    for iteration in 1..=repetitions {
        let started = Instant::now();
        let structure = build_structure(root, options)?;
        let collected = started.elapsed();
        let text = render_tree(&structure);
        let text_done = started.elapsed();
        let json = serde_json::to_string_pretty(&structure)?;
        let total = started.elapsed();
        std::hint::black_box((&structure, &text, &json));
        runs.push(serde_json::json!({
            "iteration": iteration,
            "collect_ms": collected.as_secs_f64() * 1000.0,
            "tree_render_ms": (text_done - collected).as_secs_f64() * 1000.0,
            "json_serialize_ms": (total - text_done).as_secs_f64() * 1000.0,
            "total_ms": total.as_secs_f64() * 1000.0,
            "text_bytes": text.len(), "json_bytes": json.len(),
            "text_blake3": blake3::hash(text.as_bytes()).to_hex().to_string(),
            "json_blake3": blake3::hash(json.as_bytes()).to_hex().to_string(),
            "summary": structure.summary,
        }));
    }
    Ok(serde_json::json!({
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "root": root.canonicalize()?, "options": options,
        "cache_policy": "OS cache not cleared; first iteration is not a cold-start measurement",
        "timer_scope": "collection including Git, text rendering, JSON serialization; excludes compilation, process startup, output hashing, printing and disk output",
        "runs": runs,
    }))
}
