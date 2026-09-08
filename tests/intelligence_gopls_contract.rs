//! Real acceptance coverage for the bounded shared Unix `gopls` profile.

use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_ide::{
    execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLease, AdmissionLimits,
        CommandKind, ControlledCommand, ExecutionProfileCatalog, ExecutionProfileTemplate,
        HostSandboxState, LocalExecutionPolicy, OwnerId, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
    },
    intelligence::gopls::{GoplsProfile, SharedGopls, WorktreeRef},
};
use async_lsp::{
    LanguageServer, MainLoop,
    lsp_types::{
        ClientCapabilities, DidOpenTextDocumentParams, HoverContents, HoverParams,
        InitializeParams, InitializedParams, Position, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, Url, WorkDoneProgressParams, WorkspaceFolder,
    },
    router::Router,
};
use serde_json::json;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Bounds every real daemon, initialize, semantic request, and shutdown operation in this test.
const DEADLINE: Duration = Duration::from_secs(20);

/// Deletes only the uniquely named real-Go fixture root allocated by this test.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Creates two divergent Go worktrees with duplicate package and symbol names.
    fn create() -> io::Result<Self> {
        let root = env::temp_dir().join(format!("agent-ide-gopls-contract-{}", std::process::id()));
        fs::create_dir(&root)?;
        for (name, return_type, value) in [("left", "int", "7"), ("right", "string", "\"seven\"")] {
            let worktree = root.join(name);
            fs::create_dir(&worktree)?;
            fs::write(
                worktree.join("go.mod"),
                format!("module contract.local/{name}\n\ngo 1.25.0\n"),
            )?;
            fs::write(
                worktree.join("main.go"),
                format!(
                    "package main\n\nfunc Shared() {return_type} {{\n\treturn {value}\n}}\n\nfunc main() {{\n\t_ = Shared()\n}}\n"
                ),
            )?;
        }
        Ok(Self { root })
    }

    /// Returns one named divergent worktree root.
    fn worktree(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Fixture {
    /// Removes the test-only fixture root after all owned children have been reaped.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Constructs explicit disabled-host Execution evidence for one controlled test provider child.
fn request(
    root: &Path,
    program: &Path,
    args: Vec<std::ffi::OsString>,
) -> ValidatedExecutionRequest {
    let sandbox = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": root,
    })))
    .expect("test disabled host state is valid");
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("gopls-contract", 1, &sandbox)
            .expect("test profile is valid"),
    ])
    .expect("one test profile is valid");
    let authority = WorkspaceAuthority::from_workspace(
        root.display().to_string(),
        "contract-incarnation",
        root.to_path_buf(),
        1,
    )
    .expect("fixture root is absolute");
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        program.to_path_buf(),
        args,
        root.to_path_buf(),
        Default::default(),
    )
    .expect("gopls command is absolute");
    let policy = LocalExecutionPolicy::new(BTreeSet::from([program.to_path_buf()]), 4096, 0, true)
        .expect("test policy is valid");
    ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("gopls-contract", sandbox)
            .expect("test invocation is valid"),
        authority,
        command,
        &policy,
        &catalog,
    )
    .expect("controlled gopls request is valid")
}

/// Acquires one immediate finite Execution admission lease for an owned child.
fn lease(admission: &mut AdmissionController, owner: &str) -> AdmissionLease {
    match admission.submit(
        OwnerId::new(owner).expect("owner is nonempty"),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        outcome => panic!("expected immediate gopls admission, got {outcome:?}"),
    }
}

/// Runs initialize, open, hover, shutdown, and exit for one distinct forwarder without sharing its pipes.
async fn hover(
    child: &mut agent_ide::execution::OwnedProtocolChild,
    root: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let (mainloop, mut server) = MainLoop::new_client(|_| Router::new(()));
    let root_uri =
        Url::from_file_path(root).map_err(|_| io::Error::other("invalid fixture root URI"))?;
    let file_uri = Url::from_file_path(root.join("main.go"))
        .map_err(|_| io::Error::other("invalid fixture file URI"))?;
    let exchange = async {
        tokio::time::timeout(
            DEADLINE,
            server.initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root_uri,
                    name: "contract".into(),
                }]),
                capabilities: ClientCapabilities::default(),
                ..InitializeParams::default()
            }),
        )
        .await??;
        server.initialized(InitializedParams {})?;
        let text = fs::read_to_string(root.join("main.go"))?;
        server.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "go".into(),
                version: 1,
                text,
            },
        })?;
        let response = tokio::time::timeout(
            DEADLINE,
            server.hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: file_uri },
                    position: Position::new(2, 6),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            }),
        )
        .await??
        .ok_or_else(|| io::Error::other("gopls returned no hover"))?;
        tokio::time::timeout(DEADLINE, server.shutdown(())).await??;
        server.exit(())?;
        Ok::<_, Box<dyn std::error::Error>>(response)
    };
    let loop_run = mainloop.run_buffered(
        (&mut child.stdout).compat(),
        (&mut child.stdin).compat_write(),
    );
    let (response, loop_result) = tokio::join!(exchange, loop_run);
    let response = response?;
    loop_result?;
    let text = match response.contents {
        HoverContents::Scalar(value) => format!("{value:?}"),
        HoverContents::Array(value) => format!("{value:?}"),
        HoverContents::Markup(value) => value.value,
    };
    Ok(text)
}

