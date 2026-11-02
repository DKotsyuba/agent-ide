//! Explicit operator index preflight/build. Example: project_index ROOT MODELS_JSON --estimate [USD_PER_MILLION [MAX_INPUT_BYTES]].
//! --reuse-from SOURCE_ROOT copies matching cached vectors offline; --index then publishes readiness.
//! --index is the explicit write/network action; --estimate never creates clients or reads credentials.
#[cfg(feature = "jev-pilot")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use agent_ide::project::{
        context::{
            ContextIndex, DocumentOptions,
            indexer::{IndexBudget, estimate_indexing},
            service::{ContextService, ModelConfig},
        },
        structure::StructureOptions,
    };
    agent_ide::languages::install();
    let mut args = std::env::args_os().skip(1);
    let root = args
        .next()
        .ok_or("expected ROOT MODELS_JSON --estimate|--index [USD_PER_MILLION]")?;
    let mut config: ModelConfig =
        serde_json::from_slice(&std::fs::read(args.next().ok_or("expected model config")?)?)?;
    let mode = args
        .next()
        .ok_or("explicit --estimate or --index required")?;
    if mode == "--reuse-from" {
        let source_root = args.next().ok_or("expected source project root")?;
        if args.next().is_some() {
            return Err("unexpected vector reuse argument".into());
        }
        let embedding = config
            .embedding
            .as_ref()
            .ok_or("embedding config required")?;
        let space = embedding.space_id()?;
        let source = ContextIndex::open_existing(std::path::Path::new(&source_root))?
            .ok_or("source index does not exist")?;
        let mut destination = ContextIndex::open(std::path::Path::new(&root))?;
        let scope = StructureOptions {
            max_depth: 64,
            max_nodes: 100000,
            ..Default::default()
        };
        let docs = DocumentOptions {
            max_input_bytes: DocumentOptions::default()
                .max_input_bytes
                .min(embedding.max_input_bytes),
            ..Default::default()
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        estimate_indexing(&mut destination, &scope, &docs, Some(embedding), &cancel)?;
        let copied = destination.reuse_vectors(&source, &space, embedding.dimensions, &docs)?;
        let remaining =
            estimate_indexing(&mut destination, &scope, &docs, Some(embedding), &cancel)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "vectors_reused": copied, "remaining": remaining,
                "network_requests": 0, "credentials_read": false,
                "publication_required": true
            }))?
        );
        return Ok(());
    }
    if mode == "--release-unresolved" {
        if args.next().as_deref()
            != Some(std::ffi::OsStr::new("--accept-possible-duplicate-charge"))
            || args.next().is_some()
        {
            return Err(
                "requires --accept-possible-duplicate-charge; stop IDE before retry maintenance"
                    .into(),
            );
        }
        let mut index = ContextIndex::open(std::path::Path::new(&root))?;
        let embedding = config
            .embedding
            .as_ref()
            .ok_or("embedding config required")?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &agent_ide::project::context::indexer::release_unresolved(
                    &mut index, embedding, true
                )?
            )?
        );
        return Ok(());
    }
    if mode != "--estimate" && mode != "--index" {
        return Err("expected --estimate or --index".into());
    }
    let rate = args
        .next()
        .map(|v| v.to_string_lossy().parse::<f64>())
        .transpose()?;
    let byte_budget = args
        .next()
        .map(|v| v.to_string_lossy().parse::<usize>())
        .transpose()?;
    if rate.is_some_and(|v| !v.is_finite() || v < 0.0)
        || byte_budget == Some(0)
        || args.next().is_some()
    {
        return Err("invalid tariff or extra arguments".into());
    }
    config.jev = None;
    let mut index = ContextIndex::open(std::path::Path::new(&root))?;
    let scope = StructureOptions {
        max_depth: 64,
        max_nodes: 100000,
        ..Default::default()
    };
    let docs = DocumentOptions::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let estimate = estimate_indexing(
        &mut index,
        &scope,
        &docs,
        config.embedding.as_ref(),
        &cancel,
    )?;
    if mode == "--estimate" {
        let output = serde_json::json!({"estimate":estimate,"usd_per_million_input_tokens":rate,"estimated_cost_usd":rate.map(|r|r*estimate.estimated_input_tokens as f64/1_000_000.0),"network_requests":0,"token_count_is_estimated":true,"credentials_read":false});
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let budget = IndexBudget {
            max_new_calls: config.max_new_calls,
            max_input_bytes: byte_budget.unwrap_or(docs.max_document_bytes),
        };
        if estimate.input_bytes > budget.max_input_bytes
            || estimate.estimated_requests > budget.max_new_calls
        {
            return Err("preflight exceeds explicit indexing budget; no requests sent".into());
        }
        let service = ContextService::new(config)?;
        let report = service
            .index_prepared(&mut index, &docs, &budget, &cancel, estimate)
            .await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.outcome != "ok" || report.fragments_pending > 0 {
            return Err("index incomplete; inspect the saved JSON outcome".into());
        }
    }
    Ok(())
}
/// Refuses indexing when the model feature is absent; never silently contacts another service.
#[cfg(not(feature = "jev-pilot"))]
fn main() {
    eprintln!("project_index requires --features jev-pilot");
    std::process::exit(2);
}
