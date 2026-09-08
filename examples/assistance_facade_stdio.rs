//! Test-only stdio entrypoint for the static five-tool Assistance MCP facade.

use std::{error::Error, path::PathBuf};

use agent_ide::assistance::facade::StdioFacade;
use rmcp::{serve_server, transport::io::stdio};

/// Serves static MCP discovery and bounded unavailable responses for one supplied runtime directory.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let runtime_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: assistance_facade_stdio <runtime-dir>")?;
    let service = serve_server(StdioFacade::new(runtime_dir), stdio()).await?;
    service.waiting().await?;
    Ok(())
}
