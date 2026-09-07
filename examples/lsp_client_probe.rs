//! Exercises one owned `gopls serve` process through `async-lsp`.

use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_lsp::{
    LanguageServer, MainLoop,
    lsp_types::{
        ClientCapabilities, DidOpenTextDocumentParams, GotoDefinitionParams,
        GotoDefinitionResponse, HoverContents, HoverParams, InitializeParams, InitializedParams,
        Position, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Url,
        WorkDoneProgressParams, WorkspaceFolder,
    },
    router::Router,
};
use tokio::process::{Child, Command};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const DEADLINE: Duration = Duration::from_secs(20);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);
const SOURCE: &str = "package main\n\nfunc ProbeTarget() int {\n\treturn 7\n}\n\nfunc main() {\n\t_ = ProbeTarget()\n}\n";

/// Removes the uniquely named Go workspace created solely for this process.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Atomically claims an absent directory before creating its module and disposable caches.
    fn create(root: PathBuf) -> Result<Self, Box<dyn Error>> {
        fs::create_dir(&root).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "refusing to use existing fixture directory {}: {error}",
                    root.display()
                ),
            )
        })?;
        let fixture = Self { root };
        fs::create_dir_all(fixture.root.join("go-cache"))?;
        fs::create_dir_all(fixture.root.join("go-mod-cache"))?;
        fs::create_dir_all(fixture.root.join("go-path"))?;
        fs::write(
            fixture.root.join("go.mod"),
            "module probe.local/lsp\n\ngo 1.25.0\n",
        )?;
        fs::write(fixture.root.join("main.go"), SOURCE)?;
        Ok(fixture)
    }
}

impl Drop for Fixture {
    /// Deletes only this probe's uniquely named module and caches.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Stops the `async-lsp` main loop after the client has sent `exit`.
struct Stop;

/// Runs a real owned `gopls` exchange and removes all owned resources on return.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let gopls = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gopls"));
    let fixture_root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(candidate_root);
    if args.next().is_some() {
        return Err("usage: lsp_client_probe [gopls-executable] [fixture-directory]".into());
    }
    let fixture = Fixture::create(fixture_root)?;
    run_probe(&fixture, &gopls).await
}

/// Selects the default candidate directory beneath the process temporary directory.
fn candidate_root() -> PathBuf {
    env::temp_dir().join(format!("agent-ide-lsp-probe-{}", std::process::id()))
}

/// Starts a fresh stdio `gopls`, validates hover and definition, then reaps it.
async fn run_probe(fixture: &Fixture, gopls: &Path) -> Result<(), Box<dyn Error>> {
    let mut child = start_gopls(gopls, &fixture.root)?;
    let stdout = child.stdout.take().ok_or("gopls stdout was not piped")?;
    let stdin = child.stdin.take().ok_or("gopls stdin was not piped")?;
    let callbacks = Arc::new(Mutex::new(BTreeSet::new()));
    let recorded_callbacks = Arc::clone(&callbacks);
    let (mainloop, mut server) = MainLoop::new_client(move |_| {
        let mut router = Router::new(());
        router.unhandled_notification(move |_, notification| {
            if let Ok(mut methods) = recorded_callbacks.lock() {
                methods.insert(notification.method.to_string());
            }
            std::ops::ControlFlow::Continue(())
        });
        router.event(|_, _: Stop| std::ops::ControlFlow::Break(Ok(())));
        router
    });
    let loop_task = tokio::spawn(async move {
        mainloop
            .run_buffered(stdout.compat(), stdin.compat_write())
            .await
    });

    let exchange = async {
        let root_uri =
            Url::from_file_path(&fixture.root).map_err(|_| "workspace path is not a file URL")?;
        let file_uri = Url::from_file_path(fixture.root.join("main.go"))
            .map_err(|_| "source path is not a file URL")?;
        tokio::time::timeout(
            DEADLINE,
            server.initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root_uri,
                    name: "probe".into(),
                }]),
                capabilities: ClientCapabilities::default(),
                ..InitializeParams::default()
            }),
        )
        .await??;
        server.initialized(InitializedParams {})?;
        server.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "go".into(),
                version: 1,
                text: SOURCE.into(),
            },
        })?;
        let position = Position::new(7, 6);
        let hover = tokio::time::timeout(
            DEADLINE,
            server.hover(HoverParams {
                text_document_position_params: document_position(&file_uri, position),
                work_done_progress_params: WorkDoneProgressParams::default(),
            }),
        )
        .await??
        .ok_or("gopls returned no hover")?;
        if !hover_text(hover.contents).contains("ProbeTarget") {
            return Err("hover did not name ProbeTarget".into());
        }
        let definition = tokio::time::timeout(
            DEADLINE,
            server.definition(GotoDefinitionParams {
                text_document_position_params: document_position(&file_uri, position),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: Default::default(),
            }),
        )
        .await??
        .ok_or("gopls returned no definition")?;
        if !definition_points_to_target(definition, &file_uri) {
            return Err("definition did not point to ProbeTarget".into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;

    let shutdown = tokio::time::timeout(SHUTDOWN_DEADLINE, server.shutdown(())).await;
    let exit = server.exit(());
    let stop = server.emit(Stop);
    drop(server);
    let loop_result = tokio::time::timeout(SHUTDOWN_DEADLINE, loop_task).await;
    let forced_kill = reap(&mut child).await?;
    exchange?;
    shutdown??;
    exit?;
    stop?;
    loop_result???;
    let callback_summary = callbacks
        .lock()
        .map_err(|_| "callback recorder lock was poisoned")?
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "async-lsp gopls hover+definition probe passed; forced_kill={forced_kill}; callbacks: {callback_summary}"
    );
    Ok(())
}

