//! Prints the `ide.tree` project map of a directory without starting the IDE daemon.
//!
//! Run: cargo run --example project_tree -- ROOT [PATH] [DEPTH]

use std::path::Path;
use std::time::Instant;

/// Registers the bundled languages and prints the map with its size and time.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    agent_ide::languages::install();
    let mut args = std::env::args().skip(1);
    let root = std::fs::canonicalize(
        args.next()
            .ok_or("usage: project_tree ROOT [PATH] [DEPTH]")?,
    )?;
    let path = args.next().unwrap_or_default();
    let depth = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(2);
    let started = Instant::now();
    let text = agent_ide::lang::render::project_tree(&root, Path::new(&path), depth).await?;
    print!("{text}");
    eprintln!("--- {} bytes in {:?}", text.len(), started.elapsed());
    Ok(())
}