/// Proves one listener serves two isolated divergent views and release leaves the peer usable.
#[tokio::test]
async fn shared_gopls_isolates_divergent_worktrees_and_detaches_one_view() {
    let fixture = Fixture::create().expect("fixture creation succeeds");
    let gopls = PathBuf::from(env::var("AGENT_IDE_GOPLS").expect("AGENT_IDE_GOPLS is required"));
    let profile = GoplsProfile::new(
        gopls.clone(),
        "v0.23.0".into(),
        "v0.1".into(),
        "contract-config-divergent".into(),
        env::var("AGENT_IDE_GO").expect("AGENT_IDE_GO is required"),
        "disabled-contract".into(),
        fixture.root.join("cache").display().to_string(),
    )
    .expect("profile is valid");
    let socket = fixture.root.join("gopls.sock");
    let listener_command = profile
        .listener_command(
            &WorkspaceAuthority::from_workspace("listener", "one", fixture.worktree("left"), 1)
                .unwrap(),
            &socket,
        )
        .expect("listener command is declared");
    let listener_request = request(
        &fixture.worktree("left"),
        &gopls,
        vec![
            "serve".into(),
            format!("-listen=unix;{}", socket.display()).into(),
        ],
    );
    assert!(format!("{listener_command:?}").contains("-listen=unix;"));
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 3,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut shared = SharedGopls::start(
        &profile,
        &listener_request,
        lease(&mut admission, "listener"),
        Path::new("/unused"),
        4096,
    )
    .expect("Execution starts one listener");
    tokio::time::timeout(DEADLINE, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("listener creates owned Unix socket");
    let left_root = fixture.worktree("left");
    let right_root = fixture.worktree("right");
    let left = WorktreeRef::new("same-name".into(), "left-incarnation".into()).unwrap();
    let right = WorktreeRef::new("same-name".into(), "right-incarnation".into()).unwrap();
    let left_request = request(
        &left_root,
        &gopls,
        vec![format!("-remote=unix;{}", socket.display()).into()],
    );
    let right_request = request(
        &right_root,
        &gopls,
        vec![format!("-remote=unix;{}", socket.display()).into()],
    );
    let left_view = shared
        .open_view(
            left.clone(),
            1,
            &left_request,
            lease(&mut admission, "left"),
            Path::new("/unused"),
            4096,
        )
        .unwrap();
    let right_view = shared
        .open_view(
            right.clone(),
            1,
            &right_request,
            lease(&mut admission, "right"),
            Path::new("/unused"),
            4096,
        )
        .unwrap();
    assert_eq!(shared.process_counts(), (1, 2, 2));
    assert_eq!(shared.begin_request(&left, 1).unwrap(), 1);
    assert_eq!(shared.begin_request(&right, 1).unwrap(), 1);
    let left_lease = left_view.lease();
    let mut left_child = left_view.into_child();
    let left_hover = hover(&mut left_child, &left_root).await.unwrap();
    assert!(
        left_hover.contains("int"),
        "left semantic result: {left_hover}"
    );
    let left_admission = left_child.reap(DEADLINE).await.unwrap().2;
    shared.release_view(&left, left_lease).unwrap();
    admission.release(left_admission).unwrap();
    assert_eq!(shared.process_counts(), (1, 1, 2));
    let right_lease = right_view.lease();
    let mut right_child = right_view.into_child();
    let right_hover = hover(&mut right_child, &right_root).await.unwrap();
    assert!(
        right_hover.contains("string"),
        "right semantic result: {right_hover}"
    );
    let right_admission = right_child.reap(DEADLINE).await.unwrap().2;
    shared.release_view(&right, right_lease).unwrap();
    admission.release(right_admission).unwrap();
    let listener_admission = shared
        .stop(Duration::from_millis(100), DEADLINE)
        .await
        .unwrap();
    admission.release(listener_admission).unwrap();
    println!(
        "one listener, two isolated sessions, divergent int/string results, released left view, right peer survived; forwarders=2"
    );
}