/// Starts only the probe's supplied stdio `gopls serve` process with isolated Go caches.
fn start_gopls(gopls: &Path, root: &Path) -> Result<Child, Box<dyn Error>> {
    Ok(Command::new(gopls)
        .arg("serve")
        .current_dir(root)
        .env("GOCACHE", root.join("go-cache"))
        .env("GOMODCACHE", root.join("go-mod-cache"))
        .env("GOPATH", root.join("go-path"))
        .env("GOPROXY", "off")
        .env("GOTOOLCHAIN", "local")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?)
}

/// Builds the shared document-and-position parameters for semantic requests.
fn document_position(uri: &Url, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        position,
    }
}

/// Converts all LSP hover encodings into text for the one symbol-presence check.
fn hover_text(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked) => format!("{marked:?}"),
        HoverContents::Array(marked) => format!("{marked:?}"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Checks that every valid definition response form resolves to the declaration line.
fn definition_points_to_target(definition: GotoDefinitionResponse, uri: &Url) -> bool {
    match definition {
        GotoDefinitionResponse::Scalar(location) => {
            location.uri == *uri && location.range.start.line == 2
        }
        GotoDefinitionResponse::Array(locations) => locations
            .iter()
            .any(|location| location.uri == *uri && location.range.start.line == 2),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .any(|link| link.target_uri == *uri && link.target_selection_range.start.line == 2),
    }
}

/// Reaps the owned child and reports whether it required a deadline-triggered kill.
async fn reap(child: &mut Child) -> Result<bool, Box<dyn Error>> {
    match tokio::time::timeout(SHUTDOWN_DEADLINE, child.wait()).await {
        Ok(status) => {
            let status = status?;
            if !status.success() {
                return Err(
                    io::Error::other(format!("gopls exited unsuccessfully: {status}")).into(),
                );
            }
            Ok(false)
        }
        Err(_) => {
            child.kill().await?;
            tokio::time::timeout(SHUTDOWN_DEADLINE, child.wait()).await??;
            Ok(true)
        }
    }
}

/// Regression checks for fixture ownership without starting a language server.
#[cfg(test)]
mod tests {
    use super::*;

    /// Refuses an existing candidate directory without deleting its sentinel file.
    #[test]
    fn existing_fixture_candidate_is_preserved() -> Result<(), Box<dyn Error>> {
        let root = env::temp_dir().join(format!(
            "agent-ide-lsp-probe-sentinel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&root)?;
        let sentinel = root.join("sentinel");
        fs::write(&sentinel, "preserve")?;
        let refused = Fixture::create(root.clone());
        let contents = fs::read_to_string(&sentinel)?;
        fs::remove_dir_all(root)?;
        assert!(refused.is_err());
        assert_eq!(contents, "preserve");
        Ok(())
    }
}
