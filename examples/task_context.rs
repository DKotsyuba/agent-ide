//! Operator entry point for task-context retrieval, using the same service as ide.prepare.
//! Run with --features jev-pilot --example task_context -- ROOT TASK_FILE [MODELS_JSON].
//! Build/estimate indexes separately with the project_index example.
/// Queries an already published index without creating it or embedding source documents.
#[cfg(feature = "jev-pilot")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use agent_ide::project::context::{
        ContextIndex, DocumentOptions, PacketOptions,
        service::{ContextService, ModelConfig},
    };
    agent_ide::languages::install();
    let mut args = std::env::args_os().skip(1);
    let root = args.next().ok_or("expected ROOT TASK_FILE [MODELS_JSON]")?;
    let task = std::fs::read_to_string(args.next().ok_or("task file required")?)?;
    let models = match args.next() {
        Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
        None => ModelConfig {
            embedding: None,
            jev: None,
            max_new_calls: 0,
            include_threshold: 0.5,
        },
    };
    let preview = match args.next() {
        None => false,
        Some(value) if value == "--preview" => true,
        _ => return Err("expected --preview".into()),
    };
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    if preview {
        return Err("use project_index ROOT MODELS_JSON --estimate for indexing preflight".into());
    }
    let index = ContextIndex::open_existing(std::path::Path::new(&root))?
        .ok_or("context_index_not_ready")?;
    let service = ContextService::new(models)?;
    let packet = service
        .prepare(
            &index,
            &task,
            &PacketOptions::default(),
            &DocumentOptions::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&packet)?);
    Ok(())
}

/// Model support is an explicit build feature; never silently substitutes a different service.
#[cfg(not(feature = "jev-pilot"))]
fn main() {
    eprintln!("task_context requires --features jev-pilot");
    std::process::exit(2);
}
