//! Operator entry point for task-context retrieval, using the same service as ide.prepare.
//! Run with --features jev-pilot --example task_context -- ROOT TASK_FILE [MODELS_JSON].
//! Optional --evidence FILE after MODELS_JSON uses source-bound public-check observations.
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
    let evidence = match args.next() {
        None => None,
        Some(value) if value == "--evidence" => Some(serde_json::from_slice::<
            agent_ide::project::context::evidence::PublicEvidence,
        >(&std::fs::read(
            args.next().ok_or("evidence file required")?,
        )?)?),
        Some(value) if value == "--preview" => return Err("use project_index --estimate".into()),
        _ => return Err("expected --evidence FILE".into()),
    };
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let index = ContextIndex::open_existing(std::path::Path::new(&root))?
        .ok_or("context_index_not_ready")?;
    let service = ContextService::new(models)?;
    let options = PacketOptions::default();
    let documents = DocumentOptions::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let packet = if let Some(evidence) = evidence {
        service
            .prepare_with_evidence(&index, &task, &evidence, &options, &documents, &cancel)
            .await?
    } else {
        service
            .prepare(&index, &task, &options, &documents, &cancel)
            .await?
    };
    println!("{}", serde_json::to_string_pretty(&packet)?);
    Ok(())
}

/// Model support is an explicit build feature; never silently substitutes a different service.
#[cfg(not(feature = "jev-pilot"))]
fn main() {
    eprintln!("task_context requires --features jev-pilot");
    std::process::exit(2);
}
