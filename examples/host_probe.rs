use std::{env, fs::OpenOptions, io::{self, Read, Write}, path::PathBuf};
use serde_json::Value;

const MAX_INPUT: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_FIELDS: usize = 256;
const MAX_LOG: u64 = 256 * 1024;

/// Runs the bounded diagnostic recorder in MCP or hook mode.
fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("hook") => hook(args.next().map(PathBuf::from), args.next().map(PathBuf::from)),
        Some("mcp") => mcp(),
        _ => { eprintln!("usage: host_probe <mcp|hook> [log] [key]"); Ok(()) }
    }
}

/// Records one bounded hook payload while never making recording fatal.
fn hook(log: Option<PathBuf>, key: Option<PathBuf>) -> io::Result<()> {
    let mut input = Vec::new();
    let bounded = io::stdin().take((MAX_INPUT + 1) as u64).read_to_end(&mut input).is_ok() && input.len() <= MAX_INPUT;
    let record = if bounded { serde_json::from_slice::<Value>(&input).ok().and_then(|v| key.as_deref().and_then(|p| std::fs::read(p).ok()).filter(|k| k.len() == 32).map(|k| observe(&v, &k))) } else { None };
    write_record(log.as_deref(), record.as_deref().unwrap_or("marker=malformed_or_oversized"))
}

/// Produces field shape and keyed scalar fingerprints without raw values.
fn observe(v: &Value, salt: &[u8]) -> String {
    let mut out = Vec::new(); walk(v, "", 0, &salt, &mut out);
    out.join(";")
}

fn walk(v: &Value, path: &str, depth: usize, key: &[u8], out: &mut Vec<String>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FIELDS { return }
    let typ = match v { Value::Null => "null", Value::Bool(_) => "bool", Value::Number(_) => "number", Value::String(_) => "string", Value::Array(_) => "array", Value::Object(_) => "object" };
    let fp = match v { Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => { let mut h = blake3::Hasher::new_keyed(&key32(key)); h.update(v.to_string().as_bytes()); format!(" fp={}", h.finalize().to_hex()) }, _ => String::new() };
    out.push(format!("{}:{}{}", if path.is_empty() { "$" } else { path }, typ, fp));
    match v { Value::Object(m) => for (k, x) in m { walk(x, &format!("{}.{}", path, k), depth + 1, key, out) }, Value::Array(a) => for (i, x) in a.iter().enumerate() { walk(x, &format!("{}[{}]", path, i), depth + 1, key, out) }, _ => {} }
}

fn key32(key: &[u8]) -> [u8; 32] { let mut k = [0; 32]; k.copy_from_slice(&key[..32]); k }

fn write_record(path: Option<&std::path::Path>, record: &str) -> io::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let mut f = match OpenOptions::new().create(true).append(true).open(path) { Ok(f) => f, Err(_) => return Ok(()) };
    if f.metadata().map(|m| m.len() >= MAX_LOG).unwrap_or(true) { return Ok(()) }
    let line = format!("{}\n", record.chars().take(4096).collect::<String>());
    let _ = f.write_all(line.as_bytes()); Ok(())
}

#[tokio::main]
async fn mcp() -> io::Result<()> {
    use rmcp::{handler::server::tool::ToolRouter, model::*, ServerHandler, ServiceExt};
    #[derive(Clone)] struct Probe { router: ToolRouter<Self> }
    #[rmcp::tool_router]
    impl Probe { #[rmcp::tool(description = "Diagnostic host metadata probe; authority binding remains unproven.")] async fn probe_observe(&self) -> String { "binding_unproven".into() } }
    #[rmcp::tool_handler]
    impl ServerHandler for Probe { fn get_info(&self) -> ServerInfo { ServerInfo::default().with_instructions("diagnostic probe") } }
    let p = Probe { router: Probe::tool_router() };
    p.serve(rmcp::transport::io::stdio()).await.map_err(|e| io::Error::other(e.to_string()))?.waiting().await.map_err(|e| io::Error::other(e.to_string()))?; Ok(())
}
